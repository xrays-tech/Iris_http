# P7 — openstack/firewall 编排集成

- **依赖**：P3、P5、P6 全部完成。
- **文件范围**：`src/proxy.rs`（追加 openstack 分支）、`src/main.rs`（装配 openstack 站点与 resolver/firewall 初始化）、`src/openstack/mod.rs`（暴露 resolver 构造，契约见 `02-contracts.md`）、`src/state.rs`（追加 `firewall`/`resolvers` 字段，纯增量）。
- **设计参考**：`design.md` §4.3/§4.6/§4.7、§5；`discovery.md` §7/§8；`firewall.md` §3。

## 目标
- `request_filter` 补全 openstack 分支：
  1. `RouteTarget::OpenStack { site_idx, instance, port }`；
  2. `firewall.check(instance, port, client_ip)` → `Deny` → 403（按 `AclReason` 区分指标）；
  3. `resolver.resolve(instance)`（经 `SharedState.resolvers` 的 `Arc<dyn Resolver>` 注入缝——`02-contracts.md`；单测注入 fake，OpenStack 外部边界允许 mock 容器），错误映射：`NotFound` → 404、`AuthFailed`/`DiscoveryUnavailable`/`NoAddress` → 502；`allow` → 进入 upstream。
- `fail_to_connect`：floating 失败回退 fixed 重试一次（`ResolvedInstance` 有两个地址时）。
- `upstream_peer`：`HttpPeer::new(SocketAddr::new(ip, port), false, "")`。
- `main.rs`：初始化 `SharedState`（config/routing/lbs/certs 已由前序阶段创建，本阶段追加 `firewall`/`resolvers` 字段，均为 `ArcSwap`），openstack 站点可端到端跑通；`Ctx` 记录 `instance`。
- `FirewallTable` 空目录时按 `firewall.md` §4 语义（未配置 `firewall_dir` → 不校验；配置了但无 role → 403）。

## TDD 单测清单
- 编排分支（resolver 经接口注入，仅测编排映射）：
  - Host → `{instance,port}` → ACL Allow → resolver 调用成功 → upstream 地址正确；
  - ACL Deny(NoRole/PortNotAllowed/Blacklisted/NotWhitelisted) → 403；
  - resolver 返回 `NotFound` → 404；`DiscoveryUnavailable`/`AuthFailed`/`NoAddress` → 502。
- `Ctx` 渲染：site/instance 正确透出。

## 端到端（mock Nova + mock 上游 + role fixtures）
- `curl -H "Host: INS001-80.rent.do.top"` → 200 且上游为 `{floating_ip}:80`；
- 端口未开放 / 无 role → 403；实例不存在 → 404；控制面故障 → 502（无 last-known-good）或 fail-open 200（有）。
- HTTP 与 HTTPS（配合 P4）均可达。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. 上述端到端断言通过，附命令与输出。
