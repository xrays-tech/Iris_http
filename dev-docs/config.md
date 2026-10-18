# Iris HTTP 反向代理 — 配置体系设计

> 依据：`propose.md`（站点两种模式、权重、负载均衡、防火墙目录）。总体架构见 `design.md`。

## 1. 配置目录组织

程序启动时以 `--config-dir`（默认 `./config` 或 `/etc/iris`）定位配置；**`--config-dir` 是配置根目录，`sites/` 与 `openstack/` 为其子目录**。主程序将 `--config-dir` 推导为 `<config-dir>/sites`（站点）与 `<config-dir>/openstack`（独立 OpenStack 环境）后交给 `load_config`（`main.rs`）。

```
config/
├─ sites/                # 站点配置目录：每站点一个 YAML 文件（文件名无语义，仅加载顺序无关）
│  ├─ 001-forward-example.yaml
│  ├─ 002-openstack-example.yaml
│  └─ 003-forward-instance-example.yaml
├─ openstack/            # 独立 OpenStack 环境目录：每环境一个 YAML 文件（文件 stem = 环境名）
│  └─ prod.yaml
└─ firewall/             # 防火墙规则目录（被各 openstack 站点以 firewall_dir 引用，可不在此目录内）
   ├─ INS001.role
   └─ INS002.role
```

- `sites/` 下所有 `*.yaml` 一起构成 `Config`；`openstack/` 下所有 `*.yaml` 一起构成 `Config.openstack_envs`（键 = 文件 stem）。重复/冲突在加载时整体校验（见 §6）。
- `firewall` 目录路径在**每个 openstack 站点内**配置（`firewall_dir`，**必填**），因此不同站点可指向不同目录。
- 全部加载逻辑是纯函数 `load_config(sites_dir: &Path, envs_dir: &Path, env: &HashMap<String, String>) -> Result<Config>`，供启动与 `reload` 复用（见 `reload.md`）。`envs_dir` 不存在或非目录 = **无任何环境**（`Ok(空)`）；任一环境文件坏则整份加载失败。

## 2. 通用字段

每个站点文件含两种模式（`mode` 字段区分）：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | 是 | 站点标识，仅用于日志/指标/错误定位，必须唯一 |
| `mode` | enum | 是 | `forward` \| `openstack` |
| `http` | 布尔 | 否 | 是否响应明文 HTTP（80 端口），默认 `true`；`false` 时该站点在明文监听器上返回 404（HTTPS-only 站点，拒绝明文降级） |
| `https` | 布尔 | 是 | 是否启用 HTTPS（443 端口） |
| `listen.http_port` / `listen.https_port` | int | 否 | v1 不接受：监听固定共享 `0.0.0.0:80`/`0.0.0.0:443`（design.md §4.1；测试换端口在 main 装配层进行，不进配置） |

`https: true` 时必须提供 `cert_file` + `key_file`（PEM）。证书由共享 443 监听器按 SNI 选择（见 `design.md` §4.2）。

## 3. forward（基础转发）站点

```yaml
name: www-example
mode: forward
domain: www.example.com          # 精确匹配域名（小写；不带端口）
https: true
cert_file: /etc/iris/certs/www.example.com.crt
key_file:  /etc/iris/certs/www.example.com.key

backends:                        # 后端列表，可含权重
  - addr:  10.0.0.5:8080         # addr：字面 IP:port
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

后端条目也可引用 OpenStack 实例（名称或 UUID）而非字面地址；此时站点级 `openstack_env` 必填：

```yaml
name: apps-example
mode: forward
domain: apps.example.com
https: false

openstack_env: prod             # 任一 backend 为 instance 时必填，引用 <config-dir>/openstack/prod.yaml

backends:
  - instance: INS001            # 实例名，或 8-4-4-4-12 UUID（见 §4.2）
    port: 8080                  # instance 后端必填的显式端口
    weight: 2
  - addr: 10.0.0.6:8080         # addr 与 instance 可在同一站点内混用（entry 级二选一）
    weight: 1

load_balancer: weighted_round_robin
health_check:
  kind: tcp
  interval_secs: 5
  timeout_secs: 2
