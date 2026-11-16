# Hook 技术原理 + 多模型场景下 proxy 的影响

> 触发：用户两问——① hook 的具体技术实现逻辑原理是什么？② 配了图像/音频/视频模型，proxy 接管会让它们失效吗？
> 证据：Claude Code 官方 hooks 规范（本机 agent 体系同源）+ 本机 WorkBuddy/pi 的真实模型配置
> 日期：2026-11-12

---

## 0 结论

| 问题 | 答案 |
|---|---|
| hook 原理 | agent 在**生命周期点**执行一条外部命令：**stdin 给 JSON、stdout 回 JSON**，回包里的 `additionalContext` 就是"注入" |
| 多模态模型会被 proxy 搞坏吗 | **取决于 agent 的配置结构**：WorkBuddy/pi 是"每模型一个 url"→ **可以只接管文本模型，图像/音频/视频不受影响**；若用统一 base_url（环境变量）→ **全部受影响** |
| hook 方案对多模态有影响吗 | **完全没有**。hook 只在会话边界触发，**根本不碰模型请求** |

---

## 1 Hook 的技术原理（官方规范）

来源：[Claude Code Hooks reference](https://code.claude.com/docs/en/hooks)

### 1.1 本质：agent 在特定时刻调用你的命令

```
agent 生命周期事件发生
        │
        ▼
按配置的 matcher 过滤（如"只对 Bash 工具"）
        │
        ▼
执行 hook handler（shell 命令 / HTTP 端点 / MCP 工具 / LLM prompt / agent）
        │
        ├── 输入：事件 JSON 从 **stdin** 传入
        └── 输出：命令的 **stdout + exit code** 决定后果
```

### 1.2 配置结构：三级嵌套

```json
{
  "hooks": {
    "SessionStart": [              ← 一级：hook event（生命周期点）
      {
        "matcher": ".*",           ← 二级：matcher group（过滤）
        "hooks": [
          {                        ← 三级：hook handler（实际执行的）
            "type": "command",
            "command": "orbital hook session-start"
          }
        ]
      }
    ]
  }
}
```

### 1.3 五种 handler 类型（官方定义）

| type | 做什么 | 我们能用吗 |
|---|---|---|
| `command` | 跑一条 shell 命令，stdin 收 JSON，stdout 回 JSON | ✅ **最通用**（我们的 CLI 就是这个） |
| `http` | 把事件 JSON **POST** 到 URL | ✅ 我们的 proxy/core 可接收 |
| `mcp` | 调用一个 MCP 工具 | ✅ 直接复用现有 MCP |
| `prompt` / `agent` | 让 LLM/子 agent 处理 | ⚠️ 慢，不适合热路径 |

### 1.4 关键：注入发生在哪

hook 命令 stdout 返回的 JSON 里有：

```json
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "上次你在这里记过：……"   ← ★ 这就是"注入"
  }
}
```

**`additionalContext` 会被 agent 加到当前上下文里**——这是 hook 实现"记忆注入"的唯一机制。

反例（PermissionRequest）：

```json
{ "hookSpecificOutput": { "permissionDecision": "deny" } }
```

**⇒ 所以 hook 能做两类事**：① 往上下文里塞东西（`additionalContext`）② 拦截/放行操作（`permissionDecision`）。
记忆系统只用第 ① 类。

### 1.5 完整流程：以 claude-mem 为例（115k stars 的做法）

```
用户开新一轮会话
        │
        ▼
SessionStart hook 触发
        │  stdin: { session_id, transcript_path, cwd, ... }
        ▼
hook 命令（orbital hook session-start）
        │  ① 用 session_id/cwd 查本地库（上次摘要 + 相关记忆）
        │  ② 按预算裁剪（先给索引，不给全文）
        ▼
stdout: { "hookSpecificOutput": { "additionalContext": "<摘要>" } }
        │
        ▼
agent 把 additionalContext 放进上下文 → 模型"记得上次"
```

采集侧同理：

```
会话结束 / 每次工具调用后
        │
        ▼
Stop / PostToolUse hook 触发
        │  stdin: 含 transcript_path（完整对话落盘路径）
        ▼
hook 命令读 transcript → 压缩成 observation → 写入本地库
```

### 1.6 为什么 hook 比 proxy 更适合做注入

| 维度 | hook | proxy |
|---|---|---|
| 触发点 | 会话边界（SessionStart / Stop） | **每一次模型请求** |
| 改 agent 配置吗 | 只加一条 hook 配置 | **改模型端点** |
| 动模型请求吗 | ❌ 完全不碰 | ✅ 全部经过 |
| 多模态影响 | **无** | 取决于配置（见 §2） |
| 能看到完整响应吗 | ❌（只能看 transcript） | ✅ |
| 单点故障 | hook 挂了 = 少一次注入 | **proxy 挂了 = 对话断** |

---

## 2 多模型（图像/音频/视频）会被 proxy 搞坏吗？

### 2.1 先看你真实的配置结构

**WorkBuddy**（`~/.workbuddy/models.json`）——**每个模型独立 url**：

| 模型 | url | 支持图像 |
|---|---|:--:|
| `agnes-2.0-flash` | `https://apihub.agnes-ai.com/v1` | ❌ |
| `deepseek-v4-pro` | `https://api.deepseek.com/chat/completions` | ❌ |
| `deepseek-flash` | `https://api.deepseek.com/v1` | **✅ 是** |

**pi**（`~/.pi/agent/models-store.json`）——provider 级 `baseUrl` + 模型列表（含 image 类型条目）。

### 2.2 三种情况的准确结论

| 情况 | 后果 | 说明 |
|---|:--:|---|
| **A. 只改文本模型的 url**（WorkBuddy/pi 结构支持） | ✅ **图像/音频/视频不受影响** | 它们各自有独立 url，不经过 proxy |
| **B. 用统一 base_url**（`OPENAI_BASE_URL` 等环境变量） | ❌ **全部模型都走 proxy** | 路由由环境变量统一决定 |
| **C. 改了多模态模型的 url**（如 `deepseek-flash`，支持图像） | ⚠️ **能工作但变差** | proxy 是透明转发，图片请求也能过；但：① 失去注入能力（proxy 不解析多模态）② 多一跳延迟 ③ 构成单点 |

### 2.3 还有两个容易踩的细节

1. **`deepseek-v4-pro` 的 url 是完整路径**（`.../chat/completions`，不是 base）。
   如果把它接管，proxy 收到的请求路径是 `/`，需要把上游记成**完整端点**而不是 base——
   这正是我们 `join_url()` 要处理的情况（已修）。
2. **图像/音频请求体很大**（base64 图片可达数十 MB）。
   如果它们走 proxy，proxy 会把整个 body 读进内存（`Bytes`）——**内存压力与延迟都会明显上升**。
   ⇒ **proxy 不应接管多模态端点**，这是明确的设计约束。

---

## 3 结论与设计含义

### 3.1 回答你的两个问题

**① hook 的原理**：agent 在生命周期点（会话开始/结束/工具调用前后）执行我们的命令，
**stdin 传事件 JSON、stdout 回 `additionalContext`**，agent 把这段文本加进上下文 = 注入；
采集则是读 `transcript_path` 指向的对话文件。

**② 多模态模型**：
- **hook 完全不影响**（不碰模型请求）；
- **proxy 可以做到"只影响文本模型"**（前提：agent 的每个模型有独立 url，WorkBuddy/pi 都是）；
- **但绝不能接管多模态端点**（大 body + 无注入收益 + 单点风险）。

### 3.2 对设计的修正（写进 ADR）

| # | 决策 |
|:--:|---|
| 1 | **hook 是注入主力** —— 它不碰模型请求，天然不影响任何模型类型 |
| 2 | proxy 若启用，**只允许接管"纯文本模型"的 url**；多模态端点必须排除（GUI 上要能勾选） |
| 3 | proxy 必须能处理"上游是完整端点路径"的情况（`join_url` 已覆盖） |
| 4 | proxy 不解析多模态请求体（不做注入），且应限制 body 大小 |

---

## 4 验证计划（下一步）

| # | 事项 | 判据 |
|:--:|---|---|
| 1 | 实测四个 agent 的 hook 点 | 每个 agent 能否配置"会话开始/结束"hook；stdin 能拿到什么字段 |
| 2 | 实测 `additionalContext` 是否真生效 | 配一个 hook 输出固定文本，看下一轮对话里模型是否"看到" |
| 3 | 实测 WorkBuddy 的 Claude-shape hooks | deja-vu 声称共享 hooks，需本机验证 |


---

## 5 实测结果（2026-11-12）：hook 覆盖 **2/4**，比预期悲观

### 5.1 四个 agent 的 hook 现状（本机实测）

| agent | 是否有 hook 机制 | 证据 | 结论 |
|---|:--:|---|---|
| **DSH** | ✅ **有**（核心内置） | 本机存在 `dsh-hooks-claude-code` 能力模块（在 app.asar 内，需用 `cordis.patch.yml` 的 `- insert:` 挂载）；`~/.dsh/AGENTS.md` 记录过"hook 注入必须写 `decision.messages`" | ✅ 可用，但**当前未挂载** |
| **Hermes** | ✅ **有** | `%LOCALAPPDATA%\hermes\config.yaml` L506：`hooks_auto_accept: true` —— 说明存在 hook 审批机制 | ✅ 可用，协议待查 |
| **WorkBuddy** | ❌ **未找到** | `.workbuddy` 下无任何 hook 命名条目；`settings.json` 顶键只有 `enabledPlugins/claw/sandbox/...`；`workbuddy.db` 无 hook 表 | ❌ **可能只支持 MCP** |
| **pi** | ❌ **未找到** | `~/.pi/agent/settings.json` 无 hooks 字段 | ❌ 未发现 |

### 5.2 这意味着什么（对 ADR-025 的修正）

**原结论**（ADR-025）："hook 旁路是注入主力" —— 基于 claude-mem 覆盖 20+ agent 的**外部证据**。

**实测后**：本机四个 agent 里**只有 2 个**有 hook 机制（DSH / Hermes），
WorkBuddy 与 pi **未找到 hook 入口**。

**⇒ ADR-025 需要修正为"按 agent 能力分层"**：

| agent 类型 | 注入方式 | 满足条件 |
|---|---|---|
| 有 hook（DSH / Hermes） | **hook 的 `additionalContext`** | 不改模型配置、不影响多模态 |
| 只有 MCP（WorkBuddy / pi，待进一步确认） | **MCP（pull）** | agent 主动查；**无法自动注入** |
| 无 hook 无 MCP | proxy（可选，代价高） | 最后手段 |

**诚实的结论**：**"自动注入"在 WorkBuddy / pi 上暂时做不到**（除非找到它们的 hook 入口，或接受 proxy 的代价）。

### 5.3 尚未验证的关键点

| # | 待验证 | 为什么重要 |
|:--:|---|---|
| 1 | DSH 挂上 `dsh-hooks-claude-code` 后，`additionalContext` 是否真生效 | 决定 DSH 能否自动注入 |
| 2 | Hermes 的 hook 协议（`hooks_auto_accept` 背后是什么） | 决定 Hermes 能否自动注入 |
| 3 | WorkBuddy 的 Claude-shape hooks 是否存在于**其他目录**（deja-vu 声称有） | 若存在则 WorkBuddy 也能自动注入 |
| 4 | pi 是否有扩展机制可做注入 | 同上 |

**⚠️ 这 4 项都还没验**——在验证前，不应声称"hook 方案已成立"。

---

## 6 对产品承诺的影响（必须诚实）

| 原承诺 | 实测后的真实情况 |
|---|---|
| "4 个 agent 都能自动注入记忆" | **只能承诺 2 个**（DSH / Hermes，且尚未验证 `additionalContext` 生效） |
| "不改 agent 配置就能注入" | 对 WorkBuddy / pi **做不到**（它们要么只支持 MCP，要么需要 proxy） |
| "统一管理 4 个 agent 的记忆" | ✅ 这个能承诺：**写入与查询**（MCP）4 个都通，已实测 |

**⇒ 必须把"自动注入"和"统一存取"分开承诺**：
- **统一存取（MCP）**：4/4 已实现并验证 ✅
- **自动注入（hook）**：2/4 有机制，**0/4 已验证生效** ⚠️
