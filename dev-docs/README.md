# Iris HTTP — 技术设计文档

Rust + Pingora 的 HTTP/HTTPS 反向代理。需求来源：`propose.md`。

| 文档 | 内容 |
|---|---|
| `design.md` | 总体架构、核心组件、请求生命周期、模块结构、依赖基线、设计决策、可观测性 |
| `config.md` | 配置体系：站点两种模式 schema、负载均衡枚举、校验、域名匹配规则、secret 展开 |
| `discovery.md` | OpenStack 自动模式：Keystone 认证、Nova 查询、地址选择、1 天缓存、单飞/熔断/fail-open |
| `firewall.md` | 防火墙规则：`INSTANCE.role` 格式、default-deny 判定顺序、白/黑名单语义 |
| `reload.md` | `reload` 命令热重载、生效机制、信号与优雅关闭/升级、运维联动 |
| `dev-plan.md` | TDD 多阶段开发计划：阶段划分、依赖图、模块接口契约、门禁验收、mock 契约 |

阅读顺序：`design.md` → `config.md` → `discovery.md` → `firewall.md` → `reload.md`。

版本基线：Pingora 0.9.x（API 以 `cloudflare/pingora` main 2026-10 核对）；OpenStack 发现行为对齐 `xrays-tech/Iris_ssh`。
