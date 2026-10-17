# P2 — 路由与域名匹配（纯函数）

- **依赖**：P1。
- **文件范围**：`src/routing.rs`。
- **设计参考**：`config.md` §8（匹配规则）、`design.md` §4.4；契约见 `02-contracts.md`。

## 目标
- `RoutingTable`（精确表 + openstack 通配 pattern）、`normalize_host`、`parse_instance_host`、`lookup`。
- `RoutingTable::try_from(&Config)`（冲突检测复用 P1 校验结论；`base_domain` 互不重叠已在 P1 保证）。

## TDD 单测清单
- `normalize_host`：大小写归一、去尾点、剥离 `:80`/`:443`、非缺省端口保留、空输入 → 空串。
- `parse_instance_host`：
  - `INS001-80` → `("INS001", 80)`；
  - `my-app1-8080`（实例名含 `-`）→ 按**最后一个** `-` 分割 → `("my-app1", 8080)`；
  - 端口 `0` / `65536` / 非数字 / 缺失 → `None`；空 instance → `None`。
- `lookup`：精确命中 forward；`{instance}-{port}.base` 命中 openstack；**精确优先于通配**（构造 `www.rent.do.top` 精确站点 + `rent.do.top` base 场景断言）；均未命中 → `None`。
- 表驱动：多站点、多 base_domain 混合。

## 本阶段门禁
1. 单测全绿。
2. `cargo build` 通过。
