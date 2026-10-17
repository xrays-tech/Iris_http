# Iris HTTP — TDD 多阶段开发计划

> 依据：`propose.md`（需求），`design.md` / `config.md` / `discovery.md` / `firewall.md` / `reload.md`（设计）。
> 本文档为开发执行总纲；每阶段独立派发 fixer，按 §8 门禁验收。

## 1. 执行规约（每阶段 fixer 必须遵守）

1. **TDD 批处理约定**：阶段内**先一次性生成全部单元测试**，再一次性生成实现；
   随后**一次性运行全部单元测试并集中修正**，直到全绿。禁止边写边投机跑测。
2. **编译闭环**：`cargo build` 必须通过；生产代码禁 `unwrap`、禁显式 `any`、禁空 catch、禁吞异常。
3. **Mock 边界**：内部逻辑禁 mock（重构成纯函数后直接单测）；仅外部边界允许 mock
   （OpenStack 控制面 = 第三方 API，允许 mock；上游后端用本地 mock HTTP 服务）。
4. **测试代码豁免**：允许 `unwrap` / 宽松断言。
5. **不改设计**：实现严格对照设计文档；发现设计与实现冲突时先停止并回报，不得自行改接口。
6. **交付物**：代码 + 可复现验证命令与输出摘要（`cargo test`、`curl`/`openssl` 端到端）。
7. 每阶段完成后由工作流管理器复核产物，通过后才进入下一阶段。

## 2. 阶段总览与依赖图

```
P0 → P1 → P2 → { P3 → P4 , P5 , P6 } → P7 → P8
```

- `{}` 内为可并行 lanes（写作用域互不相交）；`→` 为强依赖。
- 并行策略：P2 完成后，同时派发 **P3 / P5 / P6** 三个 fixer；
  P3 完成后可继续 **P4**（与 P5/P6 并行）；P3、P5、P6 全部完成后进入 **P7**；最后 **P8**。

| # | 阶段 | 文件范围 | 单测主体 | 阶段门禁 |
|---|---|---|---|---|
| P0 | 脚手架与 CLI | `Cargo.toml`、`src/main.rs`、`src/lib.rs`、`src/errors.rs`、`src/logging.rs` | 无业务逻辑 | `cargo build` + 启动/优雅退出冒烟 |
| P1 | 配置解析与校验 | `src/config/{mod,forward,openstack}.rs` | 解析正反例、校验规则、`${ENV}` 展开、secret 打码 | 单测全绿 |
| P2 | 路由与域名匹配 | `src/routing.rs` | `normalize_host`、`parse_instance_host`、查找优先级 | 单测全绿 |
| P3 | forward 转发链路 + 最小 Server | `src/lb.rs`、`src/proxy.rs`、`src/main.rs` | 算法映射、后端解析、健康检查状态机 | 单测 + 双后端端到端 curl |
| P4 | TLS/SNI 与 H2 | `src/tls.rs`、`src/main.rs` | SNI 选择器映射（纯逻辑） | `openssl s_client` 证书/H2/421 |
| P5 | OpenStack 发现 | `src/openstack/{mod,auth,nova,cache}.rs` | token 生命周期、地址选择、缓存/单飞/熔断/fail-open | 单测 + mock Nova 集成 |
| P6 | 防火墙 ACL | `src/firewall.rs` | role 解析、判定顺序表驱动 | 单测全绿（含 IPv6/CIDR） |
| P7 | openstack/firewall 编排 | `src/proxy.rs`、`src/main.rs`、`src/openstack/mod.rs` | 编排分支单测 | mock Nova + curl 200/403/404/502 |
| P8 | reload 与生命周期 | `src/reload.rs`、`src/main.rs`、`src/state.rs` | 重载状态机、配置指纹 | SIGHUP 热重载端到端、优雅关闭 |

## 3. 模块接口契约（P1 落定为真实类型；后续 lanes 只实现、不改签名）

