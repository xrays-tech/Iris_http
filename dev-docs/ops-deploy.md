# Iris HTTP — 部署与运维

基于本仓库实现（HEAD 含快照原子热重载、共享健康注册表、TLS 证书热重载）。本文给 systemd 部署、证书轮换、日常操作与排障。

## 1. 构建与安装

```bash
cargo build --release            # 产物：target/release/iris
install -m 0755 target/release/iris /usr/local/bin/iris
install -d -o iris -g iris /etc/iris/sites /run/iris
```

- 配置根目录：`/etc/iris`（其下必须含 `sites/` 子目录，每站点一个 `*.yaml`）。
- 运行账户：专用低权用户 `iris`；绑定 80/443 需 `CAP_NET_BIND_SERVICE`（见下方 unit）。

## 2. systemd unit

`/etc/systemd/system/iris.service`：

```ini
[Unit]
Description=Iris HTTP reverse proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=iris
Group=iris
# 低权限绑定 80/443
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
# 拓扑依赖：若 openstack 站点依赖上游 DNS/网络先就绪
# Requires=openstack-controller.service   # 按需
LimitNOFILE=1048576
ExecStart=/usr/local/bin/iris run --config-dir /etc/iris --pidfile /run/iris/iris.pid
# 零停机热重载：发 SIGHUP（进程内原子换装，无需重启）
ExecReload=/usr/local/bin/iris reload --pidfile /run/iris/iris.pid
# 优雅关闭（SIGTERM；Pingora 排水后退出）
KillSignal=SIGTERM
TimeoutStopSec=30
Restart=on-failure
RestartSec=3
# 按需注入：RUST_LOG=iris_http=debug
Environment=RUST_LOG=iris_http=info

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now iris
```

## 3. 端口 / 指标

| 项 | 默认 | 覆盖环境变量 |
|---|---|---|
| HTTP 监听 | `0.0.0.0:80` | `IRIS_HTTP_LISTEN_ADDR` |
| HTTPS 监听 | `0.0.0.0:443` | `IRIS_HTTP_LISTEN_TLS_ADDR` |
| 指标端口 | `127.0.0.1:9091` | `IRIS_HTTP_METRICS_ADDR` |

指标端口提供：`/metrics`（prometheus，含 `config_reload_total{ok}`、`firewall_denied_total{reason}`）、`/healthz`、`/version`（`binary=<ver> config_hash=<hex>`）。

- 健康检查：`curl -fsS http://127.0.0.1:9091/healthz`
- 配置指纹：`curl -fsS http://127.0.0.1:9091/version` — `config_hash` 变化即配置已生效；secret 变更会改变指纹但不会泄露值。

## 4. 证书轮换（零停机）

TLS 证书在进程内维护（`SniCertSource` 读共享快照），**执行 `iris reload`（SIGHUP）后新握手立即使用新证书**，无需重启。

推荐流程（避免读到半写文件）：

1. 签发/续期新证书与私钥：`certbot renew` 或自有 CA，写为新路径，如
   `/etc/iris/certs/example.com.new.crt`、`example.com.new.key`。
2. 原子替换目标文件：
   ```bash
   install -m 0644 example.com.new.crt /etc/iris/certs/example.com.crt
   install -m 0600 example.com.new.key /etc/iris/certs/example.com.key
   ```
   （或直接在站点 `*.yaml` 里改 `cert_file`/`key_file` 指向新路径。）
3. 热重载：`systemctl reload iris`（等价 `iris reload --pidfile /run/iris/iris.pid`）。
   - 校验失败（证书/私钥不匹配、文件缺失、YAML 错误）→ **保持旧配置与旧证书继续服务**，仅计 `config_reload_total{ok="false"}`。
4. 验证：
   ```bash
   openssl s_client -connect 127.0.0.1:443 -servername www.example.com \
     </dev/null 2>/dev/null | grep -E 'subject=|Verify return'
   curl -fsS http://127.0.0.1:9091/version   # 确认 config_hash 已变
   ```

规则提醒：

- forward HTTPS 站点证书按**精确域名**匹配；openstack HTTPS 站点的 SNI 是任意 `{instance}-{port}.base_domain`，必须提供**通配证书 `*.base_domain`**（示例生成脚本 `scripts/gen-test-certs.sh`）。
- SNI 无任何站点证书 → 中断握手（TLS 层无 HTTP 状态码）；TLS 建立后 Host 命中 `http: false` 站点 → HTTP 421。

## 5. 配置变更 / 防火墙

- 站点增删改：改 `/etc/iris/sites/*.yaml` 后 `systemctl reload iris`。forward 后端池变更走 `lb.update()` 原子换表；openstack 解析器缓存保留（控制面配置未变时）。
- 健康检查：reload 新增/变更的 forward 站点立即挂入共享探针（503/宕机后端不被选中）。
- 防火墙：在 openstack 站点 `firewall_dir` 下放 `INSTANCE.role`（`allowed_ports`/`whitelist`/`blacklist`，CIDR 或纯 IP），reload 即生效；单文件解析失败整份拒绝并保持旧规则。
- 敏感项：凭据建议走 `${ENV}`（`${OS_PASSWORD}` 等），`Environment=` 注入；`Debug`/日志/`/version` 永不落 secret。

## 6. 排障

| 症状 | 处置 |
|---|---|
| reload 后进程被 SIGHUP 杀掉 | 该二进制未注册 SIGHUP 重载任务；确认运行的是构建于快照热重载之后的 `target/release/iris` |
| 443 握手失败、无 HTTP 状态 | SNI 无证书匹配（通配证书未配/未重载）；`openssl s_client -servername <域名>` 看是否出证书 |
| openstack 站点 502 | 解析器错误（认证失败/发现不可用/无地址）；上游 floating 故障已自动回退 fixed 仍失败 |
| openstack 站点 403 | 防火墙 default-deny：未开放端口 / 未命中白名单 / 黑名单 / 无 role 文件 |
| 实例 404 | Nova 全局无该名称实例（注意是全项目 `all_tenants` 查找） |
| `/version` config_hash 未变 | reload 校验失败（看 `config_reload_total{ok="false"}`），旧配置仍在服务 |
| 指标端口被占用 | 换 `IRIS_HTTP_METRICS_ADDR`（默认 127.0.0.1:9091，仅本机可达） |

## 7. 常用命令

```bash
systemctl status iris --no-pager
systemctl reload iris                     # SIGHUP 热重载（推荐）
iris reload --pidfile /run/iris/iris.pid  # 等价手工触发
journalctl -u iris -f                     # JSON 日志
curl -fsS http://127.0.0.1:9091/healthz && echo healthy
```
