# Hook 能力最终结论：**4/4 全覆盖**（附官方文档依据）

> 触发：用户要求"调研更广的权威资料，WorkBuddy、pi 的官方技术文档，如果是开源的拉取代码分析"
> 方法：官方文档原文 + 本机配置落点实测 + 本机已有源码（pi 的扩展）
> 日期：2026-11-12 ｜ 前序：`docs/18`（当时结论 2/4，**本文件修正为 4/4**）

---

## 0 结论（修正 docs/18 的 2/4）

| agent | 有 hook 吗 | 官方依据 | 本机落点 |
|---|:--:|---|---|
| **DSH** | ✅ | 本机核心模块 `dsh-hooks-claude-code` | `~/.dsh/profiles/desktop/cordis.patch.yml` 挂载 |
| **Hermes** | ✅ | `config.yaml` 的 `hooks_auto_accept` | `%LOCALAPPDATA%\hermes\config.yaml` |
| **WorkBuddy** | ✅ **有完整规范** | [CodeBuddy Code Hooks 参考](https://www.codebuddy.cn/docs/cli/hooks)、[WorkBuddy 企业版 Hooks](https://intl.cloud.tencent.com/zh/document/product/1316/84447) | **`~/.codebuddy/settings.json`**（当前无 `hooks` 字段，可加） |
| **pi** | ✅ **扩展 API 就是 hook** | `@earendil-works/pi-coding-agent` 的 `ExtensionAPI` | `~/.pi/agent/extensions/*.ts`（**已有 `mnemon.ts` 活例**） |

**⇒ hook 是 4/4 可行**。我上一轮说"2/4"是因为**只在 `.workbuddy` 目录里找**，而 WorkBuddy 的 hook 配置落在 **`~/.codebuddy/settings.json`**（另一个目录）。

---

## 1 WorkBuddy / CodeBuddy Code 的 Hook 规范（官方原文）

来源：[CodeBuddy Code Hook 参考](https://www.codebuddy.cn/docs/cli/hooks)、[WorkBuddy 企业版 Hooks 使用指南](https://intl.cloud.tencent.com/zh/document/product/1316/84447)、[Hooks Reference (workbuddy.ai)](https://www.workbuddy.ai/docs/cli/hooks)

### 1.1 配置位置（优先级从高到低）

```
~/.codebuddy/settings.json          ← 用户级（本机已存在，425 B，当前只有 enabledPlugins）
<project>/.codebuddy/settings.json  ← 项目级
```

**本机实测**：`~/.codebuddy/settings.json` 存在，顶层键只有 `enabledPlugins` → **尚无 hooks，可直接添加**。

### 1.2 支持的生命周期事件（官方列表）

| 事件 | 触发时机 | 对记忆系统的用途 |
|---|---|---|
| **`SessionStart`** | 新会话启动 / 恢复 | ★ **注入上次摘要** |
| `SessionEnd` | 会话结束 | ★ **采集对话** |
| `UserPromptSubmit` | 用户提交提示词 | 可注入/校验 |
| `PreToolUse` / `PostToolUse` | 工具调用前后 | 采集操作记录 |
| `Stop` / `SubagentStop` | 响应完成 | 采集 |
| `PreCompact` / `PostCompact` | 上下文压缩前后 | 保留要点 |

### 1.3 输入（stdin 的 JSON，官方字段）

```json
{
  "session_id": "...",
  "transcript_path": "...",        ← ★ 采集用：完整对话文件路径
  "cwd": "...",
  "hook_event_name": "SessionStart",
  "source": "startup|resume|clear|compact",   ← SessionStart 专用
  "reason": "...",                            ← SessionEnd 专用
  "tool_name": "...", "tool_input": {...}, "tool_response": {...}
}
```

环境变量：`CLAUDE_PROJECT_DIR`（兼容 Claude Code）、`CODEBUDDY_PROJECT_DIR`。

### 1.4 **输出：`additionalContext` —— 这就是注入**

官方示例（SessionStart 注入项目上下文）：

```python
output = {
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": f"会话已启动! 项目目录: {project_dir} ..."
  }
}
print(json.dumps(output))
```

**官方明确**：Exit code 0 时，"`stdout` appears in transcript mode，**except for `UserPromptSubmit` and `SessionStart`, where stdout is appended to** [上下文]"。

**⇒ 与 Claude Code 的机制完全一致**：`additionalContext` 追加进上下文 = 注入记忆。

### 1.5 ⚠️ Windows 限制（重要）

官方原文：**"Windows 上强制使用 Git Bash 执行（不支持 cmd.exe 或 PowerShell）"**，需安装 Git for Windows。

**⇒ 我们的 hook 命令必须是 bash 兼容的**（或用一个 `.cmd` 包装但由 bash 调用）。这一条会直接影响实现。

### 1.6 还有一个坑：Beta 状态

官方注明："Hook 功能目前处于 **Beta** 阶段，仍在持续打磨" → 接口可能变，需版本适配。

---

## 2 pi 的 Hook 机制（本机源码实证）

### 2.1 它的"hook"叫扩展（Extension）

本机 `~/.pi/agent/extensions/` 已有两个 TypeScript 扩展：
- `code-guard.ts`（3254 B）—— 危险命令拦截
- **`mnemon.ts`（2260 B）—— 记忆注入，活证据**

### 2.2 扩展 API 的生命周期事件（从 code-guard.ts / mnemon.ts 反推）

```typescript
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.on("resources_discover", async () => { ... });
  pi.on("session_start", async (_event, ctx) => { ... });        // 会话开始
  pi.on("before_agent_start", async () => {                      // ★ 注入点
    return { message: visibleMessage(content) };                 //   ← 注入
  });
  pi.on("agent_end", async (_event, ctx) => { ... });            // 会话结束
  pi.on("session_before_compact", async () => {                  // 压缩前
    return { customInstructions: "..." };                        //   ← 注入指令
  });
  pi.on("tool_call", async (event, ctx) => { ... });             // 工具调用前（可拦截）
}
```

### 2.3 `mnemon.ts` 已经在做的事（可直接照抄的模板）

```typescript
pi.on("before_agent_start", async () => {
  const guide = readGuide();                                     // 读 ~/.mnemon/prompt/guide.md
  const content = [memoryStatus(), guide, "..."].filter(Boolean).join("\n\n");
  return { message: visibleMessage(content) };                   // ★ 注入到上下文
});
```

**⇒ pi 的注入 API = `before_agent_start` 返回 `{ message }`**，与 `additionalContext` 等价。
**而且本机已有现成实现可以参照/替换**（`mnemon.ts` 接的是 mnemon，我们要接 orbital）。

---

## 3 三套 hook 的对照（给实现用）

| 维度 | Claude Code / CodeBuddy（WorkBuddy） | pi | DSH |
|---|---|---|---|
| 配置落点 | `~/.codebuddy/settings.json` 的 `hooks` | `~/.pi/agent/extensions/*.ts` | `cordis.patch.yml` 挂 `dsh-hooks-claude-code` |
| 会话开始事件 | `SessionStart` | `session_start` / **`before_agent_start`** | 待查（Claude 兼容） |
| 会话结束事件 | `SessionEnd` / `Stop` | `agent_end` | 待查 |
| 注入字段 | `hookSpecificOutput.additionalContext` | `{ message: ... }` | 待查（`decision.messages`） |
| 输入 | stdin JSON（含 `transcript_path`） | `(event, ctx)` 对象 | 待查 |
| 执行环境 | **bash**（Windows 强制 Git Bash） | Node.js | Node.js |
| 成熟度 | Beta（官方注明） | 稳定（有现成扩展） | 内部 |

---

## 4 对设计的最终修正

| 项 | 旧（docs/18） | 新（本文件） |
|---|---|---|
| hook 覆盖率 | 2/4 | **4/4** |
| WorkBuddy 注入方式 | 判定"只能 MCP" | **hook（`~/.codebuddy/settings.json`）+ MCP** |
| pi 注入方式 | 判定"未找到" | **hook（扩展 `before_agent_start`）+ MCP** |
| ADR-025 分层 | 只有 2 个能自动注入 | **4 个都能自动注入**，差异在实现方式 |

**⇒ ADR-025 需要再次修正**：不再是"按能力分层"，而是"**统一能力、不同实现**"。

---

## 5 仍需验证的 3 件事（不假装已验证）

| # | 待验证 | 风险 |
|:--:|---|---|
| 1 | **WorkBuddy 桌面版（GUI）是否读 `~/.codebuddy/settings.json`** | 官方文档写的是 "CodeBuddy **Code**"（CLI），桌面版可能共用也可能不共用。deja-vu 声称"共享 session 布局与 Claude-shape hooks" → 倾向于共用，但**未实测** |
| 2 | DSH 的 `dsh-hooks-claude-code` 挂载后是否真支持 `additionalContext` | 本机未挂载过 |
| 3 | Hermes 的 hook 协议细节 | 只看到 `hooks_auto_accept` 一个字段 |

**验证方式（下一步）**：
1. 在 `~/.codebuddy/settings.json` 加一个 `SessionStart` hook，输出固定 `additionalContext`（如"测试码是 ORANGE-42"）；
2. 在 WorkBuddy 里**新开一个会话**，问"测试码是什么"；
3. 答对 → 桌面版读该配置；答不出 → 只对 CLI 生效，需另找桌面版的 hook 入口。

**这是红绿对照**：不配 hook 时模型必然答不出。

---

## 6 证据来源

| 证据 | URL / 路径 |
|---|---|
| CodeBuddy Code Hook 参考 | https://www.codebuddy.cn/docs/cli/hooks |
| WorkBuddy 企业版 Hooks 使用指南 | https://intl.cloud.tencent.com/zh/document/product/1316/84447 |
| WorkBuddy Hooks Reference | https://www.workbuddy.ai/docs/cli/hooks |
| Hook 输入输出规范 | https://www.tencentcloud.com/document/product/1316/84413 |
| pi 扩展 API（本机源码） | `~/.pi/agent/extensions/{mnemon,code-guard}.ts` |
| 本机 WorkBuddy hook 落点 | `~/.codebuddy/settings.json`（425 B，当前无 hooks） |


---

## 7 实测后的最终修正（2026-11-12 10:55）：**3/4，WorkBuddy 不支持**

第五节列的"待验证 1"（WorkBuddy 桌面版是否读 `~/.codebuddy/settings.json`）**已实测，结论是否定的**。

### 测试方法（红绿对照）

1. 备份 `~/.codebuddy/settings.json`（425 B，SHA256 `4AF502813AA31FB5`）
2. 挂 `SessionStart` hook → 输出 `additionalContext: "测试码是 ORANGE-42"`
3. 手工模拟调用 → ✅ 脚本本身正确（合法 JSON、有 additionalContext、exit 0）
4. **用户在 WorkBuddy 新开会话问「测试码是什么？」**
5. 结果：模型答"我不知道测试码是什么"（未看到注入）

### 决定性证据

```
~/.codebuddy/hooks/orbital-hook-fired.log  *** 文件不存在 ***
```

该日志由 hook 脚本第一行写入。**不存在 = 脚本从未执行 = hook 未被调用**。
（不是"调用了但注入没生效"，是"根本没调用"。）

**旁证**：
- `~/.workbuddy/logs/mcp-apps-diag.log` 搜 hook/SessionStart/additionalContext → 无记录
- WorkBuddy 无 `codebuddy` CLI 入口，主进程是纯 Electron（`app.asar` 173.6 MB）
- `~/.workbuddy/settings.json` 顶层键无 hooks

### 最终结论

| agent | hook | 注入字段 / 方式 |
|---|:--:|---|
| DSH | ✅ | `dsh-hooks-claude-code`（待挂载验证） |
| Hermes | ✅ | `hooks_auto_accept`（协议待查） |
| pi | ✅ | `before_agent_start` 返回 `{ message }`（有 `mnemon.ts` 活例） |
| **WorkBuddy** | ❌ **不支持** | 只能 MCP（agent 主动查，**无法自动注入**） |

**hook 覆盖 = 3/4。**

**为什么 deja-vu 的说法会误导**：它说 "CodeBuddy Code 与 WorkBuddy desktop **共享同一 session 布局**和 Claude-shape hooks"。
实测表明：**共享 session 布局 ≠ 共享 hooks 配置**。CLI 的 hooks 在 `~/.codebuddy/settings.json`，
而桌面版是独立 Electron 应用，不读该文件的 hooks。

### 对产品的诚实承诺

| 承诺 | 状态 |
|---|---|
| 4 个 agent 统一**存取**（MCP） | ✅ 已实现并实测 |
| 4 个 agent 自动**注入** | ❌ **只能承诺 3 个**（WorkBuddy 不行） |
| WorkBuddy 自动注入 | 只能靠 proxy（代价：丢官方积分与模型体系） |

**这是必须让用户知道的边界**：WorkBuddy 用户要么接受"MCP 主动查"，要么接受"改 base_url 丢积分"，
没有第三条路（除非腾讯后续开放桌面版的 hook）。