```rust
// src/config/mod.rs
pub struct Config { pub forward: Vec<ForwardSite>, pub openstack: Vec<OpenStackSite> }
pub struct ForwardSite {
    pub name: String, pub domain: String, pub http: bool, pub https: bool,
    pub cert_file: Option<PathBuf>, pub key_file: Option<PathBuf>,
    pub backends: Vec<Backend>, pub lb_mode: LbMode,
    pub health_check: Option<HealthCheck>,
}
pub struct Backend { pub addr: SocketAddr, pub weight: u32 }        // weight>=0，默认1
pub enum LbMode { RoundRobin, WeightedRoundRobin, Random, Ketama }
pub struct HealthCheck { pub kind: HealthKind, pub http_path: Option<String>,
                         pub interval_secs: u64, pub timeout_secs: u64 }
pub enum HealthKind { Tcp, Http }
pub struct OpenStackSite {
    pub name: String, pub base_domain: String, pub http: bool, pub https: bool,
    pub cert_file: Option<PathBuf>, pub key_file: Option<PathBuf>,
    pub firewall_dir: Option<PathBuf>, pub openstack: OpenStackConfig,
}
pub struct OpenStackConfig { pub cache_ttl_secs: u64, pub timeout_secs: u64,
    pub auth: AuthConfig, pub nova: NovaConfig }
pub enum AuthConfig { Password{ username: String, password: String, project_name: String,
                              project_domain_name: String, user_domain_name: String },
                      ApplicationCredential{ id: String, secret: String } }
pub struct NovaConfig { pub api_url: Option<Url>, pub all_tenants: bool }
pub enum ConfigError { Io(std::io::Error), Parse{ file: PathBuf, msg: String },
                       Validate(String), MissingEnv(String) }        // 完整 Debug/Display
pub fn load_config(sites_dir: &Path, env: &HashMap<String,String>) -> Result<Config, ConfigError>;
// env 参数供注入测试：生产传 std::env::vars 收集结果。

// src/routing.rs
pub struct RoutingTable { exact: HashMap<String, SiteKind>, openstack_patterns: Vec<OpenStackPattern> }
pub enum SiteKind { Forward(usize), OpenStack(usize) }               // 索引进 Config
pub struct OpenStackPattern { pub site_idx: usize, pub base_domain: String } // regex 编译后字段私有
pub enum RouteTarget { Forward(usize), OpenStack { site_idx: usize, instance: String, port: u16 } }
pub fn normalize_host(host: &str) -> String;                        // 小写、去尾点、去端口
pub fn parse_instance_host(prefix_suffix: &str, base_domain: &str) -> Option<(String, u16)>;
pub impl RoutingTable { pub fn lookup(&self, host: &str) -> Option<RouteTarget>; }

// src/firewall.rs
pub struct FirewallTable { roles: HashMap<String, RoleRules> }
pub struct RoleRules { pub allowed_ports: Vec<u16>,
                       pub whitelist: Vec<IpNet>, pub blacklist: Vec<IpNet> }
pub enum AclVerdict { Allow, Deny(AclReason) }
pub enum AclReason { NoRole, PortNotAllowed(u16), Blacklisted(IpAddr), NotWhitelisted(IpAddr) }
pub fn build_firewall(dir: &Path) -> Result<FirewallTable, ConfigError>;
impl FirewallTable { pub fn check(&self, instance: &str, port: u16, client: IpAddr) -> AclVerdict; }

// src/openstack/mod.rs
pub struct OpenStackResolver;                                        // Arc 持有：client/cache/单飞/熔断/last-known-good
pub struct ResolvedInstance { pub floating: Option<IpAddr>, pub fixed: Option<IpAddr> }
pub enum ResolveError { NotFound, AuthFailed, DiscoveryUnavailable(String), NoAddress }
impl OpenStackResolver {
    pub fn new(cfg: &OpenStackConfig, client: Arc<HttpClient>) -> Self;
    pub async fn resolve(&self, instance: &str) -> Result<ResolvedInstance, ResolveError>;
}
// src/openstack/auth.rs
pub struct Token { pub value: String, pub expires_at: DateTime<Utc>, pub nova_base: Url }
pub async fn fetch_token(cfg: &AuthConfig, api_url: &Url, client: &HttpClient) -> Result<Token, AuthError>;
// src/openstack/nova.rs
pub fn decode_server_list(body: &[u8]) -> Result<Vec<NovaServer>, NovaError>;
pub fn exact_name_match<'a>(name: &str, servers: &'a [NovaServer]) -> Option<&'a NovaServer>;
pub fn select_address(s: &NovaServer) -> AddressSelection;           // floating 优先 → fixed → None

// src/lb.rs
pub fn build_lb(backends: &[Backend], mode: &LbMode) -> Arc<LoadBalancer>;
pub fn health_check_service(lb: Arc<LoadBalancer>, hc: &HealthCheck) -> impl BackgroundService;

// src/proxy.rs
pub struct Ctx { pub route: Option<RouteTarget>, pub upstream: Option<SocketAddr>,
                 pub site: Option<String>, pub instance: Option<String> }
pub struct IrisProxy { pub shared: Arc<SharedState> }                 // SharedState 见 P8/reload.md

// src/tls.rs
pub struct SniCertSource { certs: ArcSwap<HashMap<String, Arc<CertKey>>> }   // 实现 TlsAccept
impl SniCertSource { pub fn from_sites(sites: &[&ForwardSite], ...) -> Result<Self>; }

// src/state.rs / src/reload.rs（P8）
pub struct SharedState { pub config: ArcSwap<Config>, pub routing: ArcSwap<RoutingTable>,
                         pub certs: ArcSwap<HashMap<String, Arc<CertKey>>>,
                         pub firewall: ArcSwap<FirewallTable>,
                         pub lbs: Arc<HashMap<String, Arc<LoadBalancer>>>,
                         pub resolvers: Arc<HashMap<String, Arc<OpenStackResolver>>> }
pub fn apply_new_config(state: &SharedState, cfg: Config) -> Result<(), ReloadError>;
```

