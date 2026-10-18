# Iris HTTP — 模块接口契约（跨阶段固定）

> 由 **P1** 落定为 `src/` 内真实类型；P3/P5/P6/P7/P8 只实现、不改签名。
> 这是并行 lanes 不冲突的根基：各文件的归属见 `01-overview.md` 阶段表。

```rust
// src/config/mod.rs
pub struct Config { pub forward: Vec<ForwardSite>, pub openstack: Vec<OpenStackSite>,
                    pub openstack_envs: HashMap<String, Arc<OpenStackEnv>> }   // 键=环境名（file stem）
pub struct ForwardSite {
    pub name: String, pub domain: String, pub http: bool, pub https: bool,
    pub cert_file: Option<PathBuf>, pub key_file: Option<PathBuf>,
    pub openstack_env: Option<String>,      // 任一后端为 Instance 时必填；引用 openstack_envs 中的环境名
    pub backends: Vec<Backend>, pub lb_mode: LbMode,
    pub health_check: Option<HealthCheck>,
}
pub struct Backend { pub target: BackendTarget, pub weight: u32 }   // weight 0..=65535，0=禁用
pub enum BackendTarget {
    Addr(SocketAddr),                                                // addr: "IP:port" 字面量
    Instance { instance: String, port: u16 },                        // instance: 实例名或 UUID(8-4-4-4-12 hex)；port 必填
}
pub enum LbMode { RoundRobin, WeightedRoundRobin, Random, Ketama }
pub struct HealthCheck { pub kind: HealthKind, pub http_path: Option<String>,
                         pub interval_secs: u64, pub timeout_secs: u64 }
pub enum HealthKind { Tcp, Http }
// OpenStack 环境独立化：<config-dir>/openstack/<env-name>.yaml（文件 stem=环境名，全局唯一）。
// 站点不再内嵌 openstack{} 块，只按名引用环境；forward instance 后端与 openstack 站点共用同一环境 resolver/缓存。
pub struct OpenStackSite {
    pub name: String, pub base_domain: String, pub http: bool, pub https: bool,
    pub cert_file: Option<PathBuf>, pub key_file: Option<PathBuf>,
    pub firewall_dir: Option<PathBuf>, pub openstack_env: String,   // 必填；引用 openstack_envs
}
pub struct OpenStackEnv { pub name: String, pub config: OpenStackConfig }
impl OpenStackEnv { pub fn config(&self) -> &OpenStackConfig; }     // Debug 经 OpenStackConfig（redacted）
pub struct OpenStackConfig { pub cache_ttl_secs: u64, pub timeout_secs: u64,
    pub auth: AuthConfig, pub nova: NovaConfig }
pub struct AuthConfig { pub api_url: Url, pub credentials: AuthCredentials }  // api_url = Keystone v3 基地址 https://...:5000/v3
pub enum AuthCredentials { Password{ username: String, password: String, project_name: String,
                              project_domain_name: String, user_domain_name: String },
                      ApplicationCredential{ id: String, secret: String } }
pub struct NovaConfig { pub api_url: Option<Url>, pub all_tenants: bool }
pub enum ConfigError { Io(std::io::Error), Parse{ file: PathBuf, msg: String },
                       Validate(String), MissingEnv(String) }        // 完整 Debug/Display
pub fn load_openstack_envs(envs_dir: &Path, env: &HashMap<String,String>)
    -> Result<HashMap<String, Arc<OpenStackEnv>>, ConfigError>;      // 目录缺失 → Ok(空)；任一文件坏 → 整份失败
pub fn load_config(sites_dir: &Path, envs_dir: &Path, env: &HashMap<String,String>)
    -> Result<Config, ConfigError>;
// envs_dir 缺失 ≡ 无环境（Ok(空)）；site 的 openstack_env 必须解析到已加载环境，否则 Validate 错误。
// env 参数供注入测试：生产传 std::env::vars 收集结果。

// src/routing.rs
pub struct RoutingTable { exact: HashMap<String, SiteKind>, openstack_patterns: Vec<OpenStackPattern> }
pub enum SiteKind { Forward(usize), OpenStack(usize) }               // 索引进 Config
pub struct OpenStackPattern { pub site_idx: usize, pub base_domain: String } // regex 编译后字段私有
pub enum RouteTarget { Forward(usize), OpenStack { site_idx: usize, instance: String, port: u16 } }
pub fn normalize_host(host: &str) -> String;                        // 小写、去尾点、去缺省端口(:80/:443)，非缺省端口保留
pub fn parse_instance_host(prefix_suffix: &str, base_domain: &str) -> Option<(String, u16)>;
pub impl RoutingTable { pub fn lookup(&self, host: &str) -> Option<RouteTarget>; }

// src/firewall.rs
pub struct FirewallTable { roles: HashMap<String, RoleRules> }      // 字段私有
pub struct RoleRules { pub allowed_ports: Vec<u16>,
                       pub whitelist: Vec<IpNet>, pub blacklist: Vec<IpNet> }
pub enum AclVerdict { Allow, Deny(AclReason) }
pub enum AclReason { NoRole, PortNotAllowed(u16), Blacklisted(IpAddr), NotWhitelisted(IpAddr) }
pub fn build_firewall(dir: &Path) -> Result<FirewallTable, ConfigError>;
impl FirewallTable { pub fn check(&self, instance: &str, port: u16, client: IpAddr) -> AclVerdict; }

// src/openstack/mod.rs — 注入缝（P7 单测用，OpenStackResolver 实现之）
#[async_trait]
pub trait Resolver { async fn resolve(&self, instance: &str) -> Result<ResolvedInstance, ResolveError>; }
// resolve(instance)：instance 可为 实例名 或 UUID。UUID（is_instance_uuid 判定）→ Nova show-by-id
//   GET /v2.1/servers/{uuid}；名称 → GET /v2.1/servers?name= + exact_name_match。
pub fn is_instance_uuid(s: &str) -> bool;                            // src/openstack/nova.rs 纯函数
pub fn decode_server(body: &[u8]) -> Result<NovaServer, NovaError>;  // 单实例 {"server":{...}} 应答
pub struct OpenStackResolver;                                        // Arc 持有：client/cache/单飞/熔断/last-known-good；实现 Resolver
pub struct ResolvedInstance { pub floating: Option<IpAddr>, pub fixed: Option<IpAddr> }
pub enum ResolveError { NotFound, AuthFailed, DiscoveryUnavailable(String), NoAddress }
impl OpenStackResolver {
    pub fn new(cfg: &OpenStackConfig, client: Arc<HttpClient>) -> Self;
    pub async fn resolve(&self, instance: &str) -> Result<ResolvedInstance, ResolveError>;
}
// src/openstack/auth.rs
pub struct Token { pub value: String, pub expires_at: DateTime<Utc>, pub nova_base: Url }
pub async fn fetch_token(cfg: &AuthConfig, api_url: &Url, client: &HttpClient) -> Result<Token, AuthError>;
// 调用点：fetch_token(&site.auth, &site.auth.api_url, client)
// src/openstack/nova.rs
pub fn decode_server_list(body: &[u8]) -> Result<Vec<NovaServer>, NovaError>;
pub fn exact_name_match<'a>(name: &str, servers: &'a [NovaServer]) -> Option<&'a NovaServer>;
pub fn select_address(s: &NovaServer) -> AddressSelection;           // floating 优先 → fixed → None

// src/lb.rs — 内部包装 Pingora LoadBalancer<T>；T 与 weight 字段形态以 P0 探针实测为准
pub enum LbEngine { RR(...), Random(...), Ketama(...) }              // 分别包 LoadBalancer<RoundRobin>/<Random>/<Consistent>
impl LbEngine {
    pub fn select(&self, hash_key: Option<IpAddr>) -> Option<SocketAddr>; // ketama 用客户端 IP 作 hash 键；其余忽略
    pub fn update(&self, backends: &[Backend]);                       // 委托底层 lb.update()（原子换表，保留共享 registry）
    // weight=0 后端必须完全排除：不进 Pingora 后端列表、不参与选择、不进入健康检查（P3 单测覆盖）。
}
// ⚠签名更新（FIX #2）：全部 forward 引擎共享一个 Arc<HealthRegistry>，由一个后台 HealthCheckService 驱动；
// ProbeRegistry 是注册表唯一的探针，按 backend 分派到站点级 probe 并各自遵守 interval_secs。
pub fn build_lb(backends: &[Backend], mode: &LbMode, registry: &Arc<HealthRegistry>) -> Arc<LbEngine>;
pub fn health_check_service(registry: Arc<HealthRegistry>) -> impl BackgroundService;
pub struct ProbeRegistry { /* 每个站点/后端的 probe + interval 记账 */ }
impl ProbeRegistry {
    pub fn new() -> Self;
    pub fn sync(&self, config: &Config);                              // 启动 + 每次成功 reload 后重建目标表（FIX #8：移除的站点不再探测）
}

// 站点后端池统一抽象（forward 站点二选一）：
pub enum ForwardBackends {
    Static(Arc<LbEngine>),                 // 全 addr 后端 → 现状 Pingora LB + 共享健康注册表路径
    Instance(Arc<InstancePool>),           // 含 ≥1 个 instance 后端 → 探测驱动选择
}
pub struct InstancePool { /* 私有：entries + 加权轮询游标 + 最近解析地址/健康位 + snapshot 引用 */ }
// PoolEntry = Static(SocketAddr, weight) | Instance { instance: String, port: u16, weight }
impl InstancePool {
    pub fn new(site_env: &str, backends: &[Backend], mode: &LbMode, snapshot: Arc<ArcSwap<Snapshot>>) -> Self;
        // site_env = 站点引用的 openstack 环境名（实例解析走 snapshot.resolvers[site_env]）
    pub fn select(&self, hash_key: Option<IpAddr>) -> Option<PoolPick>; // 仅健康+启用项；ketama 用客户端 IP
    pub fn update(&self, backends: &[Backend]);                        // 原子换 entries；健康位按 instance/addr 续用
}
// ⚠ketama 键 = 实例身份（名称串），绝不使用 resolved IP（IP 随重解析漂移 → 客户端重排）。
pub enum PoolPick { Static(SocketAddr), Instance { instance: String, port: u16 } }
// 实例健康：instance_health_service 单一 BackgroundService；**只负责 ForwardBackends::Instance 池**。
//   — site 无 health_check → 不设探针：池内所有启用项可选，实例故障在请求时表现为 502（与静态站点无探针语义一致）
//   — 逐 instance 后端：snapshot.resolvers[site_env].resolve(instance)（缓存命中）→ resolved IP → 按 health_check Tcp/Http 探活
//   — 解析失败/探活失败 → health.next(false)；成功 → next(true)；不可用项被 select 跳过；同池静态条目同路径直探
// ProbeRegistry::sync 必须跳过 ForwardBackends::Instance 站点（避免经 HealthRegistry 二次探测且健康库分叉）。
pub fn instance_health_service(snapshot: Arc<ArcSwap<Snapshot>>) -> impl BackgroundService;

// src/proxy.rs — forward 选择语义（instance 池）：
//   命中 PoolPick::Instance → 优先用 InstancePool 最近解析 addr 建 HttpPeer；无缓存则经该站 env Resolver 现场 resolve（走缓存）
//   解析失败统一映射 502（NotFound/NoAddress/AuthFailed/DiscoveryUnavailable——forward 站点是上游承诺，非实例存在性语义）
//   fail_to_connect 首次失败 → floating 回退 fixed 重试一次（复用 pick_upstream / should_fallback_to_fixed 语义）
pub struct Ctx { pub route: Option<RouteTarget>, pub upstream: Option<SocketAddr>,
                 pub site: Option<String>, pub instance: Option<String>,
                 pub snapshot: Option<Arc<Snapshot>>,                 // FIX #3：request_filter 取一次快照，后续 filter 复用
                 /* additive（forward instance 池）：本请求最后一次选择与解析现场——fail_to_connect
                    必须对【同一个 pick】回退 fixed 重试一次，不得重新 select（防换实例）；Ctx 每请求新建，无跨请求污染 */
                 pub pool_pick: Option<PoolPick>, pub resolved: Option<ResolvedInstance> }
pub struct IrisProxy { pub shared: Arc<SharedState> }                 // SharedState 见 src/state.rs
// select_backend(snapshot, site, client_ip) -> Option<PoolPick>（forward；Static→lb.select，Instance→pool.select）

// src/tls.rs — 证书映射：forward 站点(https) 进 exact；openstack 站点(https) 进 bases（需通配证书 *.base_domain）
pub struct SniCertMap { exact: HashMap<String, Arc<CertKey>>, bases: Vec<(String, Arc<CertKey>)> }
// ⚠签名更新（FIX #1）：SniCertSource 读取与 proxy/reload 同一个 ArcSwap<Snapshot>（无独立证书存储），
// 因此 reload 新增/轮换的证书在下次握手即生效。
pub struct SniCertSource { snapshot: Arc<ArcSwap<Snapshot>> }         // 实现 TlsAccept
impl SniCertSource {
    pub fn new(snapshot: Arc<ArcSwap<Snapshot>>) -> Self;
    pub fn lookup(&self, sni: &str) -> Option<Arc<CertKey>>;
    // certificate_callback：读取 snapshot.certs.lookup(sni)，未命中中断握手
}

// src/state.rs —— 单一原子快照（FIX #3）：SharedState 只持有一个 ArcSwap<Snapshot>。
// 快照内所有字段同为一次 store 提交，请求/握手/ reload 读同一代配置，杜绝半新状态。
pub struct Snapshot {
    pub config: Arc<Config>,
    pub routing: Arc<RoutingTable>,
    pub lbs: Arc<HashMap<String, Arc<ForwardBackends>>>,     // 键=forward 站点 name
    pub certs: Arc<SniCertMap>,
    pub firewall: Arc<HashMap<String, Arc<FirewallTable>>>,  // 键=openstack 站点 name；无 firewall_dir 的站点不出现（=不校验，仅 WARN）
    pub resolvers: Arc<HashMap<String, Arc<dyn Resolver>>>,  // 键=openstack 环境名（一环境一共享 resolver/缓存；openstack 站点与 forward instance 后端共用）
    pub registry: Arc<HealthRegistry>,                       // 跨 reload 共享，不是每次替换
    pub fingerprint: Arc<String>,
}
pub struct SharedState { /* private: snapshot: Arc<ArcSwap<Snapshot>>, probes: Arc<ProbeRegistry> */ }
impl SharedState {
    pub fn try_new(config: Config) -> Result<Self, ConfigError>;
    pub fn handle(&self) -> Arc<ArcSwap<Snapshot>>;          // 交给 SniCertSource 共享同一存储
    pub fn snapshot(&self) -> Arc<Snapshot>;
    pub fn store(&self, snapshot: Snapshot);                 // reload 的单一原子提交
    pub fn probes(&self) -> Arc<ProbeRegistry>;
    pub fn health_registry(&self) -> Arc<HealthRegistry>;
    pub fn config(&self) -> Arc<Config>;
    pub fn fingerprint(&self) -> Arc<String>;
    pub fn lb(&self, name: &str) -> Option<Arc<ForwardBackends>>;
    pub fn firewall_for(&self, name: &str) -> Option<Arc<FirewallTable>>;
    pub fn resolver_for(&self, env_name: &str) -> Option<Arc<dyn Resolver>>;   // 键=环境名
}
// src/reload.rs（P8）
pub fn apply_new_config(state: &SharedState, cfg: Config) -> Result<(), ReloadError>;
```

