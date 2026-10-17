# Iris HTTP — 模块接口契约（跨阶段固定）

> 由 **P1** 落定为 `src/` 内真实类型；P3/P5/P6/P7/P8 只实现、不改签名。
> 这是并行 lanes 不冲突的根基：各文件的归属见 `01-overview.md` 阶段表。

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
pub struct AuthConfig { pub api_url: Url, pub credentials: AuthCredentials }  // api_url = Keystone v3 基地址 https://...:5000/v3
pub enum AuthCredentials { Password{ username: String, password: String, project_name: String,
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

// src/proxy.rs
pub struct Ctx { pub route: Option<RouteTarget>, pub upstream: Option<SocketAddr>,
                 pub site: Option<String>, pub instance: Option<String>,
                 pub snapshot: Option<Arc<Snapshot>> }                // FIX #3：request_filter 取一次快照，后续 filter 复用
pub struct IrisProxy { pub shared: Arc<SharedState> }                 // SharedState 见 src/state.rs

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
    pub lbs: Arc<HashMap<String, Arc<LbEngine>>>,
    pub certs: Arc<SniCertMap>,
    pub firewall: Arc<HashMap<String, Arc<FirewallTable>>>,  // 键=openstack 站点 name；无 firewall_dir 的站点不出现（=不校验，仅 WARN）
    pub resolvers: Arc<HashMap<String, Arc<dyn Resolver>>>,
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
    pub fn lb(&self, name: &str) -> Option<Arc<LbEngine>>;
    pub fn firewall_for(&self, name: &str) -> Option<Arc<FirewallTable>>;
    pub fn resolver_for(&self, name: &str) -> Option<Arc<dyn Resolver>>;
}
// src/reload.rs（P8）
pub fn apply_new_config(state: &SharedState, cfg: Config) -> Result<(), ReloadError>;
```

- 并发策略：整代配置经**单个** `ArcSwap<Snapshot>` 原子替换（FIX #3，含 lbs/resolvers/certs）；LB 成员热更走 `lb.update()`；`OpenStackResolver` 重建后整体替换；共享 `HealthRegistry` 跨 reload 不变。
- `config_fingerprint` 覆盖凭据真实值（FIX #4）：仅凭据轮换也会改变 `/version` 指纹，从而被 `--watch-config` 应用。
- P1 实现落定真实签名时，若与上述有出入，视为设计变更，必须回报工作流管理器后再改，不得静默偏离。
- 说明：(1) `src/state.rs` 由 P3 首创并逐步加字段，P4/P7 追加（纯增量）；(2) `LbEngine` 内层 Pingora 泛型、weight 字段、`update()` 可变性经 P0 探针（`03-p0-scaffold.md` §API 探针）实测后由 P3 落定，不视为改签。
