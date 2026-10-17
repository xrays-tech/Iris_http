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
  - 内层泛型与 weight 字段形态以 **P0 API 探针**实测为准；如 `Consistent` 需特性则开启 `consistent-hashing`。
- `LbEngine::select(hash_key)` / `LbEngine::update(backends)`：委底层 `lb.select()` / `lb.update()`（原子换表，在途请求不受影响）。
- `health_check_service`：`TcpHealthCheck` / `HttpHealthCheck` + `lb.update_frequency` 周期；`build_lb` 返回的 LB 可通过 `update()` 热更。
- `proxy.rs` **forward 分支**：`request_filter` —— `normalize_host` → `RoutingTable.lookup` → 未命中 404；`upstream_peer` —— `lb.select()` → `HttpPeer::new(addr, false, "")`，附 `PeerOptions`（连接超时/重试）；`fail_to_connect` 有限重试（默认 1 次）并记录指标；`response_filter` 注入标准安全头；`Ctx` 记录 route/site/upstream。
- `main.rs` 最小装配：`Server::bootstrap` + `0.0.0.0:80` 监听（`add_listener`）+ 单 forward 站点可跑；`run_forever`。

## TDD 单测清单
- `LbMode → selection` 映射（表驱动）。
- `Backend` 构建与 `addr:port` 解析、权重默认/归一。
- 健康状态切换抽成项目自有纯函数（如 `HealthState::next(observed: bool)`）单测；Pingora `TcpHealthCheck` 内部不测。

## 端到端
- 本地两个 mock 后端（`python3 -m http.server` 或最小桩 `tests/stubs/upstream.py`），`curl -H "Host: www.example.com" http://127.0.0.1:8080/` 断言：
  - 轮询交替、加权比例、随机、ketama（同源同后端）；
  - 拔掉一个后端 → 健康检查剔除不再被选，恢复后回归。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. 上述 curl 断言通过，附命令与输出。
3. 不实现 openstack 分支 / TLS / reload。
