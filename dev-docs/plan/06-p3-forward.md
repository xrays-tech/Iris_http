# P3 — forward 转发链路 + 最小 Server 装配

- **依赖**：P2（可与 P5/P6 并行，写作用域互不重叠）。
- **文件范围**：`src/lb.rs`、`src/proxy.rs`（仅 forward 分支）、`src/main.rs`（最小装配）、`src/state.rs`（首创：`config`/`routing`/`lbs` 字段，见 `02-contracts.md`）。
- **设计参考**：`design.md` §4.3/§4.5、`config.md` §5；契约见 `02-contracts.md`。

## 目标
- `build_lb` → `Arc<LbEngine>`（`02-contracts.md`）：`LbMode` → Pingora selection 映射
  - `RoundRobin` → `LoadBalancer<RoundRobin>`（等权轮询）
  - `WeightedRoundRobin` → `LoadBalancer<RoundRobin>` + 权重（Pingora 加权轮询原生行为）
  - `Random` → `LoadBalancer<Random>`
  - `Ketama` → `LoadBalancer<Consistent>`（hash 键 = 客户端 IP，会话亲和）
  - 内层形态已由 **P0 API 探针**确认（见下节）；`pingora-load-balancing` 0.9 **无 `consistent-hashing` 特性**，`Consistent` 恒编译。
- `LbEngine::select(hash_key)`：委底层 `select(&self, key: &[u8], max_iterations)` —— ketama 用客户端 IP 字节作 key，RR/Random 忽略（P0 探针确认签名）。
- `LbEngine::update(backends)`：由配置**重建** Pingora LoadBalancer 并原子换内层 `ArcSwap`（不依赖 `LoadBalancer::update()` 的 discovery 语义；符合 reload.md『重建整体替换』）。
- `health_check_service`：`TcpHealthCheck` / `HttpHealthCheck` + `update_frequency` 周期（Pingora 自推进）；项目自有 `HealthState` 纯函数仅作代理侧观测用。
- `proxy.rs` **forward 分支**：`request_filter` —— `normalize_host` → `RoutingTable.lookup` → 未命中 404；`upstream_peer` —— `lb.select()` → `HttpPeer::new(addr, false, "")`，附 `PeerOptions`（连接超时/重试）；`fail_to_connect` 有限重试（默认 1 次）并记录指标；`response_filter` 注入标准安全头；`Ctx` 记录 route/site/upstream。
- `main.rs` 最小装配：`Server::bootstrap` + `http_proxy_service(&conf, app)` → `svc.add_tcp("0.0.0.0:80")`（**无 `Server::add_listener`**，P0 探针确认）→ `run_forever`；单 forward 站点可跑。

## P0 探针结论（P3 实现依据；`02-contracts.md` 契约不变）
- 选择即泛型：`LoadBalancer<S> where S: BackendSelection`；别名 `RoundRobin = Weighted<algorithms::RoundRobin>`、`Random = Weighted<algorithms::Random>`、`Consistent = consistent::KetamaHashing`。
- `Backend { addr: SocketAddr, weight: usize, ext: Extensions }`；构造 `Backend::new(addr)`（weight=1）与 `Backend::new_with_weight(addr, weight)`。
- `select(&self, key: &[u8], max_iterations: usize) -> Option<Backend>`。
- `LoadBalancer::update(&self) -> Result<()>`：async、无参、重跑 discovery——**不适用**静态 YAML 配置；本项目的热更路径是重建 LB 整体换内层。
- 监听：`http_proxy_service(&conf, app)` → `svc.add_tcp/add_tls_with_settings` → `server.add_service`；无 `Server::add_listener`。
- `background_service(name, task)` 两参；`BackgroundService::start(&self, shutdown: ShutdownWatch)`；`prometheus_http_service()` 可装配。

## TDD 单测清单
- `LbMode → selection` 映射（表驱动）。
- `Backend` 构建与 `addr:port` 解析、权重默认/归一；**`weight=0` 后端完全排除**（不进后端列表/健康检查）。
- 健康状态切换抽成项目自有纯函数（如 `HealthState::next(observed: bool)`）单测；Pingora `TcpHealthCheck` 内部不测。

## 端到端
- 本地两个 mock 后端（`python3 -m http.server` 或最小桩 `tests/stubs/upstream.py`），`curl -H "Host: www.example.com" http://127.0.0.1:8080/` 断言：
  - 轮询交替、加权比例、随机、ketama（同源同后端）；
  - 拔掉一个后端 → 健康检查剔除不再被选，恢复后回归。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. 上述 curl 断言通过，附命令与输出。
3. 不实现 openstack 分支 / TLS / reload。
