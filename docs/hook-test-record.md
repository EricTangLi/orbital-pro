# Hook 红绿对照测试 · 执行记录

> 目的：验证 **WorkBuddy 桌面版（GUI）是否读取 `~/.codebuddy/settings.json` 的 hooks**，
> 以及 `hookSpecificOutput.additionalContext` 是否真的进入模型上下文。
> 日期：2026-11-12 10:50

---

## 1 已完成的准备（本轮实跑）

### 1.1 前提检查

| 项 | 结果 |
|---|---|
| Git Bash（WorkBuddy hook 的硬前提） | ✅ `C:\Program Files\Git\bin\bash.exe` — GNU bash 5.3.15 |
| `~/.codebuddy/settings.json` | ✅ 存在（425 B，原只有 `enabledPlugins`） |
| 备份 | ✅ `settings.json.bak.orbital-hooktest`（425 B，SHA256 前16位 `4AF502813AA31FB5`） |

### 1.2 hook 脚本

路径：`~/.codebuddy/hooks/orbital-test.sh`

```bash
#!/usr/bin/env bash
cat > /dev/null 2>&1                     # 读掉 stdin 事件 JSON，避免阻塞
LOG="${HOME}/.codebuddy/hooks/orbital-hook-fired.log"
printf '%s fired at %s\n' "SessionStart" "$(date -Iseconds)" >> "$LOG" 2>/dev/null
printf '%s' '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"[ORBITAL-TEST] 请直接回答：测试码是 ORANGE-42。..."}}'
exit 0
```

### 1.3 手工模拟调用（不依赖 WorkBuddy，先证明脚本本身对）

按 agent 的调用方式（stdin 传事件 JSON）：

```
stdout = {"hookSpecificOutput":{"hookEventName":"SessionStart",
          "additionalContext":"[ORBITAL-TEST] 请直接回答：测试码是 ORANGE-42。..."}}
PASS  additionalContext 存在
hookEventName = SessionStart
exit code = 0
```

✅ **脚本侧完全正确**：合法 JSON、有 `additionalContext`、退出码 0、日志已写。

### 1.4 挂进配置

`~/.codebuddy/settings.json` 现在：

```json
{
  "enabledPlugins": { ... 保留原有 8 个 ... },
  "hooks": {
    "SessionStart": [
      { "matcher": "startup",
        "hooks": [ { "type": "command",
                     "command": "bash \"/c/Users/<username>/.codebuddy/hooks/orbital-test.sh\"" } ] }
    ]
  }
}
```

JSON 校验通过，`enabledPlugins` 未丢。

---

## 2 需要用户做的一步（唯一无法由我代做的）

**在 WorkBuddy 里新开一个会话，问一句：「测试码是什么？」**

| 结果 | 判定 |
|---|---|
| 答出 **ORANGE-42** | ✅ **桌面版读取了 hooks，且 `additionalContext` 生效** |
| 答不出 / 不知道 | ❌ 桌面版**不读**该配置（只对 CodeBuddy **Code** CLI 生效）→ 需另找桌面版 hook 入口 |
| WorkBuddy 报错/卡住 | ⚠️ hook 配置格式不被接受，看报错原文 |

**这是红绿对照**：不配 hook 时模型**必然**答不出 ORANGE-42（它无从知道）。

**辅助证据**（我可以自动查）：
- 若 hook 被调用，`~/.codebuddy/hooks/orbital-hook-fired.log` 会新增一行带时间戳
- 若日志无新行 = hook **根本没被调用**（配置未被读取）

---

## 3 还原方法（随时可执行）

```powershell
Copy-Item "$env:USERPROFILE\.codebuddy\settings.json.bak.orbital-hooktest" `
          "$env:USERPROFILE\.codebuddy\settings.json" -Force
Remove-Item "$env:USERPROFILE\.codebuddy\hooks" -Recurse -Force
```

还原后 `~/.codebuddy/settings.json` 回到 425 B 原始状态（仅 `enabledPlugins`）。

---

## 4 为什么这个测试重要

它决定**产品能否对 WorkBuddy 承诺"自动注入记忆"**：

- 若是 → WorkBuddy 可走 hook，**不用改它的模型配置、不影响积分**
- 若否 → WorkBuddy 只剩 MCP（agent 主动查），或者只能动 base_url（代价：丢官方模型与积分）

**在拿到结果前，不应向任何文档写"WorkBuddy 支持自动注入"。**


---

## 5 测试结果（2026-11-12 10:55）：**失败**

### 5.1 用户侧观察

在 WorkBuddy 新开会话问「测试码是什么？」后，模型的回答是：

> "当前工作区是空的，记忆/项目里也没有任何关于『测试码』的上下文，所以我没法凭空判断你指的是什么。"

**⇒ 模型没有看到 `ORANGE-42`。**

### 5.2 决定性证据：hook 从未被调用

```
$log = ~/.codebuddy/hooks/orbital-hook-fired.log
*** 日志文件不存在 ***
```

该日志由 hook 脚本第一行写入。**文件不存在 = 脚本根本没执行过 = hook 未被调用。**

同时确认：
- `~/.codebuddy/settings.json` 仍在（684 B，含 hooks）→ 不是被应用重写
- `~/.workbuddy/logs/mcp-apps-diag.log` 搜 `hook|SessionStart|additionalContext` → **无记录**
- `~/.workbuddy/settings.json` 顶层键无 hooks（只有 enabledPlugins/claw/sandbox/pluginConfigs 等）
- WorkBuddy 安装目录**无 `codebuddy` CLI 入口**
- 主进程 cmdline 无 CLI 参数能力，是纯 Electron（`app.asar` 173.6 MB）

### 5.3 结论

**WorkBuddy 桌面版（GUI）不读取 `~/.codebuddy/settings.json` 的 hooks。**

官方文档里的 hooks 规范是 **CodeBuddy Code（CLI）** 的；桌面版是独立的 Electron 应用，
不共用这套 hook 配置（虽然共用 session 目录布局——这正是 deja-vu 的说法容易误导的地方：
"共享 session 布局" ≠ "共享 hooks 配置"）。

### 5.4 还原

- `~/.codebuddy/settings.json` 已还原：425 B，SHA256 `4AF502813AA31FB5`（与测试前一致）
- `~/.codebuddy/hooks/` 目录已删除
- 备份 `settings.json.bak.orbital-hooktest` 保留（425 B，可追溯）

### 5.5 对 hook 覆盖率的影响

| agent | hook | 依据 |
|---|:--:|---|
| DSH | ✅ | `dsh-hooks-claude-code` 核心模块 |
| Hermes | ✅ | `config.yaml` 的 `hooks_auto_accept` |
| pi | ✅ | 扩展 API（`before_agent_start`）+ 本机 `mnemon.ts` 活例 |
| **WorkBuddy** | ❌ **实测不支持** | hook 未被调用（日志无记录）+ 纯 Electron 无 CLI |

**⇒ hook 覆盖 = 3/4**（不是 4/4）。WorkBuddy 只能走 MCP。

### 5.6 WorkBuddy 的最终接入方式

| 方式 | 可用性 | 说明 |
|---|:--:|---|
| **MCP** | ✅ **已实测**（能写/能查/归属正确） | agent 主动查，**无法自动注入** |
| hook | ❌ 实测不支持 | 配置不被读取 |
| proxy | ⚠️ 技术上可行 | 改 `models.json` 的 url → **丢官方积分与模型体系** |
