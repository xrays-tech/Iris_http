# Iris HTTP 反向代理 — 配置体系设计

> 依据：`propose.md`（站点两种模式、权重、负载均衡、防火墙目录）。总体架构见 `design.md`。

## 1. 配置目录组织

程序启动时以 `--config-dir`（默认 `./config` 或 `/etc/iris`）定位配置：

```
config/
├─ sites/                # 站点配置目录：每站点一个 YAML 文件（文件名无语义，仅加载顺序无关）
│  ├─ 001-forward-example.yaml
│  └─ 002-openstack-rent.yaml
└─ firewall/             # 防火墙规则目录（被各 openstack 站点以 firewall_dir 引用，可不在此目录内）
   ├─ INS001.role
   └─ INS002.role
```

- `sites/` 下所有 `*.yaml` 一起构成 `Config`；重复/冲突在加载时整体校验（见 §6）。
- `firewall` 目录路径在**每个 openstack 站点内**配置（`firewall_dir`），因此不同站点可指向不同目录。
- 全部加载逻辑是纯函数 `load_config(sites_dir: &Path) -> Result<Config>`，供启动与 `reload` 复用（见 `reload.md`）。

## 2. 通用字段

每个站点文件含两种模式（`mode` 字段区分）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | 是 | 站点标识，仅用于日志/指标/错误定位，必须唯一 |
| `mode` | enum | 是 | `forward` \| `openstack` |
| `http` | 布尔 | 否 | 是否响应 HTTP（80 端口），默认 `true` |
| `https` | 布尔 | 是 | 是否启用 HTTPS（443 端口） |
| `listen.http_port` / `listen.https_port` | int | 否 | 全局默认 80/443；允许测试环境改端口（生产部署保持默认） |

`https: true` 时必须提供 `cert_file` + `key_file`（PEM）。证书由共享 443 监听器按 SNI 选择（见 `design.md` §4.2）。

## 3. forward（基础转发）站点

```yaml
name: www-example
mode: forward
domain: www.example.com          # 精确匹配域名（小写；不带端口）
https: true
cert_file: /etc/iris/certs/www.example.com.crt
key_file:  /etc/iris/certs/www.example.com.key

backends:                        # 后端 IP:port 列表，可含权重
  - addr:  10.0.0.5:8080
    weight: 3
  - addr:  10.0.0.6:8080
    weight: 1

load_balancer: weighted_round_robin   # 见 §5 枚举
health_check:
  kind: tcp                     # tcp | http
  http_path: /healthz          # kind=http 时必填
  interval_secs: 5
  timeout_secs: 2
```

字段规则：

- `domain`：合法域名（允许 `*.` 开头的通配域名？**本版本不支持**——forward 为精确匹配；通配能力仅由 openstack 模式的 `base_domain` 提供，避免两类匹配语义混杂）。
- `backends`：至少 1 项；`addr` 为 `IP:port`（不接受域名，避免启动期 DNS 依赖）；`weight` 为 `0..=65535` 整数，默认 `1`；`0` 表示禁用该后端（不参与选择与健康检查）；越界拒绝。`weight` 仅对加权模式生效。
- `load_balancer`：见 §5 映射表。
- `health_check`：可选；缺省不做主动健康检查。`kind=http` 时额外发请求并由 2xx/3xx 判定健康。

## 4. openstack（全自动）站点

```yaml
name: rent
mode: openstack
base_domain: rent.do.top         # 访问形态：{instance_name}-{port}.rent.do.top
https: true
cert_file: /etc/iris/certs/rent.do.top.crt
key_file:  /etc/iris/certs/rent.do.top.key

firewall_dir: /etc/iris/firewall # 可选；配置后按 firewall.md 强制执行 ACL

openstack:
  timeout_secs: 5                # 单次 Nova 请求超时
  cache_ttl_secs: 86400          # 正向缓存 TTL，需求指定约 1 天（默认 86400）
  auth:
    api_url: https://controller.example.com:5000/v3   # Keystone v3 基地址
    approach: password           # password | application_credential
    # approach=password：
    username: proxy
    password: ${OS_PASSWORD}     # 支持 ${ENV} 展开，见 §7
    project_name: admin
    project_domain_name: Default
    user_domain_name: Default
    # approach=application_credential：
    # application_credential_id: ...
    # application_credential_secret: ...
  nova:
    api_url: https://controller.example.com:8774/v2.1 # 可选，缺省由 catalog 发现
    all_tenants: true            # 全局查找实例（跨项目）所需
    timeout_secs: 5
```

