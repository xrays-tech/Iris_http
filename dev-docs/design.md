# Iris HTTP 反向代理 — 总体技术设计

> 依据：`dev-docs/propose.md`。本文件为总体架构；分项设计见
> `config.md`（配置体系）、`discovery.md`（OpenStack 自动模式）、`firewall.md`（防火墙规则）、`reload.md`（动态重载与生命周期）。

## 1. 定位与目标

用 Rust + Pingora 实现一个 HTTP/HTTPS 反向代理，满足：

- 程序启动时从配置目录加载站点配置（每站点一个文件）。
- 两种站点模式：
  - **forward（基础转发）**：精确域名匹配 → 从后端 `IP:port` 列表（可带权重）按负载均衡模式选取后端并代理。
  - **openstack（全自动模式）**：`{instance_name}-{port}.{base_domain}` 形式域名 → 解析 instance 的 floating/fixed IP → 反向代理到 `{ip}:{port}`；查询结果缓存约 1 天；发现逻辑参照 `xrays-tech/Iris_ssh`。
- openstack 站点可配置防火墙规则目录（`INSTANCE.role` 文件：开放端口 + 白名单/黑名单 IP）。
- 支持 `reload` 命令热重载配置，不中断在途连接。

## 2. 范围

**做**：

- HTTP/HTTPS 反向代理（TLS 在代理侧终止），HTTP/1.1（含 WebSocket，经 Pingora `proxy` 特性）；经 `ALPN` 协商 HTTP/2。
- 基于 Host 头 / SNI 的虚拟主机路由：精确匹配 + openstack 子域通配。
- 加权负载均衡 + 健康检查。
- OpenStack 实例发现（Keystone v3 认证、Nova 查询、缓存、熔断、fail-open）。
- 防火墙访问控制（端口白名单、客户端 IP 白/黑名单）。
- `reload` 热重载、优雅关闭、优雅升级。
- Prometheus 指标。

**不做**（避免扩大范围）：

- 不集成 Neutron（按端口反查）、不调用 Glance（镜像元数据）。地址信息仅取 Nova `server.addresses` 中的 floating/fixed IP。
- 不做进程外缓存/多进程共享缓存；缓存为进程内（与 Iris_ssh 一致）。
- 上游默认走明文 HTTP（符合"反向代理 http 请求到 floating-ip 的 80 端口"）。对上游启用 TLS 通过 `HttpPeer` 的 TLS 字段作为扩展点保留，不在本版本启用。
- 不做 Layer-4 转发（本项目为七层代理）。

## 3. 总体架构

```
                        ┌──────────────────────────────────────────────┐
  客户端 ──HTTP:80 ────►│  Pingora Server                             │──► 后端（forward 站点）
  客户端 ──HTTPS:443──► │  ┌───────────┐  ┌─────────────────────────┐  │──► 上游（openstack 站点）
                        │  │ 80 监听器  │►►│  ProxyHttp 实现           │  │
                        │  │443 Tls监听 │  │  request_filter          │  │
                        │  │(SNI TlsAccept)│  upstream_peer           │  │
                        │  └───────────┘  │  response_filter         │  │
                        │       │         │  fail_to_connect         │  │
                        │       ▼         └───────────┬─────────────┘  │
                        │  ┌───────────────┐          │                │
                        │  │ 路由表 ArcSwap │◄─────────┘                │
                        │  │ (域名→站点)    │                           │
                        │  └───────┬───────┘                           │
                        │          │ 读配置快照                          │
                        │  ┌───────▼───────┐   ┌──────────────────────┐ │
                        │  │ forward 转发   │   │ openstack 站点        │ │
                        │  │ LoadBalancer  │   │  ACL 检查→发现→缓存    │ │
                        │  │ (加权/随机/   │   │  → 返回 IP:port        │ │
                        │  │  ketama)      │   └──────────────────────┘ │
                        │  └───────────────┘                            │
                        │  后台服务：健康检查 / 配置监视 / 指标            │
                        └──────────────────────────────────────────────┘
```

- 全进程共享两个监听器：`0.0.0.0:80`（纯 HTTP）与 `0.0.0.0:443`（TLS）。站点不是独立监听器，而是同一监听器内的**虚拟主机路由**（`Host`/SNI → 站点）。这是多站点证书热换与 `reload` 的前提：站点增删只改路由表，不碰监听器。
- 站点是否启用 HTTPS 由站点配置决定：`https` 关闭的站点只在 80 上可达；`https` 开启的站点需提供证书，443 握手时按 SNI 选择证书——forward 用精确域名；openstack 以 `base_domain` 通配（需 operator 提供 `*.base_domain` 通配证书）。**SNI 无任何站点证书匹配 → 中断 TLS 握手**（TLS 层无法回 HTTP 状态码）；仅当 **TLS 已建立** 且请求 `Host` 命中的站点 `https=false`（或于 SNI 不一致）时返回 HTTP 421（Misdirected Request）。