```

字段规则：

- `domain`：合法域名（允许 `*.` 开头的通配域名？**本版本不支持**——forward 为精确匹配；通配能力仅由 openstack 模式的 `base_domain` 提供，避免两类匹配语义混杂）。
- `backends`：至少 1 项；每个条目**恰设 `addr` 或 `instance` 之一**（同设或皆无 → 加载失败，`deny_unknown_fields`）。
  - `addr`：`IP:port`（不接受域名，避免启动期 DNS 依赖）；条目不得再带 `port` 字段。
  - `instance`：实例名，或 `8-4-4-4-12` UUID（十六进制、大小写不敏感）；**必须带显式 `port`**。经站点引用的 OpenStack 环境解析（floating 优先 / fixed 回退），解析逻辑见 `discovery.md`。
- `openstack_env`：可选；但**任一 backend 为 instance 时必填**，且必须引用一个已加载的环境（§4.1）。全 `addr` 后端可省略。
- `weight`：`0..=65535` 整数，默认 `1`；`0` 表示禁用该后端（不参与选择与健康检查）；越界拒绝。`weight` 仅对加权模式生效。
- ketama 模式下，instance 后端的哈希键取**实例身份**（名称/UUID）而非解析出的 IP——IP 会随重新解析而漂移，若以 IP 作键会导致同一客户端被重排到别的后端。
- `load_balancer`：见 §5 映射表。
- `health_check`：可选；缺省不做主动健康检查。`kind=http` 时额外发请求并由 2xx/3xx 判定健康。

## 4. openstack（全自动）站点

站点的控制面配置不再内嵌于站点文件，而是**按名引用**一个独立环境文件（§4.1）；openstack 站点仅保留站点自身字段：

```yaml
name: rent
mode: openstack
base_domain: rent.do.top         # 访问形态：{instance_name}-{port}.rent.do.top
https: true
cert_file: /etc/iris/certs/rent.do.top.crt
key_file:  /etc/iris/certs/rent.do.top.key

firewall_dir: /etc/iris/firewall # 必填；按 firewall.md 强制执行 ACL（default-deny）

openstack_env: prod              # 必填；引用 <config-dir>/openstack/prod.yaml（§4.1）
```

- `base_domain`：openstack 站点的根域；形如 `{instance}-{port}.{base_domain}` 的 Host 命中（§8 解析规则）。
- `firewall_dir`：**必填**。目录必须存在，缺失或不存在 → 加载校验失败（`ConfigError::Validate`，reload 时保持旧配置）。ACL 恒定强制执行（无 role 或端口未开放 → 403 拒绝）；不再存在"未配置防火墙"的跳过路径（见 `firewall.md` §4）。
- `openstack_env`：**必填**。站点文件**不再接受内嵌 `openstack:` 块**（旧格式 → Parse 错误）；必须引用一个已加载的环境，否则整份配置校验失败（§6）。引用方式也适用于 forward 站点的 instance 后端（§3）。

### 4.1 独立环境文件（`<config-dir>/openstack/<env-name>.yaml`）

文件名（不含 `.yaml`）即环境名；文件内容为**旧站点 `openstack:` 块的字段**（无顶层 `name`/`mode`）：

```yaml
timeout_secs: 5                # 单次 Nova 请求超时，校验范围 [1, 600]
cache_ttl_secs: 86400          # 正向缓存 TTL，需求指定约 1 天（默认 86400），校验范围 [60, 604800]
auth:
  api_url: https://controller.example.com:5000/v3   # Keystone v3 基地址（必填）
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
  all_tenants: true            # 全局查找实例（跨项目）所需（Nova 请求超时用顶层 timeout_secs）
