# P1 — 配置解析与校验（纯函数核心，所有 lanes 的基座）

- **依赖**：P0。
- **文件范围**：`src/config/mod.rs`、`src/config/forward.rs`、`src/config/openstack.rs`。
- **设计参考**：`config.md` 全部（§3/§4/§6/§7）；契约见 `02-contracts.md`。

## 目标
- 落定 `02-contracts.md` 中 `Config` / `ForwardSite` / `OpenStackSite` / `LbMode` / `HealthCheck` / `AuthConfig` / `NovaConfig` / `ConfigError` / `load_config` 的真实类型。
- `${ENV}` 展开；redacted `Debug`（secret 打码，不进日志）。

## TDD 单测清单
- **forward**：合法解析；空 `backends` 拒绝；`addr` 非法 `IP:port` 拒绝；`weight` 默认 `1`、越界（>65535）拒绝、`0` 语义为禁用后端；`lb_mode` 未知枚举拒绝；`https:true` 但缺证书文件拒绝。
- **openstack**：`base_domain` 合法/非法；两种凭据方式（password / application_credential）齐备性；`cache_ttl_secs` 越界（<60 或 >7d）拒绝；`firewall_dir` 不存在拒绝；`nova.api_url` 非法 URL 拒绝。
- **全局校验**：`name` 重复、`domain` 重复、`base_domain` 互为子域、`domain` 与 `base_domain` 重叠 → 拒绝。
- **secret**：`${ENV}` 展开成功 / 未知变量报 `MissingEnv`（注入式 env，不读真实环境变量）；`Debug` 输出不含密码明文。
- **解析健壮性**：YAML 语法错误、未知字段、错误类型 → `Parse` 错误且带文件路径；单文件失败即整份失败。

## 本阶段门禁
1. 单测全绿（含上述全部正反例）。
2. `cargo build` 通过；`Debug` 打码单测通过。
3. 不实现路由/LB/代理逻辑。