## 4. 核心组件与职责

### 4.1 Server 装配（`main.rs`）

按 Pingora 0.9 语义装配：`Server::bootstrap()` → 设置 `ServerConf`（`grace_period_seconds`、`graceful_shutdown_timeout_seconds`、`version: 1`）→ 构建代理服务 `http_proxy_service(&server.configuration, app)` 并向该服务挂监听（**不是** `server.add_listener`；精确装配方式以 P0 API 探针为准）→ `add_service` 注册各 Service：

- HTTP 监听：`svc.add_tcp("0.0.0.0:80")`
- TLS 监听：`svc.add_tls_with_settings("0.0.0.0:443", None, tls_settings)`（第二个参数为 `TcpSocketOptions`，默认 None）
- 健康检查服务、配置监视服务、Prometheus 服务（`pingora_prometheus::prometheus_http_service()`）
- `run_forever()`，由信号驱动关闭：
  - SIGTERM → 优雅关闭（排干在途请求，超时由 `graceful_shutdown_timeout_seconds` 控制）
  - SIGQUIT → 优雅升级（需要 `--upgrade` + `upgrade_sock`，移植自 Pingora）
  - SIGINT → 快速关闭
  - SIGHUP → 进程内热重载（见 `reload.md`；SIGHUP 是否被 Pingora 接管由 P0 探针确认——若缺省行为会杀进程，则由 reload 服务自建 tokio `SignalKind::hangup()` 处理器）

### 4.2 TLS 与 SNI 证书选择（`tls.rs`）

- 使用 `TlsSettings::with_callbacks(...)` + 实现 Pingora `TlsAccept` 的回调（0.9 不再提供 0.1 时代的 `with_sni_dynamic`/`SniCallback`）。
- 回调 `certificate_callback(&self, &mut SslRef)`（`TlsAccept` 只读 `&self`，证书表经 `ArcSwap` 共享）：从 `SslRef::servername()` 取 SNI → 查 `ArcSwap<SniCertMap>` 当前快照（`SniCertMap { exact: HashMap<Domain, Arc<CertKey>>, bases: Vec<(base_domain, Arc<CertKey>)> }`）→ `ext::ssl_use_certificate` + `ext::ssl_use_private_key` 装配证书；查找规则：精确命中 `exact` → 否则后缀匹配最长 `base_domain`（openstack 站点）；均未命中 → 中断握手。
- 启用 `enable_h2()`（H2/H1 ALPN）。
- 证书表在 `reload` 时整体换新（新请求/握手即可见新证书，旧连接不受影响）。
- 归一化：同一域名大小写统一为小写后作 key；同时将域名本身加入路由快照，供 `upstream_peer` 使用。

### 4.3 ProxyHttp 实现（`proxy.rs`）

单一 `ProxyHttp` 实现，路由放请求级：

| 方法 | 作用 |
|---|---|
| `request_filter` | 读 `req.host`，规范化后查路由表；非法/未命中 → 直接返回 404；openstack 站点 → 解析 `{instance}-{port}`、ACL 校验（见 `firewall.md`），校验失败返回 403。 |
| `upstream_peer` | 返回 `Box<HttpPeer>`：forward 站点用 `lb.select()`（注入 `ConnectOptions`/`PeerOptions` 端口、超时），openstack 站点用解析出的 `{ip}:{port}`；均为明文 HTTP 上游（`HttpPeer::new(addr, false, "")`），上游超时、重试参数写入 `PeerOptions`。解析失败时经 `fail_to_connect`/错误响应返回 502。 |
| `fail_to_connect` | 记录日志/指标；按 Pingora 的 `backoff_retry` 示例做有限重试（可配置，默认 1 次）。 |
| `response_filter` | 注入标准安全头；旁路响应（`INTERNAL_ERROR`/`SHUTDOWN` 等）直接放行。 |
| `logging` | 访问日志（状态码、站点、上游、耗时、字节数）。 |

### 4.4 路由表（`routing.rs`）

