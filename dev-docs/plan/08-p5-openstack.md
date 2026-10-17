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

## 实现记录（P5 落地 2026-10）

- **文件**：`src/openstack/{mod,auth,nova,cache}.rs`、`scripts/mock_openstack.py`、
  `tests/fixtures/openstack/servers/*.json`、`tests/openstack.rs`。
- **契约**：`Resolver`/`ResolvedInstance`/`ResolveError`/`OpenStackResolver::new|resolve`、
  `fetch_token(&site.auth, &site.auth.api_url, client)`、`decode_server_list`/`exact_name_match`/
  `select_address` 均按 `02-contracts.md` 落定。`HttpClient`（`reqwest::Client` 轻包装）定义并导出
  于 `src/openstack/mod.rs`，供 P7 注入 `Arc<HttpClient>`。
  - 唯一增量：`Resolver` 增加 `Send + Sync` 超-trait（`SharedState` 的 `Arc<dyn Resolver>` 需要），
    方法签名不变；`OpenStackResolver::invalidate(&self, instance)` 为新增辅助方法（reload/测试用）。
- **行为**：`resolve` = 缓存（正向/负 5s / 无地址 1s / 瞬时 1s，±10% jitter）→ 单飞（`watch` 折叠）
  → 熔断（连续 5 次瞬时失败 open，冷却 `timeout_secs*3`，half-open 探测）→ 认证（token 缓存，提前 60s
  刷新，Nova 401 单次刷新重试）→ `GET /v2.1/servers?name=&all_tenants=true` → `select_address`
  （全局 IPv4 优先；floating 优先，fixed 回退；网络名排序保证多 floating 稳定首取）→ 写缓存 +
  last-known-good（`ArcSwap<HashMap>`，瞬时故障 fail-open）。
- **依赖**：新增直接依赖 `serde_json = "1"`（`reqwest` 的 `json` 特性已锁定引入 1.0.154，未新增任何
  crate，`Cargo.lock` 仅把 `serde_json` 列入 iris-http 依赖表）。`decode_server_list(&[u8])` 的字节级
  JSON 解码必须具名 `serde_json`，属计划 `reqwest json` 的隐含依赖，已在此显式记录。
- **单测**：`cargo test` 中 openstack 相关 —— 单元 39 例（auth body/解析/catalog/过期、nova 双信封解码/
  exact_name_match 反前缀/地址矩阵、cache jitter/ttl/熔断状态机、resolver 缓存/单飞/熔断/fail-open/
  映射）；集成 10 例（mock harness：token 复用、401 单次刷新、空结果/404→NotFound、floating/fixed/IPv6、
  403/500→DiscoveryUnavailable、控制面宕机无记录→DiscoveryUnavailable、宕机 fail-open last-known-good）。
- **mock 冒烟**：`python3 scripts/mock_openstack.py --record /tmp/rec.jsonl --servers-dir tests/fixtures/openstack/servers`
  后 `POST /v3/auth/tokens`→201+`X-Subject-Token`、`GET /v2.1/servers?name=multi-nic&all_tenants=true`→200、
  未知名→404，`/tmp/rec.jsonl` 记录 `x-auth-token`/`openstack-api-version`。

