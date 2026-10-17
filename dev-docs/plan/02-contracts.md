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
// src/openstack/nova.rs
pub fn decode_server_list(body: &[u8]) -> Result<Vec<NovaServer>, NovaError>;
pub fn exact_name_match<'a>(name: &str, servers: &'a [NovaServer]) -> Option<&'a NovaServer>;
pub fn select_address(s: &NovaServer) -> AddressSelection;           // floating 优先 → fixed → None

// src/lb.rs — 内部包装 Pingora LoadBalancer<T>；T 与 weight 字段形态以 P0 探针实测为准
pub enum LbEngine { RR(...), Random(...), Ketama(...) }              // 分别包 LoadBalancer<RoundRobin>/<Random>/<Consistent>
impl LbEngine {
    pub fn select(&self, hash_key: Option<IpAddr>) -> Option<SocketAddr>; // ketama 用客户端 IP 作 hash 键；其余忽略
    pub fn update(&self, backends: &[Backend]);                       // 委托底层 lb.update()（原子换表）
    // ⚠更新：若 P0 探针确认 update() 需 &mut self，则以内部锁封装实现 &self 语义；超出该前提走设计变更回报，不得静默改签。
    // weight=0 后端必须完全排除：不进 Pingora 后端列表、不参与选择、不进入健康检查（P3 单测覆盖）。
}
pub fn build_lb(backends: &[Backend], mode: &LbMode) -> Arc<LbEngine>;
pub fn health_check_service(lb: Arc<LbEngine>, hc: &HealthCheck) -> impl BackgroundService;

// src/proxy.rs
pub struct Ctx { pub route: Option<RouteTarget>, pub upstream: Option<SocketAddr>,
                 pub site: Option<String>, pub instance: Option<String> }
pub struct IrisProxy { pub shared: Arc<SharedState> }                 // SharedState 见 src/state.rs（P3 首创、逐阶段加字段）

// src/tls.rs — 证书映射：forward 站点(https) 进 exact；openstack 站点(https) 进 bases（需通配证书 *.base_domain）
pub struct SniCertMap { exact: HashMap<String, Arc<CertKey>>, bases: Vec<(String, Arc<CertKey>)> }
pub struct SniCertSource { certs: ArcSwap<SniCertMap> }              // 实现 TlsAccept
impl SniCertSource {
    pub fn from_sites(config: &Config) -> Result<Self>;
    // lookup(sni)：精确命中 exact → 否则后缀匹配最长 base_domain（取该站点证书）→ 均未命中中断握手
}

// src/state.rs —— 首创于 P3（config/routing/lbs）；字段由各阶段增量添加 = 纯增量，不视为接口变更
//   P4 加 certs；P7 加 firewall/resolvers
pub struct SharedState { pub config: ArcSwap<Config>, pub routing: ArcSwap<RoutingTable>,
                         pub certs: ArcSwap<SniCertMap>,
                         pub firewall: ArcSwap<FirewallTable>,
                         pub lbs: ArcSwap<HashMap<String, Arc<LbEngine>>>,
                         pub resolvers: ArcSwap<HashMap<String, Arc<dyn Resolver>>> }
// src/reload.rs（P8）
pub fn apply_new_config(state: &SharedState, cfg: Config) -> Result<(), ReloadError>;
```

- 并发策略：所有共享快照经 `ArcSwap` 全量替换（含 lbs/resolvers）；LB 集合热更走 `lb.update()`；`OpenStackResolver` 重建后整体替换。
- P1 实现落定真实签名时，若与上述有出入，视为设计变更，必须回报工作流管理器后再改，不得静默偏离。
- 说明：(1) `src/state.rs` 由 P3 首创并逐步加字段，P4/P7 追加（纯增量）；(2) `LbEngine` 内层 Pingora 泛型、weight 字段、`update()` 可变性经 P0 探针（`03-p0-scaffold.md` §API 探针）实测后由 P3 落定，不视为改签。
