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
1. load_config(sites_dir, envs_dir)            // config.md §6：解析站点 + 环境文件，全量校验（纯函数）
2. 失败? → 打日志 + 指标 config_reload_total{ok=false}；保持旧配置继续服务，进程不退出
   成功? ↓
3. 构建一代新 Snapshot（config、路由表、LB 引用、证书表、firewall 表、resolver 表、指纹）；
   共享 HealthRegistry 沿用旧值，不重建；未变的 forward 池与环境 resolver 复用旧 Arc
4. 全部构建成功后：
   snapshot.store(Arc::new(new Snapshot))      // 唯一原子切换点（config+routing+certs+firewall+lbs+resolvers 一次发布）
5. 重算配置指纹（hash）供 /version 与告警对比（按环境名排序逐环境哈希，覆盖凭据真实值，凭据轮换也能被感知）
6. 指标 config_reload_total{ok=true}
```

- **校验先行**：任何站点文件语法/语义错误都拒绝整份新配置（部分生效会留下无法回溯的混合状态）。
- **单一快照原子切换**（FIX #3）：`SharedState` 只持有一个 `ArcSwap<Snapshot>`，请求路径在 `request_filter` 取一次快照并在后续 filter 复用；因此**不存在半新状态**。

## 3. 各类配置变更的生效机制

| 变更 | 生效点 | 机制 |
|---|---|---|
| forward 后端增删/权重变化 | 对应站点池 `ForwardBackends` | **四元比较**（`backends`/`lb_mode`/`health_check`/`openstack_env`）未变则复用同一 `Arc<ForwardBackends>`；变了则经 `build_forward_backends` 重建。`Static` 池共享同一 `HealthRegistry`（观察到的健康沿用）；`Instance` 池复用即保留解析缓存与最近解析 |
| 负载均衡算法变化 | 对应站点池 | 算法在四元内 → 触发重建（换引擎），随快照发布；`Static` 探针与已观察健康仍在共享表内 |
| forward `openstack_env` 变化 | 对应站点池 | 在四元内 → **必须重建**（同一实例改由另一环境解析），旧解析缓存不跨环境沿用 |
| 站点新增/删除/域名变化 | 路由表 | 随单一快照发布；新请求即见新表 |
| openstack 环境参数（auth/api/cache_ttl/timeout 等） | 环境 resolver（键=环境名） | 新旧该环境的 `OpenStackConfig` 相等则复用同一 `Arc<dyn Resolver>`；**进程内缓存保留**（同实例 key 继续命中，避免重载打冲击穿）。不等则重建。**一个环境重建，所有引用它的站点（openstack 站点 + forward instance 池）同时改用新 resolver** |
| `firewall_dir`/role 文件变化 | 防火墙表 | 重载时重读全部 `.role`；随快照发布 |
| 证书/密钥变化 | SNI 回调 | `SniCertSource` 读取与代理/reload **同一个** `ArcSwap<Snapshot>` 的 `certs`；新握手即用新证书，旧握手不受影响（FIX #1） |
| 健康检查规格/间隔变化 | 共享探针 | `ProbeRegistry.sync(&config)` 按新配置重指向站点级 probe 并重置其 interval 计时；恢复/剔除在下次探测生效（FIX #2） |
| forward 站点被移除 | 共享探针 + 共享 registry | `ProbeRegistry.sync` 不再为其建表；引擎被释放时其 `Backends` view 移除，registry 注销其后端（FIX #8） |
| `cache_ttl_secs` 等（环境文件） | 环境 resolver | 该环境 `OpenStackConfig` 变更 → resolver 重建，新缓存参数对新条目生效；存量条目照旧 TTL |
| 环境文件新增/删除 | 环境 resolver 表 | 新增 → 建 resolver；删除 → 其引用站点/池在校验阶段即失败（`openstack_env` 不存在），整份重载被拒 |

实现要点：
- `certificate_callback(&self)` 只读 `&self`（Trait 签名）；证书表不再有独立 store，回调读取与代理共享的 `Snapshot.certs`（FIX #1）。
- 共享 `HealthRegistry` 由启动时注册的**单个** `HealthCheckService` 驱动；`ProbeRegistry` 是注册表唯一的探针，按后端分派到站点级 probe 并各自遵守 `interval_secs`。新增/变更站点的引擎自动向该共享 registry 贡献后端，无需重挂探针（FIX #2）。
- 加载列表外的站点（reload 移除的 forward 站点）不再被探测；其引擎的 view 随最后一个 `Arc<LbEngine>` 释放而注销（FIX #8）。
- `Instance` 池由独立的 `instance_health_service` 探测（`ProbeRegistry.sync` 跳过含 instance 后端的站点），其探针循环读的是当前快照；池被复用/重建都随快照原子可见。
- 指纹按**环境名排序**逐个哈希（`{config:?}` 已把 `openstack_envs` 屏蔽为数量），因此环境的 HashMap 插入序不影响指纹；仅凭据轮换也会改变指纹（FIX #4 的扩展）。

## 4. 并发与一致性

- 整代配置打包为单一 `Snapshot`（config、routing、cert map、firewall、LB 引用、resolver 表、指纹），经**一个** `ArcSwap<Snapshot>` 原子发布（FIX #3）；**读路径无锁**，写路径单点（重载后台服务）。共享 `HealthRegistry`（及 `ProbeRegistry`）跨代不变。
- 重载不在请求处理路径中加锁；`request_filter` 每请求只取一次快照引用，`upstream_peer`/`fail_to_connect`/`openstack_gate`/421 校验全部复用该引用（`load()` 开销 O(1)）。
- 高并发重载（连发多次 SIGHUP）：重载服务串行处理（channel/mutex），以最后一次有效校验结果为准，先建后换保证幂等。
- 重载进行中允许的新请求天然落在"发布前旧"或"发布后新"，**不存在半新状态**（单一原子 store）。

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

- 单元：重载流程状态机（成功切换/校验失败保持旧配置）、配置指纹计算（含仅凭据轮换变化与按环境名排序确定性，FIX #4）、幂等重载、单一快照原子性、共享 registry 注册（FIX #2）、forward 池四元复用（未变复用 / `openstack_env` 变化重建）、环境 resolver 按名键复用（未变复用 / 环境配置变化重建）。
- 集成（组合 mock Nova + 双后端模拟上游）：
  1. 重载新增站点 → 新 Host 立即路由成功；旧站点在途 WebSocket 连接不断。
  2. 重载后端集合 → `lb.update` 生效；在途请求不 5xx。
  3. 证书更换/新增 → 新握手拿到新证书（`openssl s_client -servername` 验证）；旧连接持续到自然结束。E2E：`scripts/e2e/10-tls-reload.sh`（FIX #1，重载前断言握手中断、重载后断言新证书 CN/SAN）。
  4. 坏配置重载 → 返回错误日志，进程继续服务旧配置。
  5. `kill -HUP` 后 `/version` 的 config_hash 变化。
  6. 重载新增带健康检查的站点 → 其后端立即被共享探针门控（不健康后端永不被选中）；重载改健康检查间隔 → 重新探测。E2E：`scripts/e2e/11-health-reload-new-site.sh`（FIX #2）；`scripts/e2e/09-health-reload.sh`（不变站点引擎复用，健康状态跨重载保留）。
  7. 重载 swap forward instance 后端集 → 被移除的实例后端不再被选中。E2E：`scripts/e2e/12-forward-instance.sh`（P14-C3）。
- 全部走 `cargo build` + `cargo test` + `scripts/e2e/*.sh` 闭环。
