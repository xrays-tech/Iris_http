# P17 — instance-name 解析补 show-by-id 详情（真实 Nova 修复）

## 背景与根因（用户实测证据）
真实 Nova 的 **list 端点** `GET /v2.1/servers?name=&all_tenants=true` **不返回 `addresses`**（字段为 null，admin 亦然）；`GET /v2.1/servers/{id}` show-by-id 详情才含地址。
Iris 实例名分支当前直接对 list 结果做地址选择 → 空地址 → `NoAddress` → 502（日志 `instance="vm-v" error="instance has no usable address"`）。UUID 分支走 show-by-id 正常。
用户建议（采纳）：名称匹配成功后，按返回实例 ID 再调一次 show-by-id，用详情做地址选择：
```
servers = query_servers(name)
server  = exact_name_match(name, servers)?
detail  = query_server_by_id(server.id)
pick(detail)
```

## 语义
- 行为：name 路径解析 = 2 次 Nova 调用（list 定位 + show-by-id 详情）。进程内缓存/单飞/熔断/fail-open 不变（单飞对 `resolve(instance)` 即一次 fetch = list+detail 归并）。
- 错误映射不变：list 空/无精确匹配 → NotFound（404）；detail 404 → NotFound；detail 403/5xx/超时/解码 → Transient（502）；详情无地址 → NoAddress（502）。(`map_nova_error`/`pick` 不动)
- 不改变任何公开/契约签名（`OpenStackFetcher::query` 为 crate-private，可改内部实现）。

## 文件范围（唯一写者 = fixer）
- `src/openstack/mod.rs`：`query()` name 分支，`exact_name_match` 命中后 `query_server_by_id(&client,&token.value,&base,&server.id,timeout)` 用详情返回；UUID 分支不变。
- `scripts/mock_openstack.py`：镜像真实 Nova——name 列表端点返回**无 addresses 的壳**（`{"servers":[{"id","name"}]}`，从夹具抽取）；show-by-id 在不命中 `<servers-dir>/<uuid>.json` 时，**扫描 servers-dir 内夹具按内嵌 `server.id` 匹配**返回完整详情（保既有 name 夹具继续可用，不新增夹具文件）。
- `tests/openstack.rs`：既有 name 路径用例继续通过（现多一次 show-by-id）；**新增断言** name 解析在 record 中出现 `GET /v2.1/servers?name=`（1 次）+ `GET /v2.1/servers/{id}`（1 次）；detail 404 → NotFound；list 有壳但 detail 无地址 → NoAddress。
- `scripts/e2e/07-openstack.sh`、`scripts/e2e/12-forward-instance.sh`：按需更新 Nova 调用计数断言（name 路径现为 list+detail 各 1 次；token 复用/单飞/删除夹具→502 仍成立）。**必须保持 8/8 e2e 绿**。
- `dev-docs/discovery.md`：§3 名称路径补「list 定位 + show-by-id 取地址」两步说明；§具体错误语义同步。
- `dev-docs/plan/17-name-detail-fix.md`：追加实现记录。

## 门禁
1. `cargo build` / `cargo build --tests` 0 warnings。
2. `cargo test` 全绿（现有 371 + 新增，6 suites）。
3. e2e **8/8**（含更新后 07/12 的计数断言）。
4. 生产代码零 unwrap/expect/panic。
5. 不改公开/契约签名；不动 `src/openstack/{auth,cache}.rs` 接口、`nova.rs` 的 `decode/select/exact_name_match/query_servers/query_server_by_id` 签名。
6. 独立 commit `P17: name resolution fetches show-by-id detail (real Nova list omits addresses)`，显式路径。

## 实现记录

### 变更
- `src/openstack/mod.rs`：`OpenStackFetcher::query` 名称分支改为 `query_servers` → `exact_name_match` 取 id → `query_server_by_id` 取详情并返回详情（UUID 分支不变）。错误映射（`map_nova_error`/`pick`）与所有公开/契约签名未动。
- `scripts/mock_openstack.py`：列表端点现在按真实 Nova 只返回 `{"id","name"}` 壳（从 `<name>.json` 抽取，无 `addresses`）；show-by-id 在缺 `<uuid>.json` 时扫描 servers-dir 内夹具、按内嵌 `server.id` 返回完整 `{"server": ...}` 详情，仍保留无匹配 404、`--record` 与既有强制状态/延迟/401-once 行为。新增 `--servers-show-status`（仅 show-by-id 强制状态）用于 detail-404 断言。
- `tests/openstack.rs`：更新名称路径的调用数（list + detail）；新增「名称解析恰好 1 次 list + 1 次 detail」「detail 404 → NotFound」「list 有壳但 detail 无地址 → NoAddress」用例；混合/uuid+name、env 共享缓存用例的 Nova GET 计数同步更新。
- `scripts/e2e/12-forward-instance.sh`：名称路径计数拆为 list（`name=appA`）与 detail（`/servers/aaaaaaaa-...`），断言各 1 次，warm 路径总数按 3（appA list + appA detail + uuid show）比较。`07-openstack.sh` 无 Nova 调用计数断言，行为不变。
- `dev-docs/discovery.md`：§2/§4/§7/§8 记录「list 定位 → show-by-id 取详情地址」两步语义与错误映射。

### 验证证据
- `cargo build`、`cargo build --tests`：0 warnings。
- `cargo test`：**373 passed（6 suites）**（原 371 + 新增 2）。
- e2e 8/8：`03-forward.sh`、`04-tls.sh`、`07-openstack.sh`、`08-reload.sh`、`09-health-reload.sh`、`10-tls-reload.sh`、`11-health-reload-new-site.sh`、`12-forward-instance.sh` 全部 `ALL ... ASSERTIONS PASSED`。
- 生产代码扫描：`src/openstack/mod.rs` 变更处无新增 `unwrap`/`expect`/`panic`。

