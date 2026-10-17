# Iris HTTP — 开发计划总览

> 依据：`propose.md`（需求），`design.md` / `config.md` / `discovery.md` / `firewall.md` / `reload.md`（设计）。
> 本文档为开发计划分步入口；各步骤见 `plan/` 下对应文件。

## 1. 执行规约（每阶段 fixer 必须遵守）

1. **TDD 批处理约定**：阶段内**先一次性生成全部单元测试**，再一次性生成实现；
   随后**一次性运行全部单元测试并集中修正**，直到全绿。禁止边写边投机跑测。
2. **编译闭环**：`cargo build` 必须通过；生产代码禁 `unwrap`、禁显式 `any`、禁空 catch、禁吞异常。
3. **Mock 边界**：内部逻辑禁 mock（重构成纯函数后直接单测）；仅外部边界允许 mock
   （OpenStack 控制面 = 第三方 API，允许 mock；上游后端用本地 mock HTTP 服务）。
4. **测试代码豁免**：允许 `unwrap` / 宽松断言。
5. **不改设计**：实现严格对照设计文档；发现设计与实现冲突时先停止并回报，不得自行改接口。
6. **交付物**：代码 + 可复现验证命令与输出摘要（`cargo test`、`curl`/`openssl` 端到端）。
7. 每阶段完成后由工作流管理器复核产物，通过后才进入下一阶段。

## 2. 阶段总览与依赖图

```
P0 → P1 → P2 → { P3 → P4 , P5 , P6 } → P7 → P8
```

- `{}` 内为可并行 lanes（写作用域互不相交）；`→` 为强依赖。
- 并行策略：P2 完成后，同时派发 **P3 / P5 / P6** 三个 fixer；
  P3 完成后可继续 **P4**（与 P5/P6 并行）；P3、P5、P6 全部完成后进入 **P7**；最后 **P8**。

| # | 阶段文件 | 阶段 | 文件范围 | 单测主体 | 阶段门禁 |
|---|---|---|---|---|---|
| P0 | `plan/03-p0-scaffold.md` | 脚手架与 CLI | `Cargo.toml`、`src/main.rs`、`src/lib.rs`、`src/errors.rs`、`src/logging.rs` | 无业务逻辑 | `cargo build` + 启动/优雅退出冒烟 |
| P1 | `plan/04-p1-config.md` | 配置解析与校验 | `src/config/{mod,forward,openstack}.rs` | 解析正反例、校验规则、`${ENV}` 展开、secret 打码 | 单测全绿 |
| P2 | `plan/05-p2-routing.md` | 路由与域名匹配 | `src/routing.rs` | `normalize_host`、`parse_instance_host`、查找优先级 | 单测全绿 |
| P3 | `plan/06-p3-forward.md` | forward 转发链路 + 最小 Server | `src/lb.rs`、`src/proxy.rs`、`src/main.rs`、`src/state.rs`（首创 config/routing/lbs） | 算法映射、后端解析、健康检查状态机 | 单测 + 双后端端到端 curl |
| P4 | `plan/07-p4-tls-sni.md` | TLS/SNI 与 H2 | `src/tls.rs`、`src/main.rs`、`src/state.rs`（+certs） | SNI 选择器映射（纯逻辑） | `openssl s_client` 证书/H2/421/openstack 通配 |
| P5 | `plan/08-p5-openstack.md` | OpenStack 发现 | `src/openstack/{mod,auth,nova,cache}.rs` | token 生命周期、地址选择、缓存/单飞/熔断/fail-open | 单测 + mock Nova 集成 |
| P6 | `plan/09-p6-firewall.md` | 防火墙 ACL | `src/firewall.rs` | role 解析、判定顺序表驱动 | 单测全绿（含 IPv6/CIDR） |
| P7 | `plan/10-p7-orchestration.md` | openstack/firewall 编排 | `src/proxy.rs`、`src/main.rs`、`src/openstack/mod.rs`、`src/state.rs`（+firewall/resolvers） | 编排分支单测 | mock Nova + curl 200/403/404/502 |
| P8 | `plan/11-p8-reload.md` | reload 与生命周期 | `src/reload.rs`、`src/main.rs`（`src/state.rs` 前序已建，仅使用） | 重载状态机、配置指纹 | SIGHUP 热重载端到端、优雅关闭 |

> `src/state.rs` 由 P3 首创并逐步增量加字段（P4 加 `certs`、P7 加 `firewall`/`resolvers`），纯增量不视为接口变更，见 `02-contracts.md`。

## 3. 配套文件

| 文件 | 内容 | 适用范围 |
|---|---|---|
| `plan/02-contracts.md` | 模块接口契约（P1 落定为真实类型，后续只实现不改签名） | 全部阶段 |
| `plan/12-mock-fixtures.md` | mock OpenStack / 上游桩 / 测试 fixtures 清单与契约 | P3/P5/P7/P8 |
| `plan/13-gates-risks.md` | 门禁验收标准、风险与缓解、依赖与版本 | 全部阶段（P0 落定 Cargo.toml） |
