# P15 — 热路径可避免开销优化

> 背景：2026-10-18 热路径审计（explorer, ses_eb2d34…）确认应用层零拷贝与无锁成立，但存在三处**可避免**的每请求开销。用户指令："静态审计发现的'可避免开销' 作为独立优化项（TDD + 前/后压测对比）立项实施"。
> 原则：**行为完全等价的纯优化**——不改任何契约签名、不改配置 schema、不改语义；所有既有 360 单测与 8 个 e2e 必须保持全绿；编译 0 warning；生产代码禁 unwrap/expect/panic。
> 通用规则：TDD 批处理（先补针对性单测→实现→一次跑全量单测修正→编译→e2e→压测对比）。

## 三项优化

### OPT-1 [lb.rs] ketama 实例池环缓存（最大单项）
- 现状：`InstancePool::select` ketama 分支（lb.rs:447-462, 645-669）**每请求**重建 `nodes: Vec<(String,u32)>`（含每个健康项 `identity()` 的 String 克隆）→ 建环 `Vec<(u64,usize)>`（大小=Σ(weight·64)）→ `sort_by`。静态池的 Pingora `Consistent` 只建一次。
- 改法：`PoolState` 增环缓存。`PoolState` 加 `gen: u64`（entries 替换/健康位翻转时在持有锁内自增）+ `ring_cache: Option<(built_gen: u64, ring: Vec<(u64, usize)>)>`。`select` ketama 分支仅在 `built_gen != gen` 时重建（重建逻辑原样搬到持锁段），命中缓存直接二分查环。环的构建输入（健康+启用项、权重、`identity()`=实例名，**绝不用 resolved IP**）与现状逐位一致。
- 健康位翻转发生在 `instance_health_service` 探针路径（同一把 `state` 锁内更新 entries → `gen += 1`），无竞态。
- TDD：
  - `ketama_ring_is_rebuilt_only_when_pool_changes`：同健康集连续 N 次 select 后 gen 未变（不可直接观察内部 → 用行为等价：连续 select 分布与旧实现一致 + 探针翻转健康后分布改变）；
  - `ketama_distribution_matches_previous_algorithm`：旧算法（直接重建）与新算法对同一 key 集合（如 0..10000 的确定性伪随机 key）产出**相同 instance 选择**（新实现先用一个临时旧版函数对照，或保留重建函数为 `#[cfg(test)]` 内部助手做参照）；测试用固定种子，禁随机断言。
  - 既有池测试（加权/健康剔除/weight=0/update 保留健康/ketama sticky）必须原样通过。
- 不变量：环大小 Σ(weight·64)；key 哈希输入 = 实例身份（同现状 `ip_key_bytes` 语义只对静态池，实例池用 identity 字节）。

### OPT-2 [proxy.rs] 单次 site.name 克隆
- 现状：forward 路径 `site.name` 每请求克隆 2-3 次：`decide_route`(:147)→`ctx.site`（**保留这一次，语义必需**）；`site_facts`(:100-105, 仅 TLS)；`upstream_peer`(:659) 每 attempt 又克隆 `ctx.site`。
- 改法：后两处改借用 `&str`（`ctx.site.as_deref()` 或从 `snapshot.config.forward[idx].name` 借用），不再克隆。`snapshot.lbs.get(key)` 等检索接受 `&str`。**保持所有 `pub fn` 签名不变**（`site_facts` 形参若为 `&str` 则调用方传借用；内部不再 `.name.clone()`）。
- 保留的合理拷贝：`decide_route` 的 `ctx.site`（每请求一次）；OpenStack 分支 `instance` String（查找键，必需）。
- TDD：既有测试全绿（行为等价）；补 `forward_site_name_is_not_fetched_from_config_per_attempt` 类行为测试不可行 → 以代码评审 + e2e 03 回归为准（本项无新语义测试，压测对比即可）。

