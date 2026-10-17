# P0 — 脚手架与 CLI

- **依赖**：无（里程碑起点）。
- **文件范围**：`Cargo.toml`、`src/main.rs`、`src/lib.rs`、`src/errors.rs`、`src/logging.rs`。
- **设计参考**：`design.md` §4.1、§7、§6。

## 目标
- Cargo 工程与全部依赖落定（见下），`Cargo.lock` 提交。
- CLI（clap）：`iris run --config-dir <dir> [--pidfile <path>] [--watch-config]`、`iris reload [--pidfile <path>]`（读取 pidfile 发 `SIGHUP`）。
- `tracing` JSON + 控制台日志；`src/errors.rs` 错误类型基座。

## 依赖与版本（P0 独家落定）

```toml
[dependencies]
pingora = { version = "0.9", features = ["openssl", "proxy", "lb"] }
pingora-proxy = "0.9"
pingora-load-balancing = "0.9"
pingora-prometheus = "0.9"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
arc-swap = "1"
moka = { version = "0.12", features = ["future"] }
ipnet = "2"
notify = "7"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"] }   # OpenStack 客户端
chrono = { version = "0.4", features = ["serde"] }
url = "2"
[dev-dependencies]
tempfile = "3"
tokio = { version = "1", features = ["full", "test-util"] }
```

- API 以 `cloudflare/pingora` main（0.9.x，2026-10）为基线。
- 若 `pingora = "0.9"` 解析不到，回退 `"0.9.*"` 并以实际版本核对 API
  （`TlsSettings::with_callbacks`、`HttpPeer::new(addr,tls,sni)`、`PeerOptions`、
  `pingora_core::services::background`）；**任何 API 差异在本阶段回报**，仅调整实现映射，不改接口契约。

## TDD 单测清单
- `reload` 子命令：pidfile 缺失/损坏/路径非法 → 明确报错；临时有效 pidfile → 成功发出 `SIGHUP`（用测试仅验证信号发送路径，不真杀进程）。
- `--config-dir` 缺失默认值与显式值解析。

## 端到端
- `cargo build` 通过；`iris run --config-dir tests/fixtures/empty-config` 启动后 `SIGTERM` 优雅退出（退出码 0）；JSON 日志可输出。

## 本阶段门禁
1. `cargo build` 无 warning 增量（生产代码禁 `unwrap`/显式 `any`/空 catch）。
2. 上述单测与冒烟通过，附输出。
3. 不实现任何路由/代理逻辑（留给后续阶段）。
