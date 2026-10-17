# P4 — TLS/SNI 与 H2

- **依赖**：P3（可与 P5/P6 并行）。
- **文件范围**：`src/tls.rs`、`src/main.rs`（追加 443 监听）、`src/state.rs`（追加 `certs` 字段）。
- **设计参考**：`design.md` §4.2、§8（D1/D2）；契约见 `02-contracts.md`。

## 目标
- `SniCertSource` 实现 Pingora `TlsAccept`：
  - `certificate_callback(&mut SslRef)`：读 `SslRef::servername()`（小写归一）→ `SniCertMap::lookup`：精确命中 `exact`，否则后缀匹配最长 `base_domain`（openstack 站点）→ `ext::ssl_use_certificate` / `ext::ssl_use_private_key`；**均未命中 → 中断握手**（TLS 层无法回 HTTP 状态码）。
  - `TlsSettings::with_callbacks(...)` + `enable_h2()`（H2/H1 ALPN）。
- `SniCertSource::from_sites(config: &Config)`：forward(https) 进 `exact`（键=域名）；openstack(https) 进 `bases`（键=`base_domain`，需通配证书 `*.base_domain`，operator 提供）；文件不存在/解析失败 → 报错。
- `main.rs`：`add_tls_with_settings("0.0.0.0:443", None, tls_settings)`。
- **421 语义（修正）**：仅当 **TLS 已建立**（SNI 命中持证站点）时，请求 `Host` 命中 `https=false` 站点或与 SNI 站点不一致 → 返回 HTTP 421（Misdirected Request）。SNI 无任何站点证书匹配 → 握手失败，永远到不了 421。

## TDD 单测清单
- `SniCertMap` 构建与 `lookup`：exact 命中、base 后缀命中（`INS001-80.rent.do.top` → `rent.do.top`）、最长 base 优先、未命中；域名小写 key、重复域名覆盖。
- 未知 SNI 的查表分支（纯逻辑部分）。
- 421 分支映射（纯逻辑）。

## 端到端
- `openssl s_client -connect 127.0.0.1:443 -servername www.example.com` 断言返回对应证书 CN。
- `openssl s_client -alpn h2 -servername ...` 断言 ALPN 协商 HTTP/2。
- openstack HTTPS：`openssl s_client -servername INS001-80.rent.do.top` → 返回 `*.rent.do.top` 通配证书；随后 `curl --resolve INS001-80.rent.do.top:443:127.0.0.1 https://INS001-80.rent.do.top/` → 200（配合 P7 上游桩；P4 阶段至少验证书与协商）。
- 421：在 SNI 命中的持证连接上请求 `Host: <http-only 站点>` → 421；SNI 无任何站点匹配 → 握手失败（openssl 报错，无 HTTP 状态）。

## 本阶段门禁
1. 单测全绿 + `cargo build`。
2. 上述 openssl 断言通过，附输出。