- 并发策略：所有共享快照经 `ArcSwap`；LB 集合热更走 `lb.update()`；`OpenStackResolver` 重建后整体替换。
- 以上签名在 P1 实现时落定为真实代码；P3/P5/P6/P7/P8 只填充逻辑。

## 4. 阶段详情（P0–P4）

### P0 脚手架与 CLI
- **产出**：`Cargo.toml`（依赖见 §6）、`src/main.rs`（clap 子命令 `run` / `reload`，`--config-dir`、`--pidfile`、`--watch-config`）、`src/lib.rs`、`src/errors.rs`、`src/logging.rs`（`tracing` JSON + 控制台）。
- **TDD 单测**：`reload` 子命令的 pidfile 读取/发信号路径（用临时目录与空 pidfile 的边界）；`--config-dir` 解析。
- **门禁**：`cargo build` 通过；`iris run` 启动后 Ctrl-C/SIGTERM 优雅退出（退出码 0）；日志可输出。

### P1 配置解析与校验（纯函数核心，所有 lanes 的基座）
- **产出**：`src/config/mod.rs`（`Config`/`SiteKind`/errors/`load_config`）、`forward.rs`、`openstack.rs`；§3 契约中 `ConfigError`、`ForwardSite`、`OpenStackSite` 等全部类型；`${ENV}` 展开；redacted `Debug`（secret 打码）。
- **TDD 单测清单**：
  - forward：合法解析；空 backends 拒绝；`addr` 非法 IP:port 拒绝；`weight` 默认/越界；`lb_mode` 未知枚举拒绝；`https:true` 但缺证书文件拒绝。
  - openstack：`base_domain` 合法/非法；两种凭据方式齐备性；`cache_ttl_secs` 越界（<60 或 >7d）拒绝；`firewall_dir` 不存在拒绝。
  - 全局：`name` 重复、`domain` 重复、`base_domain` 互为子域、`domain` 与 `base_domain` 重叠 → 拒绝。
  - secret：`${ENV}` 展开成功/未知变量报 `MissingEnv`；`Debug` 输出不含密码明文。
  - 全部为 `env` 注入式，不读真实环境变量。