```

- 环境名（文件 stem）全局唯一；重名 → 加载失败。目录缺失 = 无环境；此时任何引用该环境的站点都会校验失败。
- **一个环境被任意多个 openstack 站点与 forward instance 池共享**：每环境一个 resolver、一份缓存（缓存/单飞/熔断/last-known-good 在同一环境的所有消费者间共享），见 `discovery.md` §6.6。
- `auth.api_url` 为 Keystone 版本端点（`/v3`）。`nova.api_url` 缺省时用 token 响应中的服务目录（`nova` 服务 / `compute` 类型 / 匹配 region）发现，与 Iris_ssh 的 `nova_base_from_catalog` 一致。
- 凭据不得写入环境文件明文：secret 推荐一律走 `${ENV}`（§7）；字面量值亦会被解析（无法在展开后可靠区分），属运维规范违规；日志与 `Debug` 永不落任何 secret（`Config::openstack_envs` 手写 `Debug` 只渲染环境数量）。
- `cache_ttl_secs` 默认 86400（1 天）；负缓存与瞬时错误 TTL 为固定值（见 `discovery.md`）。
- `timeout_secs` 默认 5，单次 Nova 请求超时；校验范围 `[1, 600]`（0 会让每次请求瞬时失败 → 持续 502；上限 10 分钟）。

### 4.2 实例标识（名称 / UUID）解析优先级

实例标识可写实例名，或形如 `8-4-4-4-12` 的 UUID（十六进制、大小写不敏感）。**任何形如 UUID 的字符串一律按 UUID 走 Nova show-by-id**（`GET /v2.1/servers/{uuid}`）：

- 因此，一个**真实名称为 UUID 形态**的实例**无法再按名称解析到**——该标识会被当作 UUID 处理。这是刻意的、有文档的规则（见 `discovery.md` §4）。
- 其余标识按名称做精确匹配（`GET /v2.1/servers?name=...&all_tenants=true` 后逐字节相等）。

环境文件示例见 `tests/fixtures/openstack/prod.yaml`；openstack 站点引用示例见 `tests/fixtures/sites/002-openstack-example.yaml`；forward instance 后端示例见 `tests/fixtures/sites/003-forward-instance-example.yaml`。

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

`load_config(sites_dir, envs_dir, env)` 分两步，全部失败即整体失败（`reload` 时保持旧配置）：

1. **解析阶段**：
   - **环境文件先行**：`envs_dir` 下每个 `*.yaml` → `OpenStackConfig`，键为文件 stem；任一文件坏 → 整份加载失败（在任何站点被解析之前）。
   - **站点文件**：`sites/` 下每个 `*.yaml` → 按 `mode` 判别为 `ForwardSite` / `OpenStackSite`；单文件错误 → 返回带文件路径的错误。目录内容按文件名排序读取，保证确定性。
2. **全局校验**：
   - `name` 唯一；`domain`/`base_domain` 全局唯一。
   - `base_domain` 之间不允许互为子域（如 `a.example.com` 与 `b.a.example.com` 冲突），避免通配歧义。
   - `domain` 不得与 `base_domain` 重叠。
   - HTTPS 站点证书/密钥文件存在且可读（`reload` 时对文件变更做 stat，内容变更在 SNI 选择时天然生效）。
   - forward：`backends` 非空；每个条目恰设 `addr`（可解析为 `IP:port`）或 `instance`（非空、带 `port`）之一；**有 instance 后端时 `openstack_env` 必填且引用存在**；`weight` 在 `0..=65535`。
   - openstack 站点：`openstack_env` 必填且引用存在；`firewall_dir` **必填**且必须是已存在的目录（缺失或不存在 → `Validate` 失败）。
   - openstack 环境文件：`auth.api_url` 为有效 http(s) URL；凭据方式字段齐备（`password` / `application_credential` 的字段集互斥）；`cache_ttl_secs` 在 `[60, 7*86400]`（至少 1 分钟、最多 7 天，防止配置性价比倒挂）；`timeout_secs` 在 `[1, 600]`（0 会导致每次 Nova 请求瞬时失败）。
   - 站点引用的 `openstack_env` 必须存在于已加载的 `Config.openstack_envs`，否则 `Validate` 失败。
   - `port` 合法性上限：解析出的端口 `1..=65535`（见 §8）。

## 7. 敏感信息与环境变量展开

- 站点文件内不得出现明文 secret：secret 一律推荐 `${ENV_NAME}` 展开；字面量值亦接受（无法在展开后可靠区分），仅作兼容、运维规范要求走 `${ENV}`。
- `${ENV_NAME}` 展开在解析阶段完成，结果不进日志、不参与 `Debug` 输出——`SiteConfig` 实现自定义 `Debug` 对 secret 打码（参照 Iris_ssh 的 redacted `Debug`）。
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

见 `reload.md`。要点：新配置必须 §6 全量校验通过才原子生效；站点增删走 `ArcSwap` 路由快照，证书变化走 `TlsAccept` 回调内的证书表快照。forward 站点池按 `backends`/`lb_mode`/`health_check`/`openstack_env` **四元比较**决定复用（不变则复用同一 `Arc<ForwardBackends>`）；OpenStack 环境按 `OpenStackConfig` 相等决定复用同一 `Arc<dyn Resolver>`（进程内缓存保留）。配置指纹按**环境名排序**逐个哈希各环境（含凭据真实值）。