- 数据：`ArcSwap<RoutingTable>`。
- 结构：`HashMap<Domain, SiteEntry>`（精确匹配，forward 与 openstack 共用）＋ 各 openstack 站点的 `base_domain` 通配匹配器。
- 匹配规则（见 `config.md` §5）：先精确、后通配；同 host 可能同时命中时以精确优先；启动校验阶段禁止 base_domain 相互冲突。
- 每请求从 `ArcSwap` 取一次快照原子读，无锁。

### 4.5 负载均衡（`lb.rs`，仅 forward 站点）

- 每 forward 站点一个 `LoadBalancer`（`pingora-load-balancing`）。
- 后端建模：`Backend::new(addr)`；启用权重时用 `Backend::Weighted(weight)`（`BackendWeight`），与 `Algorithm` 配合：
  - `round_robin` → `Algorithm::RoundRobin`（等权轮询）
  - `weighted_round_robin` → `Algorithm::RoundRobin`（Pingora 的 RoundRobin 对 Weighted 后端即加权轮询）
  - `random` → `Algorithm::Random`
  - `ketama` → `Algorithm::Ketama`（按客户端一致性哈希，实现会话亲和；hash 键取客户端 IP）
- 后端集合热更新用 `lb.update(backends)`（原子替换 selector，不打断在途请求）。
- 健康检查：每站点一个后台任务 `TcpHealthCheck`（或 `HttpHealthCheck`），周期 `update_frequency` 调 `lb.update()` 剔除/恢复故障后端；故障后端不参与选择，恢复后自动回归。

### 4.6 OpenStack 解析器（`openstack/`，仅 openstack 站点）

详见 `discovery.md`。职责：token 获取与缓存、Nova 查询、`{instance} → {floating_ip, fixed_ip}` 解析、TTL 缓存（正向 1 天）、负缓存、单飞、熔断、fail-open（last-known-good）。

### 4.7 防火墙判定（`firewall.rs`）

详见 `firewall.md`。职责：加载 `<instance>.role`、端口白名单 + IP 白/黑名单顺序判定，命中即 403。

### 4.8 配置监视/重载服务（`reload.rs`）

详见 `reload.md`。后台服务监听 SIGHUP 与配置目录变化，重新解析并原子切换配置快照。

## 5. 请求生命周期（openstack 站点，最复杂路径）

1. 接受连接；443 则先完成 TLS 握手（SNI 选证书）。
2. `request_filter`：取 `Host`，规范化。
3. 路由匹配：`base_domain` 通配命中 → 提取 `instance_name` 与 `port`；格式非法 → 404。
4. ACL：查 `instance_name.role` → 端口不在 `allowed_ports` → 403；客户端 IP 命中黑名单 → 403；白名单非空且未命中 → 403。
5. 解析 IP：缓存命中（TTL 1 天）或经 Nova 查询（单飞 + 熔断 + fail-open）得到 `floating_ip`（回退 `fixed_ip`）。
6. `upstream_peer`：构造 `HttpPeer`，`addr = {ip}:{port}`，明文 HTTP。
7. Pingora 转发请求/响应；记录指标与访问日志。
8. 上游失败：`fail_to_connect` 有限重试 + 502。

## 6. 模块与代码结构

```
iris-http/
├─ Cargo.toml
├─ config/                    # 运行时配置目录（--config-dir，默认 /etc/iris 或 ./config）
│  ├─ sites/*.yaml            # 每站点一个文件（forward / openstack）
│  └─ firewall/*.role         # 每个实例一个规则文件（由 openstack 站点引用其目录）
└─ src/
   ├─ main.rs                 # Server 装配、信号、CLI（iris reload 等）
   ├─ config/
   │  ├─ mod.rs               # Config / SiteConfig 解析、目录加载、校验（纯函数）
   │  ├─ forward.rs           # forward 站点解析与校验
   │  ├─ openstack.rs         # openstack 站点解析与校验（含 secret 展开 ${ENV}）
   │  └─ firewall.rs          # role 文件解析与校验
   ├─ routing.rs              # RoutingTable、域名匹配
   ├─ proxy.rs                # ProxyHttp 实现
   ├─ tls.rs                  # TlsAccept 回调（SNI→证书）
   ├─ lb.rs                   # forward 站点 LoadBalancer 构建 + 健康检查任务
   ├─ openstack/
   │  ├─ mod.rs               # 解析器组合（缓存、单飞、熔断、fail-open）
   │  ├─ auth.rs              # Keystone v3 认证 + token 缓存/刷新
   │  ├─ nova.rs              # Nova 查询 + 响应解码 + 地址选择
   │  └─ cache.rs             # 缓存、单飞、熔断、last-known-good
   ├─ firewall.rs             # ACL 判定（端口 + CIDR 匹配）
   ├─ reload.rs               # 重载后台服务
   └─ metrics.rs              # Prometheus 指标定义
```

