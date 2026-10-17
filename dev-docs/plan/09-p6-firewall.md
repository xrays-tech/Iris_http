# P6 — 防火墙 ACL

- **依赖**：P2（可与 P3/P5 并行）。
- **文件范围**：`src/firewall.rs` 与 `tests/fixtures/firewall/*.role`。
- **设计参考**：`firewall.md` 全部、`config.md` §4；契约见 `02-contracts.md`。

## 目标
- `build_firewall(dir)`：
  - 只读 `*.role` 文件（文件名即实例名，大小写敏感）；非 `.role` 忽略；
  - `serde_yaml` 解析 `allowed_ports: int[]` / `whitelist` / `blacklist`（CIDR 或纯 IP）；
  - `ipnet` 解析；纯 IP 补 `/32`/`/128`；非法 CIDR / 端口越界（非 `1..=65535`）/ CIDR 条目数 > 10000 → 拒绝加载；
  - 单文件失败 → 整份加载失败。
- `FirewallTable::check(instance, port, client_ip) -> AclVerdict`，顺序（deny by default）：
  1. 无 role → `Deny(NoRole)`；
  2. `port ∉ allowed_ports` → `Deny(PortNotAllowed)`；
  3. `client ∉ blacklist` 命中 → `Deny(Blacklisted)`；
  4. `whitelist` 非空且 `client ∉ whitelist` → `Deny(NotWhitelisted)`；
  5. 否则 `Allow`。

## TDD 单测清单（表驱动）
- 判定顺序：黑名单优先于白名单；白名单启用时未命中拒绝；空白名单 = 放行；未配置白名单时非黑即放。
- 无 role 拒绝；端口未开放拒绝；`allowed_ports` 为空 → 任何端口拒绝。
- CIDR/IPv6 边界：`10.0.0.0/8`、`172.16.0.0/12`、`::1/128`、纯 IP `203.0.113.1`。
- 非法 role 文件（坏 YAML / 非法 CIDR / 端口越界 / 超限条目）→ 拒绝加载；`.role` 后缀过滤（`.txt`/隐藏文件忽略）。

## 本阶段门禁
1. 单测全绿（含固件 fixtures）+ `cargo build`。
2. 不实现与代理/重载的接线（P7/P8）。
