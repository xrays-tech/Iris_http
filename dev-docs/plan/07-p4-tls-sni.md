# P4 — TLS/SNI 与 H2

- **依赖**：P3（可与 P5/P6 并行）。
- **文件范围**：`src/tls.rs`、`src/main.rs`（追加 443 监听）。
- **设计参考**：`design.md` §4.2、§8（D1/D2）；契约见 `02-contracts.md`。

## 目标
- `SniCertSource` 实现 Pingora `TlsAccept`：
  - `certificate_callback(&mut SslRef)`：读 `SslRef::servername()`（小写归一）→ 查 `ArcSwap<HashMap<domain, Arc<CertKey>>>` 当前快照 → `ext::ssl_use_certificate` / `ext::ssl_use_private_key`；未命中 → 不装配（握手失败）。
  - `TlsSettings::with_callbacks(...)` + `enable_h2()`（H2/H1 ALPN）。
- `SniCertSource::from_sites`：由各 HTTPS 站点 `cert_file`/`key_file` 构建证书表；文件不存在/解析失败 → 报错。
- `main.rs`：`add_tls_with_settings("0.0.0.0:443", None, tls_settings)`；无证书的站点命中 HTTPS → HTTP 421。

## TDD 单测清单
- 证书表构建（多站点 → map、域名小写 key、重复域名覆盖）。
- SNI 归一化与未知 SNI 的查表分支（纯逻辑部分）。
- 421 分支映射（纯逻辑）。

## 端到端
- `openssl s_client -connect 127.0.0.1:443 -servername www.example.com` 断言返回对应证书 CN。
- `openssl s_client -alpn h2 -servername ...` 断言 ALPN 协商 HTTP/2。
- 请求无证书站点（https）→ 421。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. 上述 openssl 断言通过，附输出。
