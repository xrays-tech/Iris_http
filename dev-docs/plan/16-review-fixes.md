# P16 — 复审 HIGH 修复（http:false 明文降级 + 无防火墙默认拒绝）

依据 `dev-docs/plan/13-gates-risks.md` 门禁流程；来源：全项目复审（reviewer APPROVE-with-notes）2 项必须修。行为变更，走 TDD + 契约 + 门禁。

## Fix #1 — `http:false` 明文通道拒绝（HIGH）

**问题**：`.http` 仅在配置解析被读取，`request_filter` 明文路径无检查；`https` 侧由 `should_421` 对称执行、`http` 侧不对称——HTTPS-only 站点（`http:false, https:true`）在 :80 明文可服务，中间人下可降级明文。

**语义（契约 + 实现）**：
- 明文通道（`tls_sni(session).is_none()`）且匹配站点 `http == false` → **404**（镜像 421 守卫；不区分实例/转发站点）。
- 纯 `http:true` 站点在 443 上也应被 421 拦下（既有行为，不改）。
- 纯函数 `should_serve_http(site_http: bool, is_tls: bool) -> bool`（`is_tls` 为 true 时与 http 旗标无关，由 421 管）。

**文件范围**：`src/proxy.rs`（request_filter 明文守卫 + 纯函数）、`src/config/forward.rs`、`src/config/openstack.rs`、`tests/`、`scripts/e2e/{03-forward,04-tls}.sh`（各加一条断言：http:false+明文→404）、`dev-docs/{config,ops-deploy}.md`（修正错误描述：421 对应 `https:false`；config.md:34 改为真话）。

TDD 单测：`should_serve_http` 表驱动（http:false+明文→deny；http:true+明文→allow；tls→allow 交给 421）；request_filter 意图验证（明文通道 + http:false → RouteDecision::NotFound）。

## Fix #2 — openstack 站点 firewall_dir 必填（默认拒绝）（MED-HIGH）

**问题**：未配 `firewall_dir` 时 openstack 通配 lane = 无认证任意端口代理（Host 内 instance/port 均攻击者可控，可扫/传任意 all_tenants 实例端口并枚举实例存在性）。

**语义（契约 + 实现）**：
- `OpenStackSite.firewall_dir` 由 `Option<PathBuf>` 改为**必填** `PathBuf`（加载时目录必须存在且可解析；任一站缺失 → `ConfigError::Validate`，reload 校验失败保持旧配置，删除不再 `WARN` 分支）。
- `src/state.rs` 的"未配置 firewall_dir → WARN"路径整体删除（不再存在）。
- `forward` 站点不受影响（实例后端 operator 钉死，无 SSRF）；`bench-ii` 等测试夹具须补齐 `firewall_dir`。
- 文档：`config.md` §4、`firewall.md` §4、`ops-deploy.md` 改为"openstack 站点必须配置 firewall_dir（default-deny）；未配置视为配置错误"。

**文件范围**：`src/config/openstack.rs`、`src/state.rs`、`src/main.rs`（如引用 WARN 文本）、`tests/config_load.rs`（全部内联 openstack YAML 补 `firewall_dir` + 新增缺失→Validate 负例）、`tests/fixtures/**`（openstack/p7/bench 夹具补 `firewall_dir` 目录与 role）、`scripts/bench/perf.sh`（bench-ii 站点补 firewall_dir + 建目录）、`dev-docs/{config,firewall,ops-deploy,plan/16}`。

TDD 单测：缺失 firewall_dir→`ConfigError::Validate`；目录不存在→`ConfigError::Validate`；reload 校验失败→保持旧配置；bench/e2e 全套回归。

> 实现备注（P16）：`firewall_dir` 缺失与目录不存在均走 `ConfigError::Validate`（沿用既有 `site_error`，非 `Io`），与 plan 主体及 `config_load` 既有负例一致。
> `bench-ii` 是 **forward（instance 后端）站点**，不含 `firewall_dir` 字段（防火墙仅作用于 openstack 通配站点）；因此 `scripts/bench/perf.sh` 无需改动，S1–S4 不受影响。另：`scripts/e2e/10-tls-reload.sh` 内的 openstack 通配站点同样需补 `firewall_dir`（已在 P16 补齐）。`src/main.rs` 未引用被删除的 WARN 文本，无需改动。

## 门禁
1. `cargo build` / `cargo build --tests` 0 warnings；2. `cargo test` 全绿（含新增用例）；3. e2e 8/8（03/04 增断言后仍过，07 验证默认拒绝路径，08 reload 保持旧配置）；4. `scripts/bench/perf.sh opt-after` 复用一次性（bench-ii 补 firewall_dir 后能跑通，S3/S4 100%）；5. 生产零 unwrap/expect/panic；6. 独立 commit `P16: ...`；7. 管理复核后推送 origin main。
