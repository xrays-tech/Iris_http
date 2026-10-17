# Iris HTTP — Mock 契约与测试桩清单

> 供 P3（上游桩）、P5（mock OpenStack）、P7/P8（组合端到端）使用。契约先于实现编写。

## 1. `scripts/mock_openstack.py`（P5 编写，P7 复用）

- `POST /v3/auth/tokens` → `201` + `X-Subject-Token`，body 含 Nova catalog
  （`service.type=compute`，public URL 指向本 mock 的 `/v2.1`）。
- `GET /v2.1/servers?name={name}&all_tenants=true` → 200 JSON（`{"servers":[...]}`），
  可配置 404 / 403 / 超时模式。
- 记录收到的 `X-Auth-Token`、`OpenStack-API-Version`，供断言 **token 复用次数**（只应发一次 `/v3/auth/tokens`）。
- 契约对齐 Iris_ssh 的 `scripts/mock_openstack.py`（2026-10 核对）。

## 2. `tests/stubs/upstream.py`（P3 编写）

- 可配置：响应体、端口、响应延迟、断连/拒连。
- 用途：P3 LB 轮询/加权/随机/ketama 与健康检查剔除/恢复；P7/P8 端到端目标；P8 慢响应跨重载。

## 3. fixtures

- `tests/fixtures/empty-config/`：空 `sites/` 目录（P0 冒烟用）。
- `tests/fixtures/sites/*.yaml`：P1 起各正反例站点文件（含重复域名、base_domain 冲突、缺证书、坏枚举等）。
- `tests/fixtures/certs/`：测试自签证书与私钥（P4 用；测试代码内生成或仓储提交）。
- `tests/fixtures/openstack/servers/*.json`：Nova 响应样例
  （multi-network、floating-only、fixed-only、IPv6-only、多 floating、无地址）（P5）。
- `tests/fixtures/firewall/*.role`：合法/非法 role（坏 YAML、非法 CIDR、端口越界、超限条目）（P6）。
- `tests/fixtures/env/`：`${ENV}` 展开用注入 env 映射（P1）。

## 4. 使用约定

- 全部 mock 只出现在 `dev-dependencies`/测试与 `scripts/` 下，不进入生产二进制。
- 集成测试统一入口：`tests/integration/`（每个阶段一个目录，`case_*` 脚本），输出结构化到 `target/integ/` 便于复核。
