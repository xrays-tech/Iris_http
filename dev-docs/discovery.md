# Iris HTTP 反向代理 — OpenStack 自动模式（实例发现）设计

> 依据：`propose.md`（解析 `instance_name`、查 floating/fixed IP、缓存 1 天、参考
> `xrays-tech/Iris_ssh`）。本篇为 `design.md` §4.6 与 `config.md` §4 的细化。
> 发现逻辑参数与行为对齐 Iris_ssh 已验证实现（2026-10 核对）。

## 1. 职责与不变量

- 输入：`instance` 标识（名称或 UUID）+ `port`。来源有二：openstack 通配站点由 Host `{instance}-{port}.base_domain` 解析得出（`port` 已通过 ACL，见 `firewall.md`）；forward 站点由后端条目 `instance` + 显式 `port` 给出（无 ACL 门禁）。
- 输出：上游 `{ip}:{port}`，其中 IP 为 floating（优先）或 fixed。
- 不变量：
  1. 同一实例并发查询**只穿透一次** OpenStack（单飞）。
  2. 正向结果缓存 TTL 默认 **1 天**（`cache_ttl_secs`）。
  3. 实例不存在（Nova 404 或名称无精确匹配）走**负缓存**，短 TTL。
  4. 控制面故障时**fail-open**：用 last-known-good 结果继续服务，而不是直接 5xx。
  5. 单实例持续故障触发**熔断**，避免击穿控制面。

## 2. 解析流程

```
request_filter: host → (instance_name, port)            [config.md §8]
ACL: 端口/IP 白黑名单                                    [firewall.md]
       │
       ▼
resolve(instance_name):
  1. cache.get(key) ──命中──► 回 TTL 内结果（frozen 才进缓存）
       │ miss
       │
  2. single-flight：同 key 并发折叠为一个 in-flight
       │
  3. 熔断器：open → 直接走 last-known-good（fail-open）并累计指标
       │ close/half-open
       ▼
  4. 认证：有效 cache token？否 → POST /v3/auth/tokens（§3）
  5. Nova 查询（§4）：known uuid 直查 / 否则 name 列表定位 + show-by-id 取详情
  6. 地址选择（§5）：floating 优先 → fixed 回退
  7. 更新缓存与 last-known-good（§6）
```

## 3. 认证（Keystone v3）

端点：`auth.api_url`（`/v3`，如 `https://controller:5000/v3`）。

| 方式 | 请求体要点 |
|---|---|
| `password` | `{"auth": {"identity": {"methods":["password"], "password": {"user": {"name":..., "domain": {"name":...}, "password":...}}}, "scope": {"project": {"name":..., "domain": {"name":...}}}}}` |
| `application_credential` | `{"auth": {"identity": {"methods":["application_credential"], "application_credential": {"id":..., "secret":...}}}}`（token 已限定项目，无需 scope） |

- 成功：HTTP 201，`X-Subject-Token` 响应头即 token；响应体含过期时间与**服务目录**。
- 服务目录：选 `type=compute`（Nova）、匹配 `nova.region`（未配置则取第一个）的 public URL；站点配置了 `nova.api_url` 则优先用它，跳过目录发现（同 Iris_ssh `nova_base_from_catalog` 语义）。

**Token 生命周期**：

- `CachedToken { token, expires_at, base_url }` 进进程内缓存。
- 过期前留缓冲（如提前 60s）视为将过期；请求 401 时**刷新一次并重试该请求**，仍失败才报错。
- token 刷新与 Nova 查询共用同一把单飞通道，避免并发刷新风暴。

## 4. Nova 查询

端点：`{nova_api_url}`（Nova API 2.1，缺省带 microversion 头）。

| 场景 | 方法/路径 | 查询参数 | 判定 |
|---|---|---|---|
| 按 UUID（identifier 形如 `8-4-4-4-12`） | `GET /servers/{uuid}` | — | 200 → 单实例信封 `{"server":{...}}`；404 → 不存在 |
| 按名称（其余情况） | `GET /servers` 然后 `GET /servers/{id}` | list: `name={instance_name}&all_tenants=true`；detail: — | 200 → 列表内**精确名称匹配**（`exact_name_match`）定位 id，再对 id 走 show-by-id 取 **详情信封** 用于地址选择 |