## 7. 依赖与版本基线

```toml
[dependencies]
pingora = { version = "0.9", features = ["openssl", "proxy", "lb"] }  # openssl 为默认 TLS 后端
pingora-proxy = "0.9"
pingora-load-balancing = "0.9"
pingora-prometheus = "0.9"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
arc-swap = "1"
moka = { version = "0.12", features = ["future"] }
ipnet = "2"                  # CIDR 白/黑名单匹配
notify = "7"                 # 配置目录监视（reload 用）
anyhow = "1"
tracing = "0.1" / tracing-subscriber = "0.3"
clap =  { version = "4", features = ["derive"] }   # CLI：iris run / iris reload
```

> API 基线：以上 API 名称以 `cloudflare/pingora` main（0.9.x，2026-10）核对。
> 注意 0.9 与旧教程差异：`TlsSettings::with_callbacks`（无 `SniCallback`）、
> `HttpPeer::new(addr, tls, sni)`（`ConnectOptions` 已更名 `PeerOptions`）、
> 后台服务位于 `pingora_core::services::background`。锁定版本由 `Cargo.lock` 提交。

## 8. 关键设计决策

| # | 决策 | 备选 | 理由 |
|---|---|---|---|
| D1 | 共享 80/443 监听器 + 虚拟主机路由 | 每站点独立监听器 | 站点增删/证书更换只需要换路由快照，是热重载的前提；避免监听器端口冲突与 FD 管理复杂度 |
| D2 | 单一 `ProxyHttp` 实现，路由放请求级 | 每站点一个 Service | 简化生命周期；路由表统一原子切换 |
| D3 | 路由表/证书表用 `ArcSwap` 快照替换 | 每请求加锁 | 无锁读、原子可见；旧快照在最后使用者释放后回收，在途请求不受影响 |
| D4 | forward 用 `LoadBalancer` + `update()` | 自研选择器 | Pingora 原生支持原子换表、健康检查与统计，符合"最佳实践"且不重复造轮子 |
| D5 | openstack 发现：进程内 TTL 缓存（1 天）+ 负缓存 + 单飞 + 熔断 + fail-open | 每次穿透或独立缓存进程 | 与 Iris_ssh 已验证实现一致；防缓存击穿/雪崩，容忍控制面抖动 |
| D6 | 防火墙默认 deny：开放端口白名单 + IP 白/黑名单 | 默认 allow | 安全最佳实践；role 未覆盖的端口/实例一律拒绝 |
| D7 | 上游默认明文 HTTP；TLS 上游仅作扩展点 | 默认 TLS | 符合需求描述（proxy http 到 floating-ip 端口）；避免引入证书信任面 |

## 9. 可观测性

- **指标**：`pingora_prometheus`，暴露在独立端口（如 `0.0.0.0:9090/metrics`，不对外）。指标项：
  - 每站点请求总数/状态码分布/耗时直方图/上游选择失败数
  - `request_filter` 拒绝数（404/403，按原因分）
  - OpenStack：缓存命中率、Nova 查询延迟、token 刷新次数、熔断开/闭状态、fail-open 次数
  - Pingora 内置：上游连接池、主动健康检查状态
- **日志**：`tracing`（JSON 行格式）。访问日志包含：`host`、`site`、`instance`、`upstream_addr`、`status`、`bytes`、`duration_ms`。重载与 ACL 拒绝均有结构化事件。
- **运行状态**：`loader version` 通过 `ServerConf.version` + 自定义版本服务（`GET /version`）暴露当前配置指纹（配置内容 hash），便于确认重载是否生效。

## 10. 验证与测试策略

- 配置解析与校验、域名解析、地址选择、ACL 判定、缓存/TTL/熔断逻辑全部为**纯函数**，直接单元测试（测试代码豁免 `unwrap`）。
- OpenStack 交互：以 `scripts/mock_openstack.py` 契约（Iris_ssh 同款：`POST /v3/auth/tokens` → `201` + `X-Subject-Token`；`GET /v2.1/servers?name=` 等）做集成测试，覆盖：token 缓存与 401 单次刷新、floating-only、跨项目、控制面宕机 fail-open。
- 端到端：`curl -H "Host: ..."` 直连代理断言状态码与转发目标；`reload` 后断言新旧配置切换与在途长连接不断；WebSocket 转发冒烟。
- 全部改动经编译（`cargo build`）+ 测试（`cargo test`）闭环；Web 行为用 Playwright 冒烟验证网关响应。
