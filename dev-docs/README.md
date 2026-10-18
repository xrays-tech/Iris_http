# Iris HTTP — 技术设计文档（含实现与验证记录）

Rust + Pingora 的 HTTP/HTTPS 反向代理。需求来源：`propose.md`。实现已按 TDD 分阶段完成（P0–P8），并经受一次全量 code-review（发现并修复 8 项缺陷：原子快照、共享健康注册表、证书热重载、并发合并取消防护、认证超时等）；后续 P14 增加**独立 OpenStack 环境文件 + forward 后端按实例名/UUID**（见 `plan/14-forward-instance-backends.md`）。下述验证证据均为本仓库当前 HEAD（`60ca157`）实测输出。

## 设计文档

| 文档 | 内容 |
|---|---|
| `design.md` | 总体架构、核心组件、请求生命周期、模块结构、依赖基线、设计决策、可观测性 |
| `config.md` | 配置体系：站点两种模式 schema、负载均衡枚举、校验、域名匹配规则、secret 展开 |
| `discovery.md` | OpenStack 自动模式：Keystone 认证、Nova 查询、地址选择、1 天缓存、请求合并/熔断/fail-open |
| `firewall.md` | 防火墙规则：`INSTANCE.role` 格式、default-deny 判定顺序、白/黑名单语义 |
| `reload.md` | `reload` 命令热重载、生效机制、信号与优雅关闭/升级、运维联动 |
| `ops-deploy.md` | 部署与运维：systemd unit、证书零停机轮换、配置/防火墙变更、排障 |
| `plan/01-overview.md` | 开发计划入口（分步文件 `plan/03…13`、门禁 `plan/13-gates-risks.md`） |

阅读顺序：`design.md` → `config.md` → `discovery.md` → `firewall.md` → `reload.md`。

版本基线：Pingora 0.9.x（API 以 `cloudflare/pingora` main 2026-10 核对）；OpenStack 发现行为对齐 `xrays-tech/Iris_ssh`。

## 运行

```bash
cargo build
cargo run --bin iris -- run --config-dir <config-root>
```

- `<config-root>` 下必须含 `sites/` 子目录（默认 `--config-dir ./config`），每站点一个 `*.yaml`；OpenStack 环境文件位于 `<config-root>/openstack/<env-name>.yaml`（文件 stem = 环境名，被站点按 `openstack_env` 引用），openstack 站点文件不再内嵌 `openstack:` 块。示例见 `tests/fixtures/sites/` 与 `tests/fixtures/openstack/prod.yaml`。
- 监听：`0.0.0.0:80`（HTTP）、`0.0.0.0:443`（HTTPS/SNI/H2）。可用环境变量覆盖端口以便测试：
  `IRIS_HTTP_LISTEN_ADDR`（默认 `0.0.0.0:80`）、`IRIS_HTTP_LISTEN_TLS_ADDR`（默认 `0.0.0.0:443`）、`IRIS_HTTP_METRICS_ADDR`（默认 `127.0.0.1:9091`）。
- 指标与健康：metrics 端口提供 `/metrics`（prometheus，含 `config_reload_total{ok}`、`firewall_denied_total{reason}`）、`/healthz`、`/version`（`binary=<ver> config_hash=<hex>`）。
- 热重载：`iris reload --pidfile <pidfile>`（缺省 `/run/iris/iris.pid`）向运行进程发 SIGHUP；配置校验失败保持旧配置生效；`--watch-config` 为可选轮询监视（默认关）。
- 关闭：SIGTERM 优雅关闭；SIGQUIT/SIGINT 由 Pingora 处理（优雅升级/快速关闭）。

## 验证证据（HEAD 60ca157，2026-10-18）

- `cargo build`：0 warnings / 0 errors。
- `cargo test`：**360 passed / 0 failed**（lib 254、bin 7、config_load 71、firewall 11、openstack 17）。
- 端到端（可复现脚本，**8/8 PASS**）：
  - `scripts/e2e/03-forward.sh` — 轮询/加权/随机/ketama 分发、健康检查剔除与恢复、未知域名 404；
  - `scripts/e2e/04-tls.sh` — 按 SNI 选证书（CN/SAN）、ALPN h2、通配 `*.rent.do.top`、未知 SNI 握手失败、421、HTTP/2 200；
  - `scripts/e2e/07-openstack.sh` — mock Keystone/Nova 全流程：200/403（未开放端口、未白名单、黑名单、无 role）/404/502、floating 故障回退 fixed、token 复用 1 次（环境文件 form）；
  - `scripts/e2e/08-reload.sh` — 热重载增/改/删站点、坏配置保持旧配置、指纹变化、`/metrics`/`/healthz`/`/version`；
  - `scripts/e2e/09-health-reload.sh` — 重载后健康检查状态保留（引擎复用，健康后端不被误选）；
  - `scripts/e2e/10-tls-reload.sh` — reload 后新 HTTPS 站点的 SNI 证书即时生效（通配 `*.rent.do.top`）；
  - `scripts/e2e/11-health-reload-new-site.sh` — reload 新增带健康检查的站点即受探针门禁（503 后端不被选中，调参后恢复）；
  - `scripts/e2e/12-forward-instance.sh` — forward 实例后端（P14-C3）：加权 2:1 分发、name+UUID 混用（UUID 走 show-by-id）、token/缓存/请求合并、解析失败 502、TCP 健康剔除与恢复、热重载移除实例后端。
- 所有生产代码（`src/`）无 `unwrap()/expect()/panic!`（测试模块除外，已多次 grep 佐证）。

## 测试/冒烟工具

- `tests/stubs/upstream.py` — 可配端口/状态码/503/延迟/断连的 HTTP 上游桩。
- `scripts/mock_openstack.py` — Keystone v3 + Nova 2.1 契约 mock（记录请求头供 token 复用断言）。
- `scripts/gen-test-certs.sh` — 重新生成自签测试证书（`tests/fixtures/certs/`）。