- `base_domain`：openstack 站点的根域；形如 `{instance}-{port}.{base_domain}` 的 Host 命中（§8 解析规则）。
- `firewall_dir`：可选。**配置后即强制执行** ACL（无 role 或端口未开放 → 拒绝）；未配置则不校验（见 `firewall.md` §4 的决策说明）。
- `auth.api_url` 为 Keystone 版本端点（`/v3`）。`nova.api_url` 缺省时用 token 响应中的服务目录（`nova` 服务 / `compute` 类型 / 匹配 region）发现，与 Iris_ssh 的 `nova_base_from_catalog` 一致。
- 凭据永不写入站点文件明文：secret 一律走 `${ENV}`（§7），不支持字面量密钥。
- `cache_ttl_secs` 默认 86400（1 天）；负缓存与瞬时错误 TTL 为固定值（见 `discovery.md`）。

## 5. 负载均衡模式枚举（仅 forward）

| 配置值 | Pingora `Algorithm` | 语义 |
|---|---|---|
| `round_robin` | `RoundRobin` | 等权轮询 |
| `weighted_round_robin` | `LbEngine::RR` + 权重（Pingora 加权轮询原生行为；内层泛型/权重形态以 P0 探针为准） | 加权轮询 |
| `random` | `Random` | 随机选择 |
| `ketama` | `Ketama` | 一致性哈希（键：客户端 IP），会话亲和 |

- 无权重时 `weighted_round_robin` 退化为轮询。
- 缺省 `load_balancer` 为 `round_robin`。
- 校验：未知枚举 → 加载失败（拒绝整份新配置）。

## 6. 加载与校验

`load_config(sites_dir)` 分两步，全部失败即整体失败（`reload` 时保持旧配置）：

1. **逐文件解析**：YAML → `SiteConfig` 结构；单文件错误 → 返回带文件路径的错误。
2. **全局校验**：
   - `name` 唯一；`domain`/`base_domain` 全局唯一。
   - `base_domain` 之间不允许互为子域（如 `a.example.com` 与 `b.a.example.com` 冲突），避免通配歧义。
   - `domain` 不得与 `base_domain` 重叠。
   - HTTPS 站点证书/密钥文件存在且可读（`reload` 时对文件变更做 stat，内容变更在 SNI 选择时天然生效）。
   - forward：`backends` 非空、`addr` 可解析为 `IP:port`。
   - openstack：`auth.api_url` 为有效 URL；凭据方式字段齐备；`cache_ttl_secs` 在 `[60, 7*86400]`（至少 1 分钟、最多 7 天，防止配置性价比倒挂）；`firewall_dir` 若配置则目录存在。
   - `port` 合法性上限：解析出的端口 `1..=65535`（见 §8）。

## 7. 敏感信息与环境变量展开

- 站点文件内不得出现明文 secret。
- 支持 `${ENV_NAME}` 展开（在解析阶段完成，结果不进日志、不参与 `Debug` 输出——`SiteConfig` 实现自定义 `Debug` 对 secret 打码，参照 Iris_ssh 的 redacted `Debug`）。
- 未知变量 → 加载失败。

## 8. 域名匹配规则

输入：请求 `Host` 头或 TLS SNI（443）。归一化：**小写、去尾部点**；剥离 `:80`/`:443` 缺省端口；**非缺省端口保留**——代理仅对外监听 80/443，携带非缺省端口的 Host 无法命中任何站点（→ 404）。

**匹配顺序（先精确、后通配）**：

1. `RoutingTable.exact`：`HashMap<Domain, SiteEntry>` 精确命中（forward 或 openstack 站点的 `base_domain` 本身不参与精确表）。
2. openstack 通配：对每个 `base_domain`，用正则
   `^([A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?)-([0-9]{1,5})\.` + `escape_regex(base_domain) + $`
   提取 `instance_name` 与 `port`：
   - 以 **最后一个 `-`** 分割前缀（容器名/instance 名允许含 `-`，如 `my-app1-80` → `instance=my-app1`, `port=80`）。
   - 端口不在 `1..=65535` → 视为格式非法。
   - 合法 → 站点引用 + `InstanceTarget { instance_name, port }`。
3. 均未命中 → `request_filter` 返回 404。

**歧义策略**：

- 精确匹配始终优先于任何通配（如 `www.rent.do.top` 被 forward 站点精确占用时，非 `{instance}-{port}` 形态归精确站点；`{instance}-{port}` 形态仍可能命中通配——启动校验要求 `domain` 不与 `base_domain` 重叠来消除此类冲突）。
- 若多个 openstack 站点的正则同时命中（由 §6 的 base_domain 互不重叠校验排除），不可能发生。

## 9. reload 语义（配置视角）

见 `reload.md`。要点：新配置必须 §6 全量校验通过才原子生效；后端集合变化走 `lb.update()`，站点增删走 `ArcSwap` 路由快照，证书变化走 `TlsAccept` 回调内的证书表快照。
