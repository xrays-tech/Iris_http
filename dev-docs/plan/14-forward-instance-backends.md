# 14 — 特性：OpenStack 环境独立化 + forward 后端按实例名/ID

> 用户指令（2026-10-18）：「openstack 的配置应该从 rent.yaml 里独立出来，然后在 forward 站点的 backends 的设置里，也可以用 instance name 或者 instance id 来代替 ip」。
> 用户确认：后端条目用**显式端口字段**；实例后端**完全复用发现链路**（按环境共享 resolver：
> floating 优先/fixed 回退、moka 缓存+单飞+熔断+fail-open、TTL 沿用 cache_ttl_secs；健康检查对 resolved IP 探测）。
> 结构决定：独立 `<config-dir>/openstack/<env-name>.yaml` 环境文件 + 站点按名引用（问题未选，采纳推荐项）。
> 契约变更已落 `02-contracts.md`（§Config/OpenStackEnv/BackendTarget/ForwardBackends/InstancePool/resolvers 键=环境名）。

## 配置形态

`<config-root>/openstack/prod.yaml`（文件 stem = 环境名；现在 openstack{} 块不再内嵌于站点）：

```yaml
auth:
  api_url: https://controller.example.com:5000/v3
  approach: password            # 或 application_credential
  username: proxy
  password: ${OS_PASSWORD}
  project_name: admin
  project_domain_name: Default
  user_domain_name: Default
nova:
  all_tenants: true
  # api_url: ...                # 可选（缺省走 catalog）
timeout_secs: 30                # 1..=600
cache_ttl_secs: 86400           # 60..604800
```

openstack 站点（`sites/rent.yaml`）——openstack 块被 `openstack_env` 引用取代：

```yaml
name: rent
mode: openstack
base_domain: rent.do.top
http: true
https: true
cert_file: .../rent.do.top.crt
key_file: .../rent.do.top.key
firewall_dir: /etc/iris/firewall/rent
openstack_env: prod
```

forward 站点 backends —— `addr` 与 `instance` 二选一（entry 级混用），instance 可为名称或 UUID：

```yaml
mode: forward
domain: apps.example.com
openstack_env: prod              # 任一 backend 为 instance 时必填
backends:
  - instance: INS001             # 或 8-4-4-4-12 UUID
    port: 8080
    weight: 2
  - addr: 10.0.0.6:8080
    weight: 1
load_balancer: weighted_round_robin
health_check: { kind: tcp, interval_secs: 5, timeout_secs: 2 }
```

## Lane 计划（顺序执行，文件互斥）

| Lane | 文件范围（唯一写者） | 产出 |
|---|---|---|
| C1 配置 | `src/config/{mod,forward,openstack}.rs`、`tests/config_load.rs`、`src/main.rs`（load_config 调用点 + `ReloadService::new` 接线）、`src/reload.rs`（load_config 调用行 + `ReloadService` 增 envs_dir 存储透传 + **`apply_new_config` resolver-reuse 段机械适配**：`site.openstack` → env 查找 + `OpenStackConfig` 相等 + **`config_fingerprint` 重写**为 per-env 排序哈希 + **cfg(test) 迁移**：`openstack_site()` helper 改 env 引用 + `openstack_envs` fixture + `Backend{addr→target}` 字面量）、`src/lb.rs`（cfg(test) 迁移 + **`BackendTarget::Instance` 机械跳过**：build_lb/LbSlot/pool_backend/backend_key/ProbeRegistry::sync 对无 addr 条目跳过——instance 池由 C3 的 InstancePool 接管，C1 过渡期实例站点池空→select None→502），**机械适配**：`src/openstack/mod.rs`（仅 `build_resolvers` 函数体），`src/{state,proxy,routing,tls,main}.rs` 的 cfg(test) 结构体字面量/助手迁移；**e2e 脚本迁移**：`scripts/e2e/{04-tls,08-reload,10-tls-reload}.sh` 的 heredoc 内嵌 `openstack:{}` 块 → `openstack_env: prod` + 写 `<config-root>/openstack/prod.yaml` + 导出所需凭据环境变量；fixtures `tests/fixtures/openstack/*.yaml`、`sites/*.yaml`、`e2e-p7/**` | OpenStackEnv 加载 + 站点按名引用 + BackendTarget + 校验 + 指纹/编译闭环 + 全量测试绿 |
| C2 发现层 | `src/openstack/{mod,nova}.rs`、`tests/openstack.rs`、`scripts/mock_openstack.py`（按需加 uuid 用例） | **增量** `build_env_resolvers`（键=环境名共享）+ UUID show-by-id + 测试 |
| C3 forward 实例路由 | `src/lb.rs`（ForwardBackends/InstancePool/instance_health_service/ProbeRegistry 跳过）、`src/proxy.rs`、`src/state.rs`、`src/reload.rs`、`src/main.rs`、`scripts/e2e/12-forward-instance.sh`、新 fixtures | 池选择/解析/502/同拾回退/健康/重载（env 比较）+ e2e；切换并删除 `build_resolvers` |
| C4 收尾（管理器或 scribe） | `dev-docs/{config,design,discovery,reload,README}.md`、根 `README.md`、`dev-docs/plan/14-*.md` 标注 | 文档与验证证据更新（含 UUID 语法优先规则、env 复用语义） |

