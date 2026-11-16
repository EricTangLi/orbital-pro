# WorkBuddy 的第三条注入路：MEMORY.md（官方文档确认）

> 触发：用户要求"扩大调研范围，找开源项目和官方技术文档的明确方案"
> 结果：**找到了 WorkBuddy 官方的记忆注入机制**——不是 hook，是 **`~/.workbuddy/MEMORY.md` 自动注入**
> 日期：2026-11-12

---

## 0 结论

| 路径 | 可行性 | 依据 |
|---|:--:|---|
| hook（`~/.codebuddy/settings.json`） | ❌ **实测不支持**（桌面版不读） | 上一轮红绿对照 |
| **MEMORY.md 自动注入** | ✅ **官方文档明确说明** | 见下 |
| MCP | ✅ 已实测 | 4/4 存取 |
| proxy 改 base_url | ⚠️ 技术可行 | 丢官方积分 |

**⇒ WorkBuddy 的自动注入有第三条路：往 `~/.workbuddy/MEMORY.md` 写。**

---

## 1 官方文档原文（腾讯云开发者社区，WorkBuddy 官方产品文）

### 1.1 机制一：每次会话自动注入

> "助手支持把一份 Markdown 记忆文件在**每次会话开始时自动注入上下文**。
> 相当于给 AI 配了一份『员工手册』，你的偏好、规矩、红线，写一次就永久生效。"
>
> —— [让 AI 助手"记住你"：三层记忆体系实战](https://cloud.tencent.cn/developer/article/2762198)（2026-10-15）

### 1.2 机制二：会话结束写入、下次启动读取

> "WorkBuddy 使用本地 Markdown 文件存储工作记忆，路径为：
> `工作区/.workbuddy/memory/YYYY-MM-DD.md` # 每日日志
> `工作区/.workbuddy/memory/MEMORY.md` # 长期记忆
> **每次会话结束后，WorkBuddy 会把重要信息写入记忆文件，下次启动时自动读取。**"
>
> —— [WorkBuddy知识库与记忆系统深度使用](https://cloud.tencent.cn/developer/article/2671974)（2026-05-21）

### 1.3 四件套的分工

| 文件 | 作用 | 读取时机 |
|---|---|---|
| `SOUL.md` | 价值观、边界、调性（"人格 DNA"） | **每次会话开始** |
| `IDENTITY.md` | 名字、角色、emoji | 特定时机 |
| `USER.md` | 用户档案（姓名/城市/偏好） | 特定时机 |
| **`MEMORY.md`** | **跨会话长期记忆** | **每次会话自动注入** |

来源：[驯服WorkBuddy（配置文件说明）](https://cloud.tencent.com.cn/developer/article/2658956)

### 1.4 还有配套的"记忆治理"

> 全书目录："第4章记忆系统：三层存储与五类内容 / 4.2 存储位置与文件形态 / **4.3 治理规则：限额、蒸馏与覆盖**"
>
> —— WorkBuddy Harness 电子书

**⚠️ 风险提示**："限额、蒸馏与覆盖" 意味着：
- MEMORY.md 有**容量上限**；
- WorkBuddy 会**蒸馏**（压缩/合并）内容；
- 可能**覆盖**我们写入的东西。

⇒ **不能把 orbital 的完整记忆塞进去**，只能塞"索引/摘要/指引"。

---

## 2 本机实证

### 2.1 文件存在且是"真实使用中"的

```
~/.workbuddy/MEMORY.md   6407 B
内容示例：
  # 用户级长期记忆（跨项目）
  ## WorkBuddy 删除防护（safe-delete）环境特性
  ## 浏览器收藏夹整理：同步会覆盖外部文件修改（2026-09-16 实测）
  ## Node / npm 在本机的安装坑（2026-09-15 实测）
  ## tauri build 经验（2026-07-24 重大修正）
```

**这些是 WorkBuddy 自己积累的跨项目技术记忆**——证明该文件确实被使用和维护。

### 2.2 已做测试（等用户验证）

1. 备份：`~/.workbuddy/MEMORY.md.bak.orbital-test`（6407 B，SHA256 前16位 `003B87A5BA5D1327`）
2. 追加到末尾：

```markdown
## Orbital Pro 记忆接入测试（2026-11-12）
- **测试码：ORANGE-42**
- 这条来自 Orbital Pro 的注入机制验证。若你在本次会话中能读到这条，
  说明 WorkBuddy 的 MEMORY.md 自动注入生效。
```

3. 文件从 6407 → 6633 B

**待用户验证**：在 WorkBuddy **新开一个会话**，问「测试码是什么？」

---

## 3 如果验证成功，方案怎么做

### 3.1 不侵入的写法（推荐）

在 MEMORY.md **末尾保留一个受控区块**，只放"索引 + 调用指引"，不放全文：

```markdown
<!-- orbital:begin（Orbital Pro 自动维护，勿手工改） -->
## Orbital 记忆索引（自动生成）
- 我的跨 agent 记忆库在 Orbital，可用 MCP 工具 `memory_recall` 查询
- 最近相关记忆（跨 agent 共享）：
  1. <标题> — <20 字摘要>（agent=dsh, 2026-11-12）
  2. ...
- 需要细节时调用 `memory_recall(query)` 或 `memory_get(id)`
<!-- orbital:end -->
```

**理由**：
1. **只放索引**——避开"限额/蒸馏"风险（全文会让文件膨胀被压缩掉）；
2. **受控区块**——用 HTML 注释标记边界，下一次写入只替换这一段，不碰用户自己的记忆；
3. **同时给调用指引**——即使索引被蒸馏，agent 仍知道"去 Orbital 查"。

### 3.2 写入时机

| 时机 | 做法 |
|---|---|
| Orbital 写入新记忆后 | 更新 MEMORY.md 的 orbital 区块（摘要+计数） |
| WorkBuddy 会话开始前 | 无法拦截（没有 hook）→ 只能"上次写完就在那" |

**⇒ 所以是"滚动更新"而非"实时注入"**：agent 看到的是**上次**同步时的索引。
对"跨会话记忆"这个用途，已经够用。

### 3.3 与 MCP 配合

```
MEMORY.md 区块（索引 + 指引）
        ↓ agent 看到
agent 决定"需要细节" → 调 MCP memory_recall
        ↓
拿到完整记忆
```

**这就是"索引 + 按需拉取"两层结构**——正是 claude-mem 推荐的"注入索引而不是全文"。

---

## 4 仍需验证的 3 件事

| # | 待验证 | 风险 |
|:--:|---|---|
| 1 | **MEMORY.md 是否真的每次会话注入**（用户测试中） | 若否，这条路也不成立 |
| 2 | WorkBuddy 的"蒸馏"会不会删掉我们的区块 | 需要观察几天 |
| 3 | 写入时 WorkBuddy 恰好在跑，会不会冲突 | 需要原子写（temp + rename） |

---

## 5 证据来源

| 证据 | URL |
|---|---|
| 三层记忆体系实战（官方产品文） | https://cloud.tencent.cn/developer/article/2762198 |
| 知识库与记忆系统深度使用 | https://cloud.tencent.cn/developer/article/2671974 |
| 驯服WorkBuddy（配置文件说明） | https://cloud.tencent.com.cn/developer/article/2658956 |
| WorkBuddy Harness 电子书（含第4章记忆系统目录） | GitHub release assets（1335574883） |
| 开源学习项目（SKILL.md read_when 解析） | https://github.com/adongwanai/learn-workbuddy |


---

## 6 ✅ 验证成功（2026-11-12 11:00，决定性证据）

### 6.1 测试过程（两次，第一次失败）

**第一次（失败）**：把测试内容写到 `~/.workbuddy/MEMORY.md` 的**末尾**（L64）。
→ WorkBuddy 回答"查了一圈，没有上下文指向测试码"。

**错在哪**（两个）：
1. **选错文件**：`user-<uid>-personal/MEMORY.md`（7837 B，10/8）才是**更新的版本**
   （比根目录那个多一段"pi 的两条铁律"），根目录的是旧版（6407 B，9/16）；
2. **写错位置**：放在 6407 B 文件的**末尾**，若只读前 N 字节则永远看不到。

**第二次（成功）**：把测试内容同时插入**两个文件的 L3（开头）**。

### 6.2 WorkBuddy 的原话（决定性证据）

> "**测试码是 ORANGE-42**。（这是你**个人空间 MEMORY.md** 里 'Orbital Pro 注入测试' 条目记的码——能读到就说明注入生效了。）"

**它自己指出了是"个人空间 MEMORY.md"** —— 即：

```
~/.workbuddy/user-<uid>-personal/MEMORY.md
```

### 6.3 结论

| 项 | 结果 |
|---|---|
| WorkBuddy 自动注入 | ✅ **成立** |
| **正确路径** | **`~/.workbuddy/user-<uid>-personal/MEMORY.md`** |
| 读取时机 | 每次会话开始（官方文档 + 实测双重确认） |
| 是否影响官方模型/积分 | ❌ **完全不影响**（不碰 models.json） |
| 是否需要 hook | ❌ **不需要**（桌面版本就不支持 hook） |

### 6.4 三个 MEMORY.md 的最终定位

| 路径 | 大小 | 作用 | 是否注入 |
|---|---:|---|:--:|
| `~/.workbuddy/user-<uid>-personal/MEMORY.md` | 7837 B | **个人空间记忆（实际生效）** | ✅ **是** |
| `~/.workbuddy/MEMORY.md` | 6407 B | 用户级（旧版，可能是早期迁移残留） | 待确认 |
| `~/.workbuddy/memory/<uid>_memory.md` | 10215 B | 用户画像（Version 154，自动生成） | 不同机制 |

### 6.5 环境已还原

| 文件 | 还原后大小 | SHA256（前16位） |
|---|---:|---|
| `user-<uid>-personal/MEMORY.md` | 7837 B | `2A1352F81B50A085` |
| `~/.workbuddy/MEMORY.md` | 6407 B | `003B87A5BA5D1327` |

ORANGE-42 已从两个文件清除；`.bak.orbital-test` 备份保留。

---

## 7 正式方案（验证成功后的落地设计）

### 7.1 写入位置

`~/.workbuddy/user-<uid>-personal/MEMORY.md` 的**末尾**，用受控区块：

```markdown
<!-- orbital:begin (由 Orbital Pro 自动维护，勿手工编辑) -->
## Orbital 记忆索引（自动生成 · 2026-11-12 11:05）

跨 agent 记忆库在 Orbital。需要细节时用 MCP 工具 `memory_recall(query)` 或 `memory_get(id)`。

最近相关记忆：
1. 双端点嵌入一致性验证通过（余弦 0.9992）— dsh, 11-12
2. WorkBuddy 注入路径确认（个人空间 MEMORY.md）— dsh, 11-12
3. Proxy URL 拼接 bug 修复 — dsh, 11-12

<!-- orbital:end -->
```

### 7.2 三条硬规则

| # | 规则 | 理由 |
|:--:|---|---|
| 1 | **只放索引 + 指引，不放全文** | 官方有"限额、蒸馏与覆盖"治理；全文会让文件膨胀被压缩掉 |
| 2 | **受控区块用 `<!-- orbital:begin/end -->` 标记** | 下次写入只替换这一段，**绝不碰用户自己的记忆** |
| 3 | **原子写**（temp + rename） | WorkBuddy 可能同时在读写 |

### 7.3 写入时机

| 时机 | 做法 |
|---|---|
| Orbital 有新记忆写入后 | 更新区块（摘要 + 计数 + 时间戳） |
| 用户手动点"同步到 WorkBuddy" | 同上 |
| WorkBuddy 会话开始前 | **无法拦截**（无 hook）→ 依赖"上次写完就在那" |

**⇒ 是"滚动更新"而非"实时注入"**。对跨会话记忆够了。

### 7.4 与 MCP 的两层配合

```
MEMORY.md 区块（索引 + 指引）   ← agent 每次会话必读
        ↓
agent 觉得需要细节 → 调 MCP memory_recall
        ↓
拿到完整记忆（含向量召回）
```

这正是 claude-mem 的"注入索引而不是全文"策略。

### 7.5 四 agent 的最终接入矩阵

| agent | 自动注入方式 | 落地路径 |
|---|---|---|
| **DSH** | hook（`dsh-hooks-claude-code`） | 待挂载验证 |
| **Hermes** | hook（`hooks_auto_accept`） | 协议待查 |
| **pi** | 扩展（`before_agent_start` 返回 `{message}`） | `~/.pi/agent/extensions/*.ts` |
| **WorkBuddy** | **MEMORY.md 自动注入** | **`~/.workbuddy/user-<uid>-personal/MEMORY.md`** |

**4/4 都有自动注入路径，且都不是 proxy！**