- 并发策略：整代配置经**单个** `ArcSwap<Snapshot>` 原子替换（FIX #3，含 lbs/resolvers/certs）；LB 成员热更走 `lb.update()`；`OpenStackResolver` 重建后整体替换；共享 `HealthRegistry` 跨 reload 不变；InstancePool 健康位按条目续用。
- `config_fingerprint` 覆盖凭据真实值（FIX #4 延续）：`openstack_envs` 的 password/secret 原始值经显式哈希（按环境名排序，见「指纹确定性」），凭据轮换即改指纹并被 `--watch-config` 应用。
- C2 采用**增量式**构造：新增 `build_env_resolvers(config) -> HashMap<env_name, Arc<dyn Resolver>>`（键=环境名，一环境一共享 resolver/缓存）；既有 `build_resolvers`（键=站点名）**保持签名**，其函数体在 **C1** 机械适配（`site.openstack` → `config.openstack_envs.get(&site.openstack_env)?.config()` 后 `OpenStackResolver::new`），公共构造逻辑复用；C3 才切换 state/reload 到 env 键并删除旧函数。
- reload 复用语义：forward 站点池四元比较 `backends`/`lb_mode`/`health_check`/`openstack_env` 相同 → 复用 `Arc<ForwardBackends>`（含 InstancePool 健康位；env 变更必重建）；resolver 按**环境**复用（同名前环境 `OpenStackConfig` 相等 → 保留缓存）。
- 指纹确定性（MUST-FIX）：`config_fingerprint` 不得 `format!("{config:?}")` 含 HashMap（迭代序不定 → 指纹抖动）；改为：`{config:?}` 中屏蔽 `openstack_envs`，并对每个环境按 **env-name 排序**依次哈希：该环境的 redacted `OpenStackConfig` Debug（覆盖 api_url/nova/timeout/cache_ttl 变化）＋ password/secret 原始值。TDD：同一配置两次加载（不同 HashMap 插入序）→ 指纹相等；仅改非凭据字段（api_url/cache_ttl）→ 指纹亦变，`--watch-config` 不回归（此前内嵌于站点文件时这些字段本就参与指纹）。
- P1 实现落定真实签名时，若与上述有出入，视为设计变更，必须回报工作流管理器后再改，不得静默偏离。
- 说明：(1) `src/state.rs` 由 P3 首创并逐步加字段，P4/P7 追加（纯增量）；(2) `LbEngine` 内层 Pingora 泛型、weight 字段、`update()` 可变性经 P0 探针（`03-p0-scaffold.md` §API 探针）实测后由 P3 落定，不视为改签。