C1→C2→C3 串行（C2/C3 都依赖 C1 类型；C2/C3 均触碰 state/reload/proxy，故不并行）。每 lane 独立 commit（`P14-Cx: ...`），TDD 批处理（先全部单测→实现→一次跑绿），编译 0 warning，生产代码禁 unwrap/expect/panic。

## C1 配置 — TDD 清单

- `load_openstack_envs`：合法/坏 YAML/重复环境名/非法 api_url/越界 timeout、cache_ttl/缺 credentials/目录缺失→空/单文件坏→整份失败/`${ENV}` 展开/MissingEnv。
- `OpenStackSite`：`openstack_env` 必填、引用存在的环境；不再接受内嵌 `openstack{}` 块（旧格式→Parse 错误，文档明示迁移）。
- 全局校验：forward/openstack 站点引用的 openstack_env 必须存在；环境名与站点 name 不冲突约束按现状。
- `BackendTarget`：`addr` 与 `instance` 恰其一（deny_unknown_fields）；instance 缺 port→错；addr 带 port 字段→错；空 instance→错；weight 0..=65535。
- `ForwardSite.openstack_env`：有 instance 后端而无 openstack_env→Validate 错；有 env 但无 instance 后端→允许。
- `Config.openstack_envs` 进入 redacted Debug 与 `config_fingerprint`（真实凭据值、**按环境名排序**哈希）。
- 指纹确定性：同一配置两次加载（envs 以不同 HashMap 插入序构造）→ `config_fingerprint` 相等；仅凭据轮换 → 指纹变化。
- fixtures：`tests/fixtures/openstack/prod.yaml`；`002-openstack-example.yaml` 迁移为 `openstack_env: prod`；新增 `003-forward-instance-example.yaml`；**`tests/fixtures/e2e-p7/sites/001-openstack.yaml` 同步迁移**（其 e2e 配置根补 `openstack/prod.yaml`）；config_load 存量 19 个内联 openstack YAML 迁移 + 每 temp 配置根建空 `openstack/` 目录。
- main.rs：`load_config(&sites_dir, &config_dir.join("openstack"), &env)`；`ReloadService::new` 增 envs_dir 参数（reload.rs 的字段/签名接线在 C1 一并改，仅存储透传，目录语义由 C3 实现）。
- reload.rs（C1）：`apply_new_config` resolver-reuse 段机械适配（env 查找 + `OpenStackConfig` 相等）；`config_fingerprint` 重写为：`{config:?}` 屏蔽 openstack_envs + 按 env-name 排序逐个哈希该环境 redacted `OpenStackConfig` Debug 与 password/secret 原始值。
- lb.rs（C1）：`BackendTarget::Instance` 未含 SocketAddr——build_lb/LbSlot::new/pool_backend/backend_key/ProbeRegistry::sync 对 Instance 条目机械跳过（不 panic、不入池）；过渡期实例池空→select None→502。
- `build_resolvers` 函数体机械适配（openstack/mod.rs，仍按站点名键）——C1 内编译闭环。
- `src/{state,proxy,routing,tls,main}.rs` 的 cfg(test) 内 `OpenStackSite{ openstack: ... }` / `Backend{ addr, .. }` 字面量迁移（test helpers）。
- 指纹：同一配置两次加载（envs 不同插入序）→ 相等；仅改 env 非凭据字段（api_url/cache_ttl）→ 指纹变化。

