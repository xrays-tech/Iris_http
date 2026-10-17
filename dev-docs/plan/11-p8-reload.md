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
