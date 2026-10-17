# Iris HTTP — 开发计划（分步索引）

> TDD 多阶段开发计划已按步骤拆分到 `plan/` 目录。入口：`plan/01-overview.md`。

| 文件 | 内容 |
|---|---|
| `plan/01-overview.md` | 执行规约、阶段总览表、依赖图（P0→P1→P2→{P3→P4,P5,P6}→P7→P8） |
| `plan/02-contracts.md` | 模块接口契约（跨阶段固定，P1 落定） |
| `plan/03-p0-scaffold.md` | P0 脚手架与 CLI（含依赖与版本） |
| `plan/04-p1-config.md` | P1 配置解析与校验 |
| `plan/05-p2-routing.md` | P2 路由与域名匹配 |
| `plan/06-p3-forward.md` | P3 forward 转发链路 + 最小 Server |
| `plan/07-p4-tls-sni.md` | P4 TLS/SNI 与 H2 |
| `plan/08-p5-openstack.md` | P5 OpenStack 实例发现 |
| `plan/09-p6-firewall.md` | P6 防火墙 ACL |
| `plan/10-p7-orchestration.md` | P7 openstack/firewall 编排集成 |
| `plan/11-p8-reload.md` | P8 reload 与生命周期 |
| `plan/12-mock-fixtures.md` | mock 契约与测试桩清单 |
| `plan/13-gates-risks.md` | 门禁验收、风险与缓解、依赖版本 |