### OPT-3 [lb.rs + proxy.rs] 实例池单次持锁
- 现状：稳态 `InstancePool::select`（锁→建 pick）与 `resolved()`（再锁→读缓存地址）**每请求两次取锁**。
- 改法：`InstancePool` 新增 `fn pick(&self, ip: Option<IpAddr>) -> PickOutcome`（`PickOutcome { pick: Option<PoolPick>, cached: Option<ResolvedInstance> }`），在一次持锁内完成：健康列表+cursor/环+拷贝已缓存解析结果（`ResolvedInstance: Copy`）。proxy 的 `select_backend`/`resolve_instance_pick` 稳态路径改走 `pick`：`cached` 有值 → 直接用（不再第二次锁）；无值 → 现场 live resolve → 走既有 store 路径（该路径持锁登记，反正是 cache-miss）。
- **同 pick 回退语义不变**：`fail_to_connect` 仍对同 `ctx.pool_pick` 的 fixed 重试一次（若 pick 是实例且 floating 失败）；严禁 re-select 换实例——OPT-3 不触碰 `Ctx`/fallback 逻辑。
- 旧 `select`/`resolved` 若仍被其他调用点使用则保留（或内部改调 `pick`），公开签名列表在 `02-contracts.md` 的项不动（`select`/`update` 若在契约中是公开项则保留签名，内部实现可改）。
- TDD：既有实例池/fallback/reload 测试全绿；补 `pick_returns_cached_resolution_without_second_lock` 的**行为**测试（连续两次 pick 同一实例返回相同地址；update 后 pick 反映新 entries）。

## 压测协议（前/后对比，同一脚本）

- `scripts/bench/perf.sh`（本计划一并新增）：`ulimit -n 8192`；构建 release；依赖：`scripts/mock_openstack.py`（2 实例 fixtures）+ `tests/stubs/upstream.py`（2 个桩端口）+ `iris` release = `/Users/alex/.cargo/global-target/release/iris`。
- 场景（oha 1.15，`-z 6s`，`-H "Host: bench.test"`，文本输出存 `/tmp/iris-bench-*`）：
  - S1 静态 round_robin c=50；S2 静态 round_robin c=200（回归，覆盖 OPT-2/3 通用路径）；
  - S3 forward 实例 ketama c=50；S4 forward 实例 ketama c=200（OPT-1/3 主线，环重建开销锚点）。
- 指标：RPS、p50/p90/p99、success%；同一机器同 macOS 会话下连续跑，避免后台负载；每场景跑 2 次取优（防抖动）。
- **基线**：`60ca157`/`91bbe4a` 同代码树在改动前先跑一遍，数值记录到本节；改动后同脚本复跑，输出对比表。结论标准：OPT 后 RPS 不降（静态回归）且实例 ketama 场景 p99/RPS 有可观测改善即达标；如实记录差距（受 oha/Python 桩上限 ≈29-31k 包围，改善可能仅在 p99/稳定性层面）。

## 门禁
1. `cargo build` + `cargo build --tests` 0 warnings；2. `cargo test` 全绿（360 baseline，只增不减）；3. e2e 8/8 全过（03/04/07/08/09/10/11/12）；4. `scripts/bench/perf.sh` 前/后对比表入档；5. 生产代码零 unwrap/expect/panic（grep 复核）；6. 独立 commit `P15-perf:` + 更新本计划记录。

## 禁止
触碰契约签名/schema/行为语义；改 `02-contracts.md`；动 `src/openstack/*`（OPT 不涉及发现层，除 lb/proxy 外一律不动）；`git add -A`。

---

## 实施记录（2026-10-18, fixer）

范围内文件：`src/lb.rs`、`src/proxy.rs`（仅此二者 + 本计划记录）。契约签名/ schema / 行为语义未改；`02-contracts.md` 未动。

