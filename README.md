# Iris HTTP

基于 **Rust + Pingora** 的 HTTP/HTTPS 反向代理，支持精确域名的基本转发与 OpenStack 全自动实例发现两种站点模式，配置热重载、按 SNI 的多站点 TLS（含 HTTP/2）与进程内防火墙。

> 状态：**已实现并验证**（P0–P8 分阶段 TDD，含一次全量 code-review 与修复）。`cargo test` **290 passed**，7 个端到端脚本全部通过。详见下方「验证证据」。

## 功能

- **两种站点模式**
  - **forward**：精确域名匹配 → 后端 `IP:port` 列表（可加权）→ 负载均衡
    （`round_robin` / `weighted_round_robin` / `random` / `ketama`），支持 TCP/HTTP 健康检查，宕机后端自动剔除。
  - **openstack**：全自动发现。域名形如 `{instance}-{port}.base_domain`（如 `INS001-80.rent.do.top`），
    经 Keystone v3 认证 + Nova 2.1 全局查询实例的 floating/fixed IP，反向代理到 `IP:port`；结果缓存约 1 天，
    带单飞并发折叠、熔断与 fail-open（last-known-good）。
- **HTTPS / SNI**：共享 `0.0.0.0:443` 单监听，按 SNI 选择证书（forward 精确匹配；openstack 通配 `*.base_domain`），
  ALPN 协商 HTTP/2；证书可随热重载无停机轮换。
- **防火墙**：可选 `firewall_dir` 下的 `INSTANCE.role` 规则（开放端口 + IP 白/黑名单，支持 CIDR），默认拒绝；403 不泄露细节。
- **热重载**：`iris reload`（SIGHUP，进程内原子快照换装）、URL 与请求零中断；配置校验失败保持旧配置。
- **可观测**：Prometheus `/metrics`（`config_reload_total`、`firewall_denied_total`）、`/healthz`、`/version`（含配置指纹 `config_hash`）；JSON 结构化日志。
- 借鉴并对照 `xrays-tech/Iris_ssh` 的 OpenStack 发现行为（地址选择、鉴权、缓存、熔断、fail-open）。

## 快速开始

需要 Rust 工具链（1.75+）。

```bash
cargo build --release
# 配置根目录：<root>/sites/ 下每站点一个 .yaml
cargo run --bin iris -- run --config-dir ./config
```

默认监听：HTTP `0.0.0.0:80`、HTTPS `0.0.0.0:443`、指标 `127.0.0.1:9091`
（可在 main 装配层用环境变量 `IRIS_HTTP_LISTEN_ADDR` / `IRIS_HTTP_LISTEN_TLS_ADDR` / `IRIS_HTTP_METRICS_ADDR` 覆盖，便于测试）。

### forward 站点示例

`sites/www-example.yaml`：

```yaml
name: www-example
mode: forward
domain: www.example.com
http: true
https: true
cert_file: /etc/iris/certs/www.example.com.crt
key_file: /etc/iris/certs/www.example.com.key
backends:
  - addr: 10.0.0.5:8080
    weight: 3
  - addr: 10.0.0.6:8080   # weight 默认 1
load_balancer: weighted_round_robin
health_check:
  kind: http
  http_path: /healthz
  interval_secs: 5
  timeout_secs: 2
```

### openstack 站点示例

`sites/rent.yaml`：

```yaml
name: rent
mode: openstack
base_domain: rent.do.top
http: true
https: true
cert_file: /etc/iris/certs/rent.do.top.crt   # 需覆盖 *.rent.do.top 的通配证书
key_file: /etc/iris/certs/rent.do.top.key
firewall_dir: /etc/iris/firewall/rent         # 可选；实例名.role 规则文件

openstack:
  timeout_secs: 30        # 1..=600
  cache_ttl_secs: 86400   # 默认 1 天；校验范围 60..604800
  auth:
    api_url: https://controller.example.com:5000/v3
    approach: password    # 或 application_credential
    username: proxy
    password: ${OS_PASSWORD}   # 凭据推荐走环境变量
    project_name: admin
    project_domain_name: Default
    user_domain_name: Default
  nova:
    all_tenants: true
```

### 防火墙 role 示例

`firewall_dir/INS001.role`（文件名 = 实例名）：

```yaml
allowed_ports: [80, 8080]      # 开放本地端口；不列出 = 全拒
whitelist:                     # 可选；非空时仅放行名单内客户端
  - 192.168.1.0/24
  - 10.0.0.7
blacklist:                     # 可选；命中即拒（优先级高于白名单）
  - 203.0.113.9
```

### 热重载

```bash
iris reload --pidfile /run/iris/iris.pid   # 发 SIGHUP；校验失败保持旧配置
```

## 设计文档（`dev-docs/`）

| 文档 | 内容 |
|---|---|
| `dev-docs/README.md` | 文档索引 + 运行说明 + 最新验证证据 |
| `dev-docs/design.md` | 总体架构、组件、请求生命周期、设计决策 |
| `dev-docs/config.md` | 配置 schema、校验、域名匹配、secret 展开 |
| `dev-docs/discovery.md` | OpenStack 发现：认证/查询/地址选择/缓存/熔断/fail-open |
| `dev-docs/firewall.md` | 防火墙判定顺序与白/黑名单语义 |
| `dev-docs/reload.md` | 热重载机制、信号、优雅关闭/升级 |
| `dev-docs/ops-deploy.md` | **部署与运维**：systemd unit、证书零停机轮换、排障 |

开发计划与门禁见 `dev-docs/plan/`（P0–P8 分步、跨阶段接口契约、验收门禁）。

## 项目结构与验证

```
src/
  main.rs            # 装配：监听、TLS、后台服务、CLI 分发
  config/            # forward/openstack 配置解析、校验、${ENV} 展开（redacted Debug）
  routing.rs         # 精确/通配域名路由（纯函数）
  lb.rs              # LbEngine：RR/加权/随机/ketama + 共享健康注册表
  proxy.rs           # ProxyHttp：forward/openstack 双分支、防火墙门禁、421/404/502、floating→fixed 回退
  tls.rs             # SniCertMap/SniCertSource：按 SNI 选证书、H2
  state.rs           # 单一原子 Snapshot（热重载一次切换）
  firewall.rs        # INSTANCE.role 解析 + default-deny 判定
  reload.rs          # apply_new_config + SIGHUP ReloadService + 指标
  openstack/         # mod/auth/nova/cache：Keystone、Nova 2.1、缓存/单飞/熔断/fail-open
scripts/
  e2e/*.sh           # 7 个可复现端到端（forward/tls/openstack/reload/health-reload/tls-reload/new-site-health）
  mock_openstack.py  # Keystone/Nova 契约 mock
  gen-test-certs.sh  # 自签测试证书生成
tests/
  stubs/upstream.py  # 可配置 HTTP 上游桩
```

**验证证据（HEAD `2b56903`，2026-10-17）**：`cargo build` 0 warnings；`cargo test` 290 passed / 0 failed（lib 212、bin 7、config_load 49、firewall 11、openstack 11）；`scripts/e2e/` 7/7 通过；生产代码零 `unwrap/expect/panic`。

## Acknowledgements

OpenStack 自动模式的解析器行为（Keystone v3 / Nova 2.1 查询、地址选择、缓存/熔断/fail-open）对齐并借鉴了 Cloudflare Pingora 生态与 [`xrays-tech/Iris_ssh`](https://github.com/xrays-tech/Iris_ssh) 的实现思路。
