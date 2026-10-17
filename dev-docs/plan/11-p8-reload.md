# P8 — reload 与生命周期

- **依赖**：P7 完成。
- **文件范围**：`src/reload.rs`（`ReloadService`/`apply_new_config`）、`src/main.rs`（信号接线 + 指标服务）。`src/state.rs` 已在 P3 首创、P4(P7) 增字段，本阶段**不再改字段**，仅使用。
- **设计参考**：`reload.md` 全部、`design.md` §4.8/§9。

## 目标
- `SharedState`（`02-contracts.md`；config/routing/lbs/certs/firewall/resolvers 全部字段已由前序阶段创建，本阶段只使用）。
- `ReloadService` 实现 `BackgroundService`：
  - ✅P0 探针已确认：Pingora `run_forever` 只注册 SIGQUIT/SIGTERM/SIGINT，**SIGHUP 未被接管**（缺省动作终止进程，实测 `iris reload` 后 wait status 129）。
  - 监听来源：`SIGHUP`（`iris reload` 发；本阶段**必须**自建 tokio `SignalKind::hangup()` 任务触发重载，不与 Pingora 冲突）与 `--watch-config` 时的目录变化（`notify`，debounce）；
  - 触发 `apply_new_config(state, cfg)`：`load_config` 校验 → **失败保持旧配置**（日志 + 指标）→ 成功则：
    - forward LB：后端集合变化 → `lb.update()`；算法变化 → 重建 LB 整体替换；
    - `ArcSwap` 原子换 routing/certs/firewall；
    - openstack resolver 重建后整体替换（进程内缓存保留，避免重载击穿）；
    - 更新 `/version` 配置指纹（内容 hash）。
- `main.rs`：`SIGHUP`→重载、`SIGTERM`→优雅关闭、`SIGQUIT`→升级预留；后台服务统一 `shutdown.changed()` 收敛；`pingora_prometheus::prometheus_http_service()` 挂独立端口（新增 `--metrics-listen`，默认 `127.0.0.1:9091`，可配——本机 9090 被占）；`/version` 返回 `{binary, config_hash}`。

## TDD 单测清单
- `apply_new_config`：成功切换（新配置生效）、校验失败保持旧态（断言旧 `config_hash` 不变）、幂等重载。
- 配置指纹：内容 hash 计算稳定。
- resolver 重建：同实例 key 缓存保留断言（对外行为）。

## 端到端
- 运行中 `kill -HUP`：新站点立即可路由、删除的站点 404；
- 坏配置重载 → 旧配置继续服务（`/version` hash 未变、日志有 `config_reload_total{ok=false}`）；
- 慢响应/WebSocket 跨重载不断（`curl` 慢响应期间重载）；
- `SIGTERM` 优雅关闭：在途请求完成，退出码 0；
- `GET /version` 与 `GET /metrics` 可访问。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. 全部端到端断言通过，附命令与输出。

---

## P8 实现记录（落地注解，不改契约）

**交付文件**：`src/reload.rs`（新增）、`src/state.rs`（追加 `fingerprint` 字段）、`src/main.rs`（信号接线 + 指标/版本服务）、`src/proxy.rs`（`firewall_denied_total{reason}` 接线）、`scripts/e2e/08-reload.sh`（新增）。

### `apply_new_config(state, cfg)`（`src/reload.rs`）
- **先建后换**：构建顺序 `routing → firewall → certificates → lbs → resolvers`；任一构建失败即返回 `ReloadError::Build{component, source}`，**不动任何已发布字段**（老配置继续服务）。全部成功后逐字段 `ArcSwap::store`（每字段原子，跨字段非原子；在途请求持有旧 `Arc` 快照，不存在半新状态）。
- **resolver 缓存保留**：逐站点比较新旧 `OpenStackConfig`；相同者复用旧 `Arc<dyn Resolver>`（进程内 moka 缓存/熔断/last-known-good 全部保留），仅对变更站点 `OpenStackResolver::new` 重建。`src/openstack/*` 签名未动。
- **LbEngine map 重建**：对每个 forward 站点 `build_lb(&backends, &lb_mode)` 新建后整表 `store`（对齐本阶段"重建 LB 映射"的钉定语义）。**已知局限**：P3 的健康检查后台服务在启动时持有初始 `Arc<LbEngine>`，其 `LbSlot::run` 在启动时载入一次 inner；`build_lb` 新引擎不带探针，故重载后 forward 健康探测不会自动指向新池（P3 `update()` 语义亦然）。P8 门禁未覆盖该场景，未在 `src/lb.rs` 内改动（超出本阶段独占文件范围）。
- **指纹**：`config_fingerprint(&Config)` = 配置规范 `Debug` 渲染的 64-bit `DefaultHasher` 十六进制（稳定、内容敏感；`Debug` 已对凭据脱敏，故密钥轮换不改指纹——有意如此，避免 `/version` 泄露或噪声）。`state.fingerprint: ArcSwap<String>` 为 P8 追加字段（纯增量）。

