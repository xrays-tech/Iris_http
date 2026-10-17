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
pub struct FirewallTable { roles: HashMap<String, RoleRules> }      // 字段私有
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
pub struct IrisProxy { pub shared: Arc<SharedState> }                 // SharedState 见 P8

// src/tls.rs
pub struct SniCertSource { certs: ArcSwap<HashMap<String, Arc<CertKey>>> }   // 实现 TlsAccept
impl SniCertSource { pub fn from_sites(sites: &[&ForwardSite], base: &Path) -> Result<Self>; }

// src/state.rs / src/reload.rs（P8）
pub struct SharedState { pub config: ArcSwap<Config>, pub routing: ArcSwap<RoutingTable>,
                         pub certs: ArcSwap<HashMap<String, Arc<CertKey>>>,
                         pub firewall: ArcSwap<FirewallTable>,
                         pub lbs: Arc<HashMap<String, Arc<LoadBalancer>>>,
                         pub resolvers: Arc<HashMap<String, Arc<OpenStackResolver>>> }
pub fn apply_new_config(state: &SharedState, cfg: Config) -> Result<(), ReloadError>;
```

- 并发策略：所有共享快照经 `ArcSwap`；LB 集合热更走 `lb.update()`；`OpenStackResolver` 重建后整体替换。
- P1 实现落定真实签名时，若与上述有出入，视为设计变更，必须回报工作流管理器后再改，不得静默偏离。
