# Iris HTTP 反向代理 — 防火墙规则与访问控制设计

> 依据：`propose.md`（openstack 站点可配置防火墙设置目录；目录下多个 `INSTANCE.role`
> 规则文件，配置开放本地端口，支持白名单/黑名单 IP）。总体架构见 `design.md`，
> 与 `config.md` §4、`discovery.md` 配合。

## 1. 定位

防火墙只作用于 **openstack 自动模式** 站点，在代理转发前对每请求做三层判定：

1. **目标端口**是否在该实例的 role 开放列表内。
2. **客户端 IP** 是否命中黑名单。
3. **客户端 IP** 是否命中白名单（启用时）。

判定在 `request_filter` 阶段完成（早于 DNS/上游建立），拒绝不产生任何上游流量。

## 2. 规则目录与文件

- 目录路径由站点 `firewall_dir` 指定（`config.md` §4）。
- 目录内每个文件命名为 `<instance_name>.role`（如 `INS001.role`、`my-app1.role`，文件名即实例名，大小写敏感）。
- 非 `.role` 后缀文件忽略。

`INS001.role` 示例：

```yaml
# 端口开放列表：缺省为空 = 不开放任何端口（default deny）
allowed_ports:
  - 80
  - 8080

# 客户端 IP 白名单（CIDR）。为空列表 = 不启用白名单（即非黑名单即放行）
whitelist:
  - 10.0.0.0/8
  - 172.16.0.0/12

# 客户端 IP 黑名单（CIDR）。命中即拒绝，优先级最高
blacklist:
  - 203.0.113.0/24
```

字段规格：

| 字段 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `allowed_ports` | `int[]` | `[]` | 允许代理的目标端口，`1..=65535`，去重有序 |
| `whitelist` | `string[]`（CIDR 或 IP） | `[]` | 空 = 不启用白名单 |
| `blacklist` | `string[]`（CIDR 或 IP） | `[]` | 命中即拒；IP 格式(`203.0.113.1/32`)与 CIDR 均支持 |

- 每个 role 文件独立解析、独立校验；单文件解析失败 → **整份配置加载失败**（启动与 reload 均整体失败，保持旧配置），不做部分生效。
- 支持热更新的角色保留：**新规则只随 `reload` 生效**（`firewall_dir` 变化或文件变化触发 `config.md` §9 的重载），不提供独立热更通道，保持单一重载语义。

## 3. 判定顺序（deny by default）

```
开始
 │
 ├─ 1. 实例 role 文件是否存在？
 │     ├─ 不存在 → 403（未授权实例；见 §4 无 role 语义）
 │     └─ 存在 ↓
 ├─ 2. port ∈ allowed_ports ?
 │     ├─ 否 → 403（端口未开放；记录尝试端口，防探测噪声需要限速/日志分级）
 │     └─ 是 ↓
 ├─ 3. client_ip ∈ blacklist ?          （逐条 CIDR 匹配，任中即拒）
 │     ├─ 是 → 403
 │     └─ 否 ↓
 ├─ 4. whitelist 非空？
 │     ├─ 是 → client_ip ∈ whitelist ?
 │     │        ├─ 否 → 403
 │     │        └─ 是 ↓
 │     └─ 否（未启用） ↓
 └─ 5. 通过 → 进入实例发现（discovery.md）
```

- 客户端 IP 取连接对端地址（Pingora `Session::client_addr()`），端口即 `upstream_peer` 要用的目标端口。
- **403（Forbidden）统一用于 ACL 拒绝**（对探测者隐藏"实例是否存在"的差别：未授权实例与端口未开放均 403；实例存在性差异由 `discovery.md` 的 404 语义单独表达——两条路径职责分离：先 ACL 后发现）。

## 4. 无 role / firewall_dir 的语义

`firewall_dir` 是 openstack 站点的**必填**字段（P16）：未配置，或配置的目录不存在/非目录，
均为配置错误（`ConfigError::Validate`，reload 时保持旧配置）。不存在"跳过 ACL"的运行态。

| 情况 | 行为 | 理由 |
|---|---|---|
| 未配置 `firewall_dir` | **加载失败**（`ConfigError::Validate`） | 无防火墙的 openstack 通配站点 = 未认证的任意端口代理（实例名与端口均来自 Host），必须默认拒绝 |
| `firewall_dir` 不是已存在的目录 | **加载失败**（`ConfigError::Validate`） | 同上；路径拼写错误不应静默地关闭 ACL |
| 某实例无 role 文件 | **403**（default deny） | 安全最佳实践：开放列表是显式白名单 |
| role 存在但 `allowed_ports` 为空 | 任何端口均 403 | 空开放列表 = 不开放 |

> `forward` 站点不受影响：其实例后端由 operator 在配置里钉死实例名与端口，Host 不可控，无 SSRF。

## 5. 实现

- 解析：role 文件用 `serde_yaml`；CIDR 用 `ipnet` crate 解析（非法 CIDR → 加载失败）。
- 匹配：`IpNet` 列表按版本（v4/v6）分组，逐条 `contains(&client_ip)`；条目数大（≥ 阈值，如 1000）时用前缀树（`ipnet`/自建 trie）加速——当前版本先线性匹配，条目上限 10000，超限拒绝加载。
- 数据结构随 `reload` 整体重建（`Arc<FirewallTable>`，`HashMap<instance_name, Arc<RoleRules>>` 放进路由快照），请求级只读。
- 校验每个 IP 条目同时支持纯 IP（自动补 `/32`/`/128`）与 CIDR 写法。

## 6. 安全与防探测

- 403 响应体统一、不携带实例/端口细节（仅 `Forbidden`），防枚举。
- ACL 拒绝/端口未开放/无 role 三类拒绝事件分别打结构化日志 + 指标（`firewall_denied_total{reason=...}`），供异常行为告警。
- 端口未开放仍返回 403 而非更聚焦的语义，避免给探测者反馈"端口存在与否"。若运营需要，可通过指标区分。
- 日志中客户端 IP 为原始对端 IP，供后续统一限速/封禁（不在本版本实现限速，属明显追加功能，预留指标点位）。

## 7. 测试策略

- 单元（纯函数）：判定顺序表驱动（含：黑名单优先于白名单、白名单启用时未命中拒绝、空白名单=放行、无 role 拒绝、端口未开放拒绝、IPv4/IPv6 与 CIDR 边界、非法 CIDR 拒绝加载）。
- 集成（mock 上游 + mock Nova）：`curl -H "Host: INS001-80.rent.do.top"` 在 role 开放时 200、未开放端口 403、黑名单来源 403。
- 端到端：真实 role 目录 + `reload` 后新规则生效、旧规则在途连接不受影响。