- **门禁**：`cargo test` 全绿。

### P2 路由与域名匹配（纯函数）
- **产出**：`src/routing.rs`；`RoutingTable::try_from(&Config)`（冲突检测复用 P1 校验）。
- **TDD 单测清单**：
  - `normalize_host`：大小写、尾点、`:80`/`:443` 端口剥离、非缺省端口保留。
  - `parse_instance_host`：`INS001-80` → `("INS001",80)`；`my-app1-8080`（含 `-` 的实例名）最后 `-` 分割；端口 `0`/`65536`/非数字 → `None`；空 instance → `None`。
  - `lookup`：精确命中 forward；`{instance}-{port}.base` 命中 openstack；精确优先于通配；均未命中 → `None`。
- **门禁**：`cargo test` 全绿。

### P3 forward 转发链路 + 最小 Server 装配
- **产出**：`src/lb.rs`（`build_lb`：`RoundRobin`/`WeightedRoundRobin`→`Backend::Weighted`、`Random`、`Ketama`(键=客户端 IP)；`health_check_service`：`TcpHealthCheck`/`HttpHealthCheck` + `lb.update_frequency`）、`src/proxy.rs` 的 **forward 分支**（`request_filter` 查路由→404、`upstream_peer` 调 `lb.select()`→`HttpPeer::new(addr,false,"")`、`fail_to_connect` 有限重试、`response_filter` 安全头、`Ctx`）、`src/main.rs` 最小装配（`Server::bootstrap` + 80 监听 + 单 forward 站点）。
- **TDD 单测**：`LbMode→Algorithm` 映射、`Backend` 构建与权重归一、健康检查状态解析（mock 健康后端/故障后端）。
- **端到端**：本地两个 mock 后端（`python3 -m http.server` 或最小 axum 测试桩），`curl -H "Host: www.example.com"` 断言：轮询交替、加权比例、随机、ketama 同源同后端；拔掉一个后端后健康检查剔除不选、恢复后回归。
- **门禁**：单测全绿 + 上述 curl 断言已跑通（fixer 附输出）。

### P4 TLS/SNI 与 H2
- **产出**：`src/tls.rs`（`SniCertSource` 实现 `TlsAccept`：`certificate_callback` 读 `SslRef::servername()` → 查 `ArcSwap` 证书表 → `ext::ssl_use_certificate`/`ssl_use_private_key`；`enable_h2()`）、`src/main.rs` 增加 `add_tls_with_settings("0.0.0.0:443", ...)`。
- **TDD 单测**：证书表构建（多站点→map）、SNI 归一化 key、未知 SNI 处理分支（纯逻辑部分；握手本身靠端到端）。
- **端到端**：`openssl s_client -connect 127.0.0.1:443 -servername www.example.com` 断言返回对应证书 CN；`-alpn h2` 协商 HTTP/2；无证书域名请求 → 421。
- **门禁**：单测全绿 + 上述 openssl 断言通过。

## 5. 阶段详情（P5–P8）

### P5 OpenStack 发现（与 P3/P6 并行）
- **产出**：`src/openstack/{mod,auth,nova,cache}.rs`，严格对照 `discovery.md`。
  - `auth.rs`：`fetch_token`（Keystone v3 `POST /v3/auth/tokens`：password / application_credential 两种 body；解析 `X-Subject-Token` + `expires_at` + 服务目录选 nova public URL）；token 提前过期缓冲（60s）；401 单次刷新重试。
  - `nova.rs`：`decode_server_list`（`server.addresses.*[].{addr,version,"OS-EXT-IPS:type"}`）、`exact_name_match`、`select_address`（IPv4 优先 → floating 优先 → fixed 回退）。
  - `cache.rs`：moka 正向 TTL(`cache_ttl_secs`)/负缓存 5s/瞬时 1s + jitter；单飞（同 key 折叠）；熔断（连续失败 5 → open，`timeout*3` 后 half-open）；last-known-good `ArcSwap` fail-open。
  - `mod.rs`：`OpenStackResolver::resolve` 组合上述（缓存→单飞→熔断→认证→Nova→地址→写缓存）。
