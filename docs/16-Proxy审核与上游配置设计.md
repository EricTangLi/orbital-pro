# Proxy 审核与「上游 API 配置」设计（参考 CC Switch）

> 触发：用户提问——① 上游接 DeepSeek 能否实现？② 界面是否需要「上游 API 配置」？③ 审核 proxy 结构与代码
> 调研对象：**CC Switch**（[farion1231/cc-switch](https://github.com/farion1231/cc-switch)，MIT 开源，本机已安装并正在运行）
> 日期：2026-11-12

---

## 0 结论

| 问题 | 结论 |
|---|---|
| 上游接 DeepSeek 能否实现？ | ✅ **能**。DeepSeek 是 OpenAI 兼容协议；CC Switch 本机实例就在用 `https://api.deepseek.com`（codex）/ `https://api.deepseek.com/anthropic`（claude）。我们的 proxy 已实测转发 200 |
| 界面是否该有「上游 API 配置」？ | ✅ **必须有**。没有它，proxy 就是死的（不知道转发到哪、不知道用什么 key） |
| proxy 代码审核 | 发现 **1 个 P0 bug（已修）**、3 个 P1 问题（已修 2）、4 个 P2 差距（需排期） |

---

## 1 CC Switch 是怎么做的（本机实证 + 官方仓库）

### 1.1 数据层：18 张表（SQLite）

从本机 `~/.cc-switch/cc-switch.db` 读出的真实表：

```
mcp_servers          providers            provider_endpoints   provider_health
proxy_config         proxy_live_backup    proxy_request_logs   profiles
prompts              settings             skills               skill_repos
model_pricing        session_log_sync     session_usage_dedup  stream_check_logs
usage_daily_rollups  sqlite_sequence
```

**四张核心表**：

| 表 | 作用 | 关键列 |
|---|---|---|
| `providers`（14 行） | 一个「上游」= 一条 provider | `app_type` / `name` / `settings_config`（JSON）/ `is_current` / `in_failover_queue` / `cost_multiplier` / `limit_daily_usd` / `limit_monthly_usd` |
| `provider_endpoints`（4 行） | provider 的实际 URL | `provider_id` / `app_type` / `url` |
| `proxy_config`（4 行，每 app 一份） | 代理的全部参数 | 见下表 |
| **`proxy_live_backup`** | **接管前的原始配置备份** | `app_type` / `original_config` / `backed_up_at` |

### 1.2 `proxy_config` 的真实参数（本机值）

| 参数 | claude | codex / gemini / grokbuild | 含义 |
|---|---:|---:|---|
| `listen_address` / `listen_port` | 127.0.0.1 / 15721 | 同 | 单端口服务所有 app |
| `proxy_enabled` | 1 | 1 | 是否启用代理能力 |
| `enabled` | 0 | 0 | 当前是否真的接管 |
| `auto_failover_enabled` | 0 | 0 | 自动故障转移 |
| `max_retries` | 6 | 6 | 重试次数 |
| `streaming_first_byte_timeout` | **90s** | 90s | 流式**首字节**超时 |
| `streaming_idle_timeout` | **180s** | 180s | 流式**空闲**超时 |
| `non_streaming_timeout` | **600s** | 600s | 非流式总超时 |
| `circuit_failure_threshold` | 8 | 4 | 熔断：连续失败阈值 |
| `circuit_success_threshold` | 3 | 2 | 熔断恢复：连续成功阈值 |
| `circuit_timeout_seconds` | 90 | 60 | 熔断冷却时长 |
| `circuit_error_rate_threshold` | 0.7 | 0.6 | 错误率阈值 |
| `circuit_min_requests` | 15 | 10 | 计算错误率的最少请求数 |
| `live_takeover_active` | 0 | 0 | **是否处于接管状态** |

### 1.3 状态机（`~/.cc-switch/live-state.json`）

```json
"claude-desktop": {
  "mode": "proxy",                       ← direct | proxy
  "attached": true,
  "proxy_route": "40035c1e-...",          ← 指向哪个 provider
  "contract": { "version": 1, "key": "..." }   ← 接管校验
}
```

**⇒ 这就是「改 base_url 让它走 proxy」的产品化形态**：`mode` + `route` + `backup`。

### 1.4 架构演进（官方 release notes）

| 版本 | 关键变化 |
|---|---|
| v3.8.0 | 从「单 JSON 文件」迁移到 **SQLite + JSON 双层**（providers / mcp_servers 表化） |
| **v3.9.0** | **Local API Proxy + per-app takeover + Auto Failover（熔断 + 每 app failover 队列）+ Universal Provider** |
| v3.15.0 | 性能："Trimmed per-request hot-path work and database wait time"；修 `message_delta` 重复、`usage` 缺失等流式兼容 bug |
| PR #9390（2026-11-09） | 补 `/v1/messages/count_tokens` 路由（否则客户端为估算 token 会发**真实计费请求**） |

**对我们的启示**：CC Switch 用两年时间、上百个 PR 才把「代理 + 熔断 + 计费 + 流式兼容」做稳。**我们不应重复造，应先做最小可用，把它的参数作为设计输入。**

---

## 2 我的 proxy 审核（代码级）

### 2.1 P0：URL 拼接 bug（**已修 + 已验证**）

| 项 | 内容 |
|---|---|
| 现象 | 上游配 `https://api.deepseek.com/v1`（OpenAI 习惯），请求 `/v1/chat/completions` → 拼成 `/v1/v1/...` → **404** |
| 根因 | `format!("{}{}", upstream, path)` 无去重 |
| 实测 | 修前：`status=404`；修后：`status=200` |
| 修法 | `join_url()`：若上游带路径前缀且请求路径以同段开头，去掉重复段 |
| 单测 | 3 个（去重 / 无前缀 / 带查询串） |

**为什么这是 P0**：用户按最自然的方式（抄 OpenAI 的 base_url）配置就会坏，且报错是 404 不易联想。

### 2.2 P1：已修的 2 项

| # | 问题 | 修法 |
|:--:|---|---|
| 1 | **超时一刀切 300s** → 流式长对话被截断 | 改为只设 `connect_timeout=30s` + 连接池空闲 90s，**不设总超时** |
| 2 | `no_proxy()` 硬编码 → 必须走代理的环境不可用 | 改为 `ORBITAL_PROXY_USE_SYSTEM_PROXY=1` 可开启 |

### 2.3 P1：未修的 1 项

| # | 问题 | 影响 | 建议 |
|:--:|---|---|---|
| 3 | **无熔断**（上游连续失败时会一直等） | 用户感知为"卡住" | 参考 CC Switch：连续失败 N 次 → 冷却期内直接 502（快速失败） |

### 2.4 P2：与 CC Switch 的功能差距（需排期）

| # | 差距 | CC Switch 的做法 | 建议里程碑 |
|:--:|---|---|---|
| 1 | 单一上游，无法切换/故障转移 | `providers` 表 + `in_failover_queue` + 每 app 队列 | M3 |
| 2 | 无接管备份 | `proxy_live_backup`（original_config + 时间） | **M1（detach 需要它）** |
| 3 | 无请求日志 | `proxy_request_logs` + `stream_check_logs` | M3 |
| 4 | 无用量/成本统计 | `usage_daily_rollups` + `model_pricing`（236 条） | M5 |

---

## 3 「上游 API 配置」功能设计（借鉴 CC Switch，简化）

### 3.1 为什么不直接用 CC Switch 的表结构

CC Switch 的 18 张表是为「14 个 app × 多 provider × 计费 × 会话同步」设计的。
我们 M1 只需要：**一个上游 + 能接管/还原 + 能看见状态**。
**照抄会带来大量我们不需要的复杂度**——按 ADR-012「先有基线再优化」的原则，先做最小闭环。

### 3.2 最小设计（三个概念）

```text
Upstream {                     // 一个"上游 API"
  id: "deepseek"               // 标识
  label: "DeepSeek 官方"
  base_url: "https://api.deepseek.com"   // 只到 host，或到 /v1（join_url 已去重）
  api_key_ref: "secrets:deepseek"        // 只存引用，不存明文
  kind: "openai" | "anthropic"           // 协议
  enabled: bool
}

Takeover {                     // 一次"接管"（= 改某个 agent 的 base_url）
  agent_id: "workbuddy"
  config_path: "~/.workbuddy/models.json"
  original: <原始值的快照>       // = CC Switch 的 proxy_live_backup
  upstream_id: "deepseek"
  active: bool
}

ProxyRuntime {                 // 运行时状态
  listening: bool
  upstream_configured: bool
  inject: "off" | "on" | "skipped(reason)"
}
```

### 3.3 界面（M2 的「设置 · 上游」页）

参考 CC Switch 的布局，但只保留我们需要的：

```text
┌─ 上游 API ────────────────────────────────┐
│  ● DeepSeek 官方        https://api.deepseek.com    [当前]
│    key: 已配置（隐藏）                  [编辑] [测试]
│  ○ 硅基流动              https://api.siliconflow.cn
│  ○ 本地 Ollama          http://127.0.0.1:11434
│                                          [+ 添加上游]
├─ 代理接管 ────────────────────────────────┤
│  agent        模式        上游           状态
│  WorkBuddy    proxy  ●   DeepSeek 官方   已接管 [还原]
│  pi           direct     —              未接管 [接管]
│  DSH          direct     —              未接管 [接管]
│  Hermes       direct     —              未接管 [接管]
├─ 健康 ────────────────────────────────────┤
│  proxy: 监听中   注入: 未启用（M3）      上游探测: 200 (12ms)
└───────────────────────────────────────────┘
```

**关键交互**：
1. **「测试」按钮**：直接打上游 `/models`，显示真实状态码与耗时（不猜）
2. **「接管」**：先写 `Takeover.original` 快照 → 再改配置（顺序不能反）
3. **「还原」**：用快照写回，**并把 `active` 置 false**（这是 detach 的基础）
4. **「当前」标记**：同一时刻只有一个上游是 `is_current`（CC Switch 的 `is_current` 语义）

### 3.4 与 CC Switch 的明确分工（避免重复造）

| 能力 | CC Switch | 我们 |
|---|---|---|
| 多 app 配置切换 | ✅ 主业（14 个 app） | ❌ 不做（我们只做记忆） |
| 上游 provider 管理 | ✅ 90+ 预设 | ✅ 做**最小版**（自建/自定义） |
| 代理接管 + 备份 | ✅ | ✅ 做（我们 proxy 需要） |
| 熔断/重试/计费 | ✅ 完整 | ⏳ M3 做熔断，计费不做 |
| 记忆注入 | ❌ | ✅ **这是我们的差异点** |

**结论：CC Switch 是"配置切换器 + 网关"，我们是"记忆层"。上游配置是必需品，但只做最小版。**

---

## 4 本轮实跑证据

```text
cargo test -p orbital-proxy → 3 passed（join_url 去重/无前缀/带查询串）
端到端（上游 Ollama，无前缀）：200，模型数 2
端到端（上游带 /v1 + 请求 /v1）：修前 404 → 修后 200
cargo test -p orbital-core → 44 passed
```

---

## 5 待办（按优先级）

| # | 事项 | 理由 |
|:--:|---|---|
| 1 | **Takeover 快照 + 还原**（= CC Switch 的 `proxy_live_backup`） | detach 的基础；现在 4 个 agent 配置被改过却没退路 |
| 2 | 上游配置的数据模型 + 设置页 | 没有它 proxy 只能靠环境变量，用户不可用 |
| 3 | 熔断 | 上游挂了不能一直等 |
| 4 | 请求日志 | 排障必需 |