### `ReloadService`（`BackgroundService`）
- `start(&self, mut shutdown)`：`tokio::select!` 三分支——(a) tokio `signal(SignalKind::hangup())`（P0 探针确认 Pingora `run_forever` 不接管 SIGHUP）；(b) `--watch-config` 时的 2s 轮询重扫（指纹相同则跳过，非 `notify` 事件式——保持单一后台任务、可测）；(c) `shutdown.changed()` → break。
- `reload_once(force)`：`load_config` 失败 → 日志 + `config_reload_total{ok="false"}`，保持旧配置；成功 → `apply_new_config` → `config_reload_total{ok="true"}` + 日志前后指纹。SIGTERM 优雅路径（P0）未改，SIGQUIT/SIGINT 交由 Pingora。

### 指标与 `/version`（`src/main.rs`）
- 新增 `--metrics-listen`（默认 `127.0.0.1:9091`）与 `IRIS_HTTP_METRICS_ADDR` 覆盖（环境变量优先于 CLI）。`VersionMetricsProxy`（独立 `ProxyHttp`）经 `add_tcp(metrics_addr)` 服务：`GET /version` → `binary=<ver> config_hash=<fp>` 文本；`GET /metrics` → `pingora_prometheus::prometheus::gather()` + `TextEncoder`（Prometheus 文本）；`GET /healthz` → `ok`；其余 404。
- 计数器 `config_reload_total{ok}`、`firewall_denied_total{reason}` 注册于默认 registry；`init_metrics()` 在启动时**预置 0 值标签序列**（`MetricVec` 未观测的 label 不出现在 exposition，预置保证 `/metrics` 启动即含 `config_reload_total`）。`firewall_denied_total` 在 `openstack_gate` 的 403 路径增量（`no_role|port_not_allowed|blacklisted|not_whitelisted|no_table|no_client`），由 `firewall_denial_reason()` 与 `firewall_verdict_to_http()` 同序决策，二者不会分歧。

### 契约/字段变更（回报项）
- `src/lib.rs` **被修改**（超出"仅必要"的默认约束，属必要）：新增 `pub mod reload;` 与 `RunArgs::metrics_listen: Option<String>`（`--metrics-listen`）。`pub fn reload`（CLI）与 `pub mod reload` 同名共存于不同命名空间，无冲突。
- `SharedState` 追加 `fingerprint: ArcSwap<String>`（纯增量，P8 拥有）。
- 无新依赖；`bytes` 通过 `into()` 类型推断构造，未引入 `bytes` 直接依赖。

### 测试
- 单测：`reload.rs` 11 项（成功切换/构建失败全字段不变(Arc 指针比较)/幂等/指纹稳定与内容敏感/resolver 复用与重建/`reload_once` 成功与坏配置/ watch 无变更跳过/关闭收敛），`state.rs` +2（指纹稳定/内容敏感），`proxy.rs` +1（denial 标签与判决一致），`lib.rs` +1（`--metrics-listen` 解析）。
- 全量 `cargo test`：**273 passed / 0 failed**（6 suites：lib 196、bin 7、config_load 49、firewall 11、openstack 10）。`cargo build` 0 warning。
- 端到端 `scripts/e2e/08-reload.sh`：A 基线（alpha/openstack 200、`/healthz` ok、`/metrics` 可达含 `config_reload_total`）→ B（加 `beta.test` + 改 openstack 站点）reload → 新旧全 200、指纹 A→B 改变、`{ok="true"} 1` → C（坏 YAML）reload → 指纹不变、`{ok="false"} 1`、`beta.test` 仍 200 → D（删 `alpha.test`）reload → `alpha.test` 404、`beta.test` 200、`{ok="true"} 2`。
- 回归：`03-forward.sh`、`04-tls.sh`、`07-openstack.sh` 全绿。
- **WebSocket 跨重载**：本阶段未做（可选）。原因：需额外 WS 客户端且 Pingora 升级语义与 P8 热重载正交；已在门禁范围外，未添加 `tests/stubs/` 客户端。

