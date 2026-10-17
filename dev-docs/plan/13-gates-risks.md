# Iris HTTP — 门禁验收、风险、依赖

## 1. 门禁验收（每阶段 fixer 交回即复核）

1. `cargo build` 无 warning 增量（生产代码禁 `unwrap` / 显式 `any` / 空 catch / 吞异常）。
2. `cargo test` 全绿（本阶段新增用例 + 既有用例无回归）。
3. 端到端断言齐全且附可复现输出（curl / openssl 命令原文）。
4. 文件范围不越界（`01-overview.md` 阶段表）；未改其他阶段接口签名（`02-contracts.md`）。
5. 未引入与设计相悖的行为；有疑义先回报，不静默偏离。
6. 每阶段完成即提交（commit message 含阶段号，如 `P1: config parsing & validation`），
   由工作流管理器复核后方可进入依赖它的下一阶段。

## 2. 风险与缓解

| 风险 | 缓解 | 责任人阶段 |
|---|---|---|
| 设计文档 API 名称与真实 Pingora 版本有出入 | P0 冒烟即验证核心 API（Server/TlsSettings/HttpPeer）；差异回报后仅调整实现映射，不改接口契约 | P0 |
| OpenStack 契约假设错误（token/响应结构） | 以 `scripts/mock_openstack.py` 为唯一契约；全部 P5/P7 集成用例基于它；契约先于实现编写 | P5 |
| 并行 lanes 编辑冲突 | `02-contracts.md` 固定各文件归属；P3/P5/P6 文件互不重叠；提交粒度=阶段 | P3/P5/P6 |
| 重载语义破坏在途连接 | P8 端到端专门验证"慢请求/WebSocket 跨重载""坏配置保持旧态" | P8 |
| 地址选择/ACL 判定边界漏测 | 表驱动单测覆盖浮动/固定/IPv6/多网卡/CIDR 边界 | P5/P6 |
| secret 泄漏进日志 | P1 redacted Debug 单测 + 全局日志扫关键词 | P1 |
| SIGHUP 是否被 Pingora 接管不明，缺省可能杀进程 | P0 API 探针验证归属；P8 若需则自建 tokio `SignalKind::hangup()` 重载任务 | P0/P8 |
| LoadBalancer selection 泛型 / 权重字段形态不确定 | `LbEngine` 包装解耦契约；P0 探针实测后由 P3 落定内层映射 | P0/P3 |
| 监听器装配归属不确定（向 Service 挂监听） | P0 探针；P4 照实装配（`add_tcp`/`add_tls_with_settings`） | P0/P4 |
| openstack HTTPS 依赖通配证书 | 文档明示 operator 提供 `*.base_domain`；P4 端到端覆盖 SNI 通配 | P4 |

## 3. 依赖与版本（P0 落定 Cargo.toml）

```toml
[dependencies]
pingora = { version = "0.9", features = ["openssl", "proxy", "lb"] }
pingora-proxy = "0.9"
pingora-load-balancing = "0.9"
pingora-prometheus = "0.9"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
arc-swap = "1"
moka = { version = "0.12", features = ["future"] }
ipnet = "2"
notify = "7"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"] }   # OpenStack 客户端
chrono = { version = "0.4", features = ["serde"] }
url = "2"
async-trait = "0.1"   # Resolver 注入缝（P7 测试用）
[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["full", "test-util"] }
```

- 版本由 `Cargo.lock` 提交；API 基线 `cloudflare/pingora` main（0.9.x，2026-10）。