### OPT-1 — ketama 环缓存（src/lb.rs）
- `PoolState` 新增 `gen: u64` 与 `ring_cache: Option<(u64, Vec<(u64, usize)>)>`。
- `gen` 在持锁内自增：`update()`（成员集替换，同时清空缓存）、`probe_instances()`（健康位真正翻转时，同时清空缓存）。`cached` 解析写入不影响环，不触发。
- 环构建逻辑抽为 `ring_from_nodes(&[(String,u32)]) -> Vec<(u64,usize)>`（与旧内联代码逐位一致：`max(1)*64` 副本、`hash_identity(identity, replica)`、按 hash 排序），查环抽为 `ketama_lookup(ring, key)`。`ketama_index` 保留为 `#[cfg(test)]` 无缓存参照（消除 dead_code）。
- `pick()` 的 ketama 分支仅在 `built_gen < gen`（或缓存缺失）时重建；节点输入 = 健康+启用项的 `identity()`（实例名/字面地址）与 weight，**绝不用 resolved IP**。round_robin/weighted/random 分支不碰环。

### OPT-2 — site.name 冗余克隆（src/proxy.rs）
- 保留 `decide_route`→`ctx.site` 的唯一 owned 克隆（:147）。
- `site_facts` 改为借用：`pub fn site_facts<'a>(&'a Snapshot, &RouteDecision) -> Option<(bool, &'a str)>`，直接借 `site.name.as_str()`，不再 clone；调用点同步去掉 `.as_str()`。该函数非 `02-contracts.md` 钉定项，属计划授权的内部助手签名变更。
- `upstream_peer` 的 `ctx.site.clone()` 改为 `ctx.site.as_deref()` 借用（与 `ctx.pool_pick`/`ctx.resolved` 为不相交字段借用）。

### OPT-3 — 单锁 pick（src/lb.rs + src/proxy.rs）
- `InstancePool::pick(&self, Option<IpAddr>) -> PickOutcome`（`pub(crate)`）：一次持锁完成与 `select()` 完全相同的健康过滤+选择，并在同一锁域内返回被选实例的 `cached: Option<ResolvedInstance>`（静态项/未缓存 → None）。`PickOutcome { pick, cached }` 为 `pub(crate)`（不扩展 crate 外部公开 API）。
- `select()` 委托 `pick().pick`；`resolved()` 原样保留。
- `proxy.rs` 新增内部 `select_pick()`：Static 池走 `lb.select`，Instance 池走 `pool.pick`，把「选择 + 缓存解析」合并为一次持锁。`upstream_peer` 稳态路径改走它；缓存缺失 → 仍走既有 `resolve_instance_pick` live resolve 路径。`select_backend`/`resolve_instance_pick` 公开签名保留（`select_backend` 现仅测试引用）。
- **同 pick 回退语义不变**：`reuse_pick`（retry 固定同 pick）、`fail_to_connect`、`instance_fallback`、`use_fixed` 逻辑一律未触碰。

### 门禁证据
1. `cargo build` → 0 error / **0 warning**。
2. `cargo build --tests` → 0 error / **0 warning**。
3. `cargo test` → **367 passed / 0 failed**（6 suites；baseline 360 + 7 新增：lb 6 + proxy 1）。新增单测：`ketama_distribution_matches_previous_algorithm`、`ketama_ring_is_rebuilt_only_when_pool_changes`、`ketama_ring_tracks_entry_set_update`、`pick_returns_cached_resolution_without_second_lock`、`pick_static_entry_has_no_cached_resolution`、`pick_reflects_updated_entries`、`select_pick_fetches_cached_resolution_in_one_call`。
4. e2e **8/8 PASS**：03-forward、04-tls、07-openstack、08-reload、09-health-reload、10-tls-reload、11-health-reload-new-site、12-forward-instance。
5. 生产代码零 `unwrap()`/`expect()`/`panic!`/`unreachable!`/`todo!`/`unimplemented!`（按文件 grep，全部命中均为既有 `unwrap_or*` 安全变体/注释；测试模块起点 lb.rs:1202、proxy.rs:832）。
6. 单 commit `P15-perf: OPT-1 ring cache, OPT-2 name borrows, OPT-3 single-lock pick`，显式路径暂存（`src/lb.rs`、`src/proxy.rs`、本文件）；未 push。

### 契约偏差
无不可回避的契约偏差。唯一签名变化是 `site_facts` 返回值由 `Option<(bool, String)>` 改为 `Option<(bool, &str)>`（去克隆所必需），该函数不在 `02-contracts.md` 钉定列表内，属计划明示的「内部助手签名可变」。