- **TDD 单测**：token body 序列化、过期解析与时区、catalog 选择；`decode_server_list`/`select_address` 表驱动（floating-only、fixed-only、IPv6-only、多网卡、多 floating 取首个稳定序）；缓存 TTL/负缓存/瞬时、单飞并发折叠（`tokio` 并发任务断言只发一次 HTTP）、熔断状态机、fail-open 回退最后一跳。
- **集成（外部边界 mock 允许）**：`scripts/mock_openstack.py`（契约同 Iris_ssh：`POST /v3/auth/tokens`→`201`+`X-Subject-Token`；`GET /v2.1/servers?name=&all_tenants=true`）。断言：token 复用（只发一次）、401 刷新一次成功、空结果→负缓存+NotFound、floating-only、控制面宕机→熔断→fail-open 200 / 无记录→DiscoveryUnavailable。
- **门禁**：单测全绿 + mock 集成用例通过。

### P6 防火墙 ACL（与 P3/P5 并行）
- **产出**：`src/firewall.rs`：`build_firewall(dir)`（读 `*.role`、`serde_yaml` 解析、`ipnet` CIDR、纯 IP 补 `/32`/`/128`、非法 CIDR/端口/超限条目(>10000)拒绝）、`FirewallTable::check`（顺序：无 role→Deny；端口未开放→Deny；黑名单→Deny；白名单非空未命中→Deny；否则 Allow）。
- **TDD 单测**（表驱动）：判定顺序（黑名单优先于白名单）、空白名单=放行、未启用白名单时非黑即放、无 role 拒绝、端口未开放拒绝、IPv4/IPv6/CIDR 边界（`10.0.0.0/8`、`::1/128`、`203.0.113.1` 纯 IP）、非法 role 文件拒绝加载、`.role` 后缀过滤。
- **门禁**：单测全绿。

### P7 openstack/firewall 编排集成
- **产出**：`src/proxy.rs` 增加 **openstack 分支**：`request_filter` 完成 Host→`{instance,port}` 解析→`firewall.check`（Deny→403）→解析 IP；`upstream_peer` 用解析结果 `HttpPeer::new("{ip}:{port}",false,"")`；解析错误映射状态码（`NotFound`→404、`DiscoveryUnavailable`→502、ACL→403）；`fail_to_connect` 对 floating 失败回退 fixed 重试一次。`src/main.rs` 装配 openstack 站点（含 resolver 初始化、firewall 表）。`src/openstack/mod.rs` 暴露 resolver 构造接口（§3）。
- **TDD 单测**：Host 解析→ACL→resolver 的编排分支（resolver 以 trait/接口注入，仅测试编排：403/404/502 分支映射）。
- **端到端**（mock Nova + mock 上游）：`curl -H "Host: INS001-80.rent.do.top"` → 200 且上游为 `{floating_ip}:80`；端口未开放/无 role → 403；实例不存在 → 404；控制面故障 → 502/或 fail-open 200。
- **门禁**：单测全绿 + 端到端断言通过。

### P8 reload 与生命周期
- **产出**：`src/state.rs`（`SharedState`，§3）、`src/reload.rs`（`ReloadService` 实现 `BackgroundService`：监听 SIGHUP/目录变化(notify, `--watch-config`) → `apply_new_config`：校验→`lb.update()`/重建 LB→`ArcSwap` 换路由/证书/firewall→resolver 重建→`/version` 指纹更新；坏配置保持旧态；配置指纹=内容 hash）、`src/main.rs` 信号接线（SIGHUP→重载、SIGTERM→优雅关闭、SIGQUIT→升级预留）+ `prometheus_http_service()` 独立端口；后台服务统一 `shutdown.changed()` 收敛。
- **TDD 单测**：`apply_new_config` 成功切换/校验失败保持旧、配置指纹计算与幂等重载、resolver 重建后缓存保留断言。
- **门禁**：单测全绿 + 端到端断言通过。