## C2 发现层 — TDD 清单

- `is_instance_uuid`：true（合法 8-4-4-4-12 hex，含大写/小写）、false（长度/分隔符/非 hex/明显名称）。
- `decode_server`：`{"server":{...}}` 单实例（含 OS-EXT-IPS:type）→ NovaServer；空/畸形→Err。
- `OpenStackResolver::resolve(uuid)`：走 show-by-id（mock 记录路径断言），mixin 现有缓存/单飞/熔断/fail-open。
- **`build_env_resolvers(config)`**：键=环境名；同环境多站点/多 forward 引用 → 只有一个 resolver（缓存共享断言：两次 resolve 只一次 HTTP）。既有 `build_resolvers`（站点名键）保持原样，不改不删（C3 才切换）。
- 回归：名称路径 `GET /v2.1/servers?name=` 不变；`exact_name_match`、`select_address` 不变。
- mock_openstack.py：支持 `GET /v2.1/servers/{uuid}` → 单信封；记录路径供断言。

## C3 forward 实例路由 — TDD 清单

- `InstancePool::new(site_env, backends, mode, snapshot)`：加权轮询（健康项）、ketama（**键=实例身份名**，非 resolved IP）、weight=0 排除、无健康项→None、update 保留健康位。
- `instance_health_service`：仅服务 Instance 池站点；实例解析失败→不健康；解析→探活失败→health.next(false)；探活成功→true；静态项直探；**site 无 health_check → 无探针循环，全部启用项可选，故障即请求时 502**；`ProbeRegistry::sync` 跳过 Instance 池站点（无双探/健康分叉断言）。
- `proxy` forward instance：命中→用池内最近解析 addr 建 peer；无缓存→现场 resolve；`NotFound/NoAddress→502`；`AuthFailed/DiscoveryUnavailable→502`；**fail_to_connect 只对同一 PoolPick 回退 fixed 重试一次（Ctx.pool_pick/resolved 现场），严禁 re-select 换实例**。
- `apply_new_config`：forward 站点池四元比较（backends/lb_mode/health_check/**openstack_env**）复用 `Arc<ForwardBackends>`；resolver 按环境 config 相等复用（切 `build_env_resolvers`，删 `build_resolvers`）。
- env 键切换的显式落点（C3 文件内）：(1) `proxy.rs` openstack 分支 `snapshot.resolvers.get(&site.name)` → `resolver_for(site.openstack_env)`；(2) `reload.rs` resolver 复用键 `site.name` → `env`（同名同 config 复用）；(3) `state.rs` Snapshot.resolvers 注释/`resolver_for` 键语义改为环境名。
- e2e `12-forward-instance.sh`：双实例加权分发（断言 X-Upstream-Port）、name+uuid 混用、删 fixture 文件→502、reload 换 backends 生效、健康剔除恢复、单飞/缓存（mock 记录断言）、token 复用 1 次。

## 门禁

- `cargo build` / `cargo build --tests` 0 warning；`cargo test` 全绿（≥290 实测基线之上 + 新增）。
- 既有 e2e 03/04/07/08/09/10/11 全过 + 新 12-forward-instance.sh 过。
- 生产代码无 unwrap/expect/panic；无 `{config:?}` 泄露 secret（Debug redacted 回归）。
- 文件范围不越界（唯一写者）；不改 `02-contracts.md`（除非发现契约缺陷→回报管理器）。
- 每 lane 独立 commit 并附验证输出。

## C1 配置 — 阶段记录

状态：**完成**（commit `P14-C1: ...`）。

实现要点（与上方 TDD 清单一致，无契约偏离）：

- `Config` 新增 `openstack_envs: HashMap<String, Arc<OpenStackEnv>>`（键=env 文件 stem）；手写 `Debug` 仅渲染 `openstack_envs.len()`，secret 永不入日志。
- `load_openstack_envs(envs_dir, env)`（`src/config/openstack.rs`，经 `mod.rs` 再导出）：目录缺失→`Ok(空)`；任一文件坏→整份失败；文件 schema = 旧 `openstack:` 块字段；`${ENV}` 展开 + `MissingEnv`；含重复环境名防御性守卫（目录布局下不可达）。
- `OpenStackSite` 删除内嵌 `openstack: OpenStackConfig`，改为必填 `openstack_env: String`（旧内嵌格式→Parse 错）；全局校验要求引用的 env 必须存在于 `openstack_envs`，否则 `Validate`。
- `Backend{ target: BackendTarget, weight: u32 }`；`enum BackendTarget { Addr(SocketAddr), Instance{ instance, port } }`；`addr`/`instance` 恰其一（`deny_unknown_fields`）、instance 缺 port / 空 instance / addr 带 port → 错；weight 0..=65535。
- `ForwardSite.openstack_env: Option<String>`：有 instance 后端而无 env→`Validate`；有 env 无 instance 后端→允许。
- `load_config(sites_dir, envs_dir, env)` 三参；`main.rs` 传 `&config_dir.join("openstack")` 并把 `envs_dir` 透传进 `ReloadService`（仅存储，目录语义留待 C3）。
- `config_fingerprint` 重写：`{config:?}`（openstack_envs 已屏蔽）+ **按 env 名排序**，逐个哈希该 env 的 redacted `OpenStackConfig` Debug 与 password/secret 原始值。
- `apply_new_config` resolver-reuse 段机械适配：旧/新 env 的 `OpenStackConfig` 相等则复用（仍按站点名键，env 键切换属 C3）。
- `src/openstack/mod.rs::build_resolvers` 函数体机械适配为对 `config.openstack_envs.get(&site.openstack_env)` 的 `filter_map`（签名与站点名键不变）。
- `src/lb.rs`：仅 `pool_backend` 一处生产改动——`BackendTarget::Instance` 机械跳过（`None`），`BackendTarget::Addr` 入池；未触碰 Pingora 自有 `Backend.addr`（约 157 行）与 `pool_set`/`enabled_backends`。
- cfg(test) 迁移：`src/{state,proxy,routing,tls,reload,main}.rs`；`tests/config_load.rs` 全面重写（temp 配置根补空 `openstack/` 目录、19 处内联 openstack YAML 迁移、新增 env/BackendTarget/指纹测试）。
- fixtures：新增 `tests/fixtures/openstack/prod.yaml`、`tests/fixtures/sites/003-forward-instance-example.yaml`、`tests/fixtures/e2e-p7/openstack/prod.yaml`；`sites/002-openstack-example.yaml` 与 `e2e-p7/sites/001-openstack.yaml` 迁为 `openstack_env: prod`；e2e `04`/`08`/`10` heredoc 同步迁移。

验证输出：

- `cargo build`：0 warning / 0 error。
- `cargo build --tests`：0 warning / 0 error（无越界编译错误）。
- `cargo test`：**317 passed / 0 failed**（6 suites）：lib 217、bin 7、`config_load` 71、`firewall` 11、`openstack` 11。
- VERIFY-2（lb 跳过只存在于 `pool_backend`）：单测 `mixed_addr_and_instance_site_yields_an_addr_only_pool`（选出 addr）、`instance_only_site_yields_an_empty_pool_that_selects_none`（`select`→`None`）、`probe_registry_sync_skips_instance_backends` 均绿。
- VERIFY-3（指纹确定性）：`fingerprint_is_independent_of_env_insertion_order`、`non_credential_env_change_changes_the_fingerprint`、`credential_rotation_changes_the_fingerprint_but_not_debug` 均绿。
- e2e：`03-forward`、`04-tls`、`07-openstack`、`08-reload`、`09-health-reload`、`10-tls-reload`、`11-health-reload-new-site` 全部 PASS（其中 03/07/09/11 未编辑，迁移后仍绿）。
- 生产代码无 unwrap/expect/panic/unreachable/todo/unimplemented；无签名偏离。

发现（供 C3）：C1 过渡期抢答 `BackendTarget::Instance` 站点池为空 → `select` 为 `None`，由 proxy 层映射 502；Instance 池/健康/解析复用留给 C3。