**UUID 优先级规则（重要）**：只要 identifier 是 `8-4-4-4-12` 十六进制字符串（大小写不敏感，长度与分隔符严格），就**一律**走 `GET /servers/{uuid}` show-by-id——`is_instance_uuid` 在 `query` 分发前判别。

- 推论：一个**真实名称为 UUID 形态**的实例**无法按名称解析**；同一标识总被当作 UUID。这是刻意的确定性规则，避免"同名 UUID 字符串"歧义。
- 名称路径的具体过程（**两次 Nova 调用**）：`query_servers` 取回列表 → `exact_name_match` 逐字节相等定位实例 id（列表为空或无精确匹配 → 404，负缓存）→ 对该 id 调 `query_server_by_id` 取详情，用详情中的 `addresses` 做地址选择。**真实 Nova 的列表端点不返回 `addresses`（字段为 null，admin 亦然）**，因此不能直接在列表结果上选地址。两次调用同属一次 `fetch`，对 `resolve(instance)` 仍是一次单飞/一次缓存条目（冷路径 2 次 Nova 调用，热路径 0 次）。
- 列表定位成功但详情 404 → 404（实例已不存在）；详情 403/5xx/超时/解码失败 → 502（瞬时错误，§7）；详情无地址 → 502（`NoAddress`）。
- 两条路径复用同一套标准头、状态映射（401/403/404/其它非 2xx）与 401 单次刷新重试。
- 必需头：`X-Auth-Token: {token}`、`Accept: application/json`、`OpenStack-API-Version: compute 2.1`（与 Iris_ssh mock 契约一致）。
- `all_tenants=true` 配合 admin/有权限的认证主体，实现"全局查找实例"（propose 要求）。若认证主体无跨项目权限，Nova 可能返回 403 → 记录清晰错误事件，按"发现服务错误"处理（§7）。show-by-id 路径不带 `all_tenants`。

## 5. 地址选择（floating / fixed）

Nova 响应结构（`server.addresses`）：

```json
"addresses": {
  "net-a": [
    {"version": 4, "addr": "192.168.1.10",          "OS-EXT-IPS:type": "fixed"},
    {"version": 4, "addr": "203.0.113.10",          "OS-EXT-IPS:type": "floating"},
    {"version": 6, "addr": "fd00::10",              "OS-EXT-IPS:type": "fixed"}
  ]
}
```

选择算法（确定性、可测试）：

1. 遍历所有网络的所有地址条目。
2. 收集 `version == 4` 的地址；无 IPv4 才允许 IPv6。
3. `type == floating` 优先于 `fixed`。
4. 最终顺序 `[floating_primary, fixed_fallback]`：**上游默认用第一个 floating IPv4**；该地址不可达时（连接被拒/超时），`fail_to_connect` 重试中切换到 fixed 再试一次（避免因浮动 IP 偶发漂移导致不可用）。
5. 任一排序维度都不存在时如实失败（见 `discovery.md` §3.4/附录的 floating-only 边界：floating-only 实例正常；仅 IPv6 实例也可用，但默认不做 NAT64 转换，直接代理 IPv6 地址）。

结果对象：`ResolvedInstance { floating: Option<IpAddr>, fixed: Option<IpAddr> }`。

## 6. 缓存与容错

### 6.1 缓存表（moka，进程内）

| 项 | key | TTL | 说明 |
|---|---|---|---|
| 正向结果 | `instance_name` | `cache_ttl_secs`（默认 86400 ≈ 1 天） | 存 `ResolvedInstance` |
| 负缓存（不存在） | `instance_name` | 5s | 防抖动；用 `ResolvedInstance` 的 empty 变体表示 |
| 瞬时错误（如 5xx/超时） | `instance_name` | 1s | 快速重试窗口，避免雪崩 |

- TTL 抖动（jitter，±10% 量级）打散高并发冷启动时的同时过期。
- 缓存内容**冻结后引用**（共享不可变 `Arc`），不为每条响应复制。

### 6.2 单飞（single-flight）

- 同一 `instance_name` 上同时只有一条 Nova 查询在途；其余等待者共享同一结果（含失败结果）。
- 实现：`Mutex<HashMap<String, Arc<tokio::sync::Notify + 结果槽>>>`，查询完成统一 `notify`。防止缓存击穿。

### 6.3 熔断（circuit breaker）