## 6. 依赖与版本（P0 落定 Cargo.toml）

```toml
[dependencies]
pingora = { version = "0.9", features = ["openssl", "proxy", "lb"] }
pingora-proxy = "0.9"
pingora-load-balancing = "0.9"
pingora-prometheus = "0.9"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
arc-swap = "1"
moka = { version = "0.12", features = ["future"] }
ipnet = "2"
notify = "7"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"] }   # OpenStack 客户端
chrono = { version = "0.4", features = ["serde"] }
url = "2"
[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["full", "test-util"] }
```

- 版本由 `Cargo.lock` 提交；API 以 `cloudflare/pingora` main（0.9.x，2026-10）为基线。
- 若 `pingora = "0.9"` 解析不到，回退 `"0.9.*"` 并以其实际版本核对 API（`TlsSettings::with_callbacks`、`HttpPeer::new(addr,tls,sni)`、`PeerOptions`、`pingora_core::services::background`）；任何 API 差异在 P0 回报，不改设计。

## 7. 风险与缓解

| 风险 | 缓解 | 责任人阶段 |
|---|---|---|
| 设计文档 API 名称与真实 Pingora 版本有出入 | P0 冒烟即验证核心 API（Server/TlsSettings/HttpPeer）；差异回报后仅调整实现映射，不改接口契约 | P0 |
| OpenStack 契约假设错误（token/响应结构） | 以 `scripts/mock_openstack.py` 为唯一契约；全部 P5/P7 集成用例基于它；契约先于实现编写 | P5 |
| 并行 lanes 编辑冲突 | §3 契约固定各文件归属；P3/P5/P6 文件互不重叠；提交粒度=阶段 | P3/P5/P6 |
| 重载语义破坏在途连接 | P8 端到端专门验证"慢请求/WebSocket 跨重载""坏配置保持旧态" | P8 |
| 地址选择/ACL 判定边界漏测 | 表驱动单测覆盖浮动/固定/IPv6/多网卡/CIDR 边界 | P5/P6 |
| secret 泄漏进日志 | P1 redacted Debug 单测 + 全局日志扫关键词（CI 一阶段） | P1 |

## 8. 门禁验收（每阶段 fixer 交回即复核）

1. `cargo build` 无 warning 增量（生产代码禁 `unwrap`/显式 `any`/空 catch）。
2. `cargo test` 全绿（本阶段新增用例 + 既有用例无回归）。
3. 端到端断言齐全且附可复现输出（curl/openssl 命令原文）。
4. 文件范围不越界（§3 归属）；未改其他阶段接口签名。
5. 未引入与设计相悖的行为；有疑义先回报。
6. 每阶段完成即提交（commit 信息含阶段号），由工作流管理器复核后方可进入依赖它的下一阶段。

## 9. 阶段依赖的 mock 与测试桩清单

- `scripts/mock_openstack.py`：P5 编写；P7 复用。契约：
  - `POST /v3/auth/tokens` → `201` + `X-Subject-Token`，body 含 Nova catalog。
  - `GET /v2.1/servers?name=&all_tenants=true` → 200 JSON（可选 404/403/超时模式）。
  - 记录收到的 `X-Auth-Token` / `OpenStack-API-Version`，供断言 token 复用次数。
- `tests/stubs/upstream.py`：P3 编写；可配置响应体/端口/延迟/断连，供 LB 与 P7/P8 端到端。
- fixtures：`tests/fixtures/openstack/servers/*.json`（多网卡、floating-only、fixed-only、IPv6-only、多 floating）；`tests/fixtures/firewall/*.role`。

