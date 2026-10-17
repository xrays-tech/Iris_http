# Iris HTTP 反向代理 — 动态重载与进程生命周期设计

> 依据：`propose.md`（"支持 reload 命令，可以动态重新加载配置"）。
> 机制基于 Pingora 0.9 的信号/后台服务模型（见 `design.md` §4.1、§4.8）。

## 1. 重载入口

| 方式 | 触发 | 语义 |
|---|---|---|
| `iris reload` | CLI 子命令：向运行实例的 pidfile 对应进程发 `SIGHUP` | 进程内热重载（推荐、无感知） |
| 信号 `SIGHUP` | 直接 `kill -HUP <pid>`（pidfile：默认 `/run/iris/iris.pid` 或 `--pidfile`） | 同上 |
| 配置监视（可选） | 后台服务用 `notify` 监听 `sites/` 目录变化，`debounce` 后自动重载 | 默认关闭（`--watch-config` 开启），避免开发/运维预期外的配置漂移 |

- **`reload` 只重载配置，不重启进程、不重开监听器、不断既有连接。**
- 进程级替换（二进制升级）走 Pingora 优雅升级：`--upgrade` + `SIGQUIT`（见 §5）。

## 2. 重载执行流程（原子切换）

后台服务（或 SIGHUP 处理）执行：

```
1. load_config(sites_dir)                      // config.md §6：解析 + 全量校验（纯函数）
2. 失败? → 打日志 + 指标 config_reload_total{ok=false}；保持旧配置继续服务，进程不退出
   成功? ↓
3. 构造新 Config 快照（含路由表、LB 描述、证书表、firewall 表、openstack 客户配置）
4. 逐站点应用（见 §3），全部就绪后：
   arcswap_config.store(Arc::new(new_config))  // 路由/证书/firewall 的原子切换点
5. 更新配置指纹（hash）供 /version 与告警对比
6. 指标 config_reload_total{ok=true}
```

- **校验先行**：任何站点文件语法/语义错误都拒绝整份新配置（部分生效会留下无法回溯的混合状态）。
- 切换点尽量收敛到一次 `ArcSwap::store`（路由表 + 证书表 + firewall 表可打包进同一 `Config` 结构，或各自 `ArcSwap` 但**先构建后发布**，杜绝读到半新状态）。

## 3. 各类配置变更的生效机制

| 变更 | 生效点 | 机制 |
|---|---|---|
| forward 后端增删/权重变化 | 对应站点 `LoadBalancer` | `lb.update(backends)`：原子替换 selector 表，在途请求继续用旧迭代器，不受影响（Pingora 文档语义） |
| 负载均衡算法变化 | 对应站点 LB | 重建 `LoadBalancer` 并整体 `ArcSwap` 替换引用（不再有请求指向旧实例时回收） |
| 站点新增/删除/域名变化 | 路由表 | `ArcSwap<RoutingTable>` store；新请求即见新表 |
| openstack 站点参数（auth/api 等） | 站点解析器 | 重建解析器并 `ArcSwap`；**进程内缓存保留**（同实例 key 继续命中，避免重载打冲击穿） |
| `firewall_dir`/role 文件变化 | 防火墙表 | 重载时重读全部 `.role`；随路由快照发布 |
| 证书/密钥变化 | SNI 回调 | `TlsAccept.certificate_callback` 每握手读 `ArcSwap<CertMap>` 当前快照；新握手即用新证书，旧握手不受影响 |
| `cache_ttl_secs` 等 | 缓存参数 | 解析器重建时对新缓存条目生效；存量条目照旧 TTL |

实现要点：`certificate_callback(&self)` 只读 `&self`（Trait 签名），证书表用 `ArcSwap` 存于回调实例内，实现"外部 store、回调只读"。

## 4. 并发与一致性

- 所有共享快照（config、routing、cert map、firewall、LB 引用、last-known-good）为 `Arc`/`ArcSwap`；**读路径无锁**，写路径单点（重载后台服务）。
- 重载不在请求处理路径中加锁；`upstream_peer`/`request_filter` 每请求最多再取一次快照引用（`load()` 开销 O(1)）。
- 高并发重载（连发多次 SIGHUP）：重载服务串行处理（channel/mutex），以最后一次有效校验结果为准，先建后换保证幂等。
- 重载进行中允许的新请求天然落在"发布前旧"或"发布后新"，**不存在半新状态**（原子 store）。

## 5. 生命周期与信号约定（对齐 Pingora）

| 信号 | 行为 |
|---|---|
| SIGHUP | 配置热重载（§2） |
| SIGTERM | **优雅关闭**：广播 `ShutdownWatch` → 各 Service 与后台服务停止接受新请求 → 排干在途请求（`grace_period_seconds`），超时强制（`graceful_shutdown_timeout_seconds`）→ 运行时 `shutdown_timeout` 回收 |
| SIGINT | 快速关闭（立即终止，不排干） |
| SIGQUIT | **优雅升级**：`iris --upgrade --config-dir ...` 启动新进程（同一 `upgrade_sock`）→ 对旧进程 SIGQUIT → 旧进程把监听 FD 移交 → 新进程 `bootstrap` 接管 → 旧进程排干后退出。静态资源与新二进制滚动，配置另走 `reload` |
| SIGUSR1 | （预留）`daemon_wait_for_ready` 就绪信号，配合 systemd |

后台服务统一实现 `BackgroundService`：`start(&self, shutdown: ShutdownWatch)` 内 `tokio::select!` 监听 `shutdown.changed()`，保证优雅关闭不残留后台任务。

## 6. 运维联动

- **启动**：`--config-dir` 必须显式（或默认目录存在）；配置加载失败 → 启动失败并给出文件定位错误（不静默降级）。
- **版本/指纹**：`PingoraServer` 暴露 `GET /version` → JSON `{ binary: ..., config_hash: ... }`；脚本在 `reload` 后核对 hash 变化以确认生效。
- **指标**：`config_reload_total{ok}`、`config_reload_duration_seconds`、`firewall_denied_total{reason}`、`openstack_cache_hit/miss`、`openstack_circuit_open`；挂到 `prometheus_http_service()`。
- **systemd**：`ExecStart=iris run --config-dir /etc/iris`、`ExecReload=/bin/kill -HUP $MAINPID`、`KillSignal=SIGTERM`；`reload` 即 systemd reload。
- **审计**：每次重载成功/失败打 `config_reload` 事件（含 config_hash 前后值），便于回滚排查。

## 7. 测试策略（reload 专项）

- 单元：重载流程状态机（成功切换/校验失败保持旧配置）、配置指纹计算、幂等重载。
- 集成（组合 mock Nova + 双后端模拟上游）：
  1. 重载新增站点 → 新 Host 立即路由成功；旧站点在途 WebSocket 连接不断。
  2. 重载后端集合 → `lb.update` 生效；在途请求不 5xx。
  3. 证书更换 → 新握手拿到新证书（`openssl s_client -servername` 验证）；旧连接持续到自然结束。
  4. 坏配置重载 → 返回错误日志，进程继续服务旧配置。
  5. `kill -HUP` 后 `/version` 的 config_hash 变化。
- 全部走 `cargo build` + `cargo test` 闭环。