- 会话维度连续失败（Nova 网络错误/5xx/超时）达阈值（默认 **5**，对齐 Iris_ssh）→ 熔断 open。
- open 期间不再发 Nova 请求，直接 fail-open；冷却期（默认 `timeout_secs * 3`）后 half-open 放行少量探测，成功即恢复 close。

### 6.4 fail-open（last-known-good）

- 维护 `ArcSwap<HashMap<instance_name, ResolvedInstance>>`：每次成功解析写一份。
- 熔断 open 或 Nova 故障时：优先用内存 last-known-good；无记录才报"发现服务错误"。
- 策略取舍：**可用性优先于精确性**（实例 IP 变更最坏缓存 1 天，可接受），符合代理场景。

### 6.5 并发安全

- 全部共享状态（缓存、token、last-known-good、熔断器）置于 `Arc<...>` 内；写路径加锁，读路径尽可能无锁/`ArcSwap`。
- Nova 客户端用 `reqwest` 连接池（或 Pingora 自带 HTTP client `pingora::http::Client`，避免在代理主线程外引入独立运行时）；`discovery` 的 async 调用在 `upstream_peer` 之前完成于 `request_filter` 阶段，避免阻塞连接池回收语义——详见 §8 时序。

### 6.6 环境共享（resolver / 缓存按环境键）

- 解析器**每环境一个**，键为环境名（`<config-dir>/openstack/<env-name>.yaml` 的文件 stem）；`build_env_resolvers(config)` 为一个 `OpenStackEnv` 建一个 `Arc<dyn Resolver>`。
- 同一环境的所有消费者共享同一 resolver 实例，因而共享同一份**缓存 / 单飞 / 熔断 / last-known-good**；例如一个 openstack 通配站点与一个 forward instance 池引用同一 `prod` 环境时，对同名实例只穿透一次 Nova。
- 全部环境的 resolver 共用一个 `HttpClient`，Keystone/Nova 连接跨环境池化（`discovery.md` §6.5 的连接池语义）。
- reload 时仅当该环境的 `OpenStackConfig` 相等才复用同一 `Arc<dyn Resolver>`（缓存保留）；否则重建，见 `reload.md` §3。

## 7. 错误语义（对外 HTTP 状态）

| 场景 | 客户端状态 | 说明 |
|---|---|---|
| 实例不存在 / 名称无精确匹配 / 列表命中但 show-by-id 详情 404 | 404 | 负缓存；语义为"无此路由目标" |
| Nova 401 且刷新后仍失败 | 502 | 发现服务错误，日志含 auth 上下文（不打码的字段如 auth_url） |
| 实例存在但无任何可用地址 | 502 | 无 floating 且无 fixed |
| 熔断 open 且无 last-known-good | 502 | 带 `Retry-After` 提示（可选） |
| ACL 拒绝 | 403 | 见 `firewall.md` |
| 上游连接失败（有 IP） | 502（重试后） | `fail_to_connect` 路径 |

## 8. 性能与时序

- 每请求成本：路由（精确哈希）→ ACL（O(#cidr)）→ 缓存查找（moka `O(1)`）→ 命中即完成，无任何网络开销。
- 冷路径成本：单飞 + Nova 查询（按名称 = list 定位 + show-by-id 详情两次调用，按 UUID = 一次 show-by-id；均受 `timeout_secs` 上限约束），期间同实例并发请求**等待同一结果**而不是各自超时。
- 建议上游连接复用 Pingora 连接池（`HttpPeer` 默认行为），同一 `{ip}:{port}` 高频复用 TLS/HTTP 连接。

## 9. 测试策略

- 单元：`parse_instance_host`、`exact_name_match`、`select_address`（表驱动：floating-only、fixed-only、IPv6-only、多网卡）、缓存 TTL/负缓存、单飞并发折叠、熔断状态机、fail-open 回退。
- 集成（mock Nova，契约同 Iris_ssh `scripts/mock_openstack.py`）：
  - token 首次获取 → 缓存 → 后续复用（断言只发一次 `/v3/auth/tokens`）
  - token 过期后 401 → 单次刷新并重试成功
  - `GET /servers?name=` 空结果 → 负缓存 + 404
  - floating-only / 跨项目（`all_tenants`）
  - 控制面宕机 → 熔断 → last-known-good 继续 200；无记录 → 502
- 全部通过 `cargo test`；端到端用 `curl -H "Host: INS001-80.rent.do.top"` 冒烟。
