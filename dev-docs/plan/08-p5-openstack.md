# P5 — OpenStack 实例发现

- **依赖**：P2（可与 P3/P6 并行）。
- **文件范围**：`src/openstack/mod.rs`、`src/openstack/auth.rs`、`src/openstack/nova.rs`、`src/openstack/cache.rs`，以及 `scripts/mock_openstack.py`。
- **设计参考**：`discovery.md` 全部；契约见 `02-contracts.md`。

## 目标
- `auth.rs`：`fetch_token` —— Keystone v3 `POST /v3/auth/tokens`：
  - `password` body：`{"auth":{"identity":{"methods":["password"],"password":{"user":{"name","domain":{"name"},"password"}}},"scope":{"project":{"name","domain":{"name"}}}}}`
  - `application_credential` body：`{"auth":{"identity":{"methods":["application_credential"],"application_credential":{"id","secret"}}}}`
  - 解析 `X-Subject-Token` + body `expires_at` + 服务目录（`type=compute`、匹配 region）；token 提前 60s 视为过期；401 单次刷新重试。
- `nova.rs`：`decode_server_list`（`server.addresses.<net>[]` 的 `{addr,version,"OS-EXT-IPS:type"}`）、`exact_name_match`、`select_address`（IPv4 优先 → floating 优先 → fixed 回退）。
- `cache.rs`：
  - moka：正向 TTL=`cache_ttl_secs`（默认 86400）、负缓存 5s、瞬时错误 1s，±10% jitter；
  - 单飞：同 key 并发折叠为一次 HTTP；
  - 熔断：连续失败 5 → open；`timeout_secs*3` 后 half-open；
  - last-known-good：`ArcSwap<HashMap<name, ResolvedInstance>>` 供 fail-open。
- `mod.rs`：`OpenStackResolver::resolve` 组合：缓存 → 单飞 → 熔断 → 认证 → Nova 查询（`GET /v2.1/servers?name={name}&all_tenants=true`，头 `X-Auth-Token`/`OpenStack-API-Version: compute 2.1`/`Accept: application/json`）→ 地址选择 → 写缓存与 last-known-good。

## TDD 单测清单
- token body 序列化（两种凭据）、`expires_at` 解析（RFC3339）与提前过期判断、catalog 选 nova URL。
- `decode_server_list` / `select_address` 表驱动：multi-network、floating-only、fixed-only、IPv6-only、多 floating 取首个稳定序、无地址 → `None`。
- `exact_name_match`：同名多实例、前缀相似名（`web` vs `web-1`）不误配。
- 缓存：正向 TTL/负缓存/瞬时 TTL 各自独立；单飞并发折叠（`tokio` 并发同 key，断言只发一次 HTTP）；熔断状态机（open 后不发请求、half-open 恢复）；fail-open 回退 last-known-good。

## 集成（外部边界 mock，允许）
- `scripts/mock_openstack.py` 契约（同 Iris_ssh）：
  - `POST /v3/auth/tokens` → `201` + `X-Subject-Token`，body 含 Nova catalog；
  - `GET /v2.1/servers?name=&all_tenants=true` → 200 JSON（可配 404/403/超时模式）；
  - 记录收到的 `X-Auth-Token` / `OpenStack-API-Version`，供断言 token 复用次数。
- 用例：token 复用（只发一次 `/v3/auth/tokens`）；token 过期后 401 → 单次刷新并重试成功；空结果 → 负缓存 + `NotFound`；floating-only；控制面宕机 → 熔断 → fail-open（有 last-known-good 则继续成功）；无记录 → `DiscoveryUnavailable`。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. mock 集成用例通过，附输出。
