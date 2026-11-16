# 多 agent 记忆注入产品化方案（装上即用 · 一键接入 · 共享）

> 触发：用户 2026-11-14 明确目标——"Orbital 安装后进行便捷配置（最好一键连接），agent 就能配好并正常使用，
> 成为完整、全面、稳定、准确的记忆系统，并实现多 agent 记忆共享"。
> 本文基于**三路源码级调研**（DSH / Hermes / WorkBuddy / pi）+ 本机实测，给出可落地的产品方案。
>
> 证据标注：**【实测】**=本轮跑出来的；**【实读】**=读源码/配置；**【官方】**=官方文档；**【未核实】**。

---

## 0 一页结论

| 问题 | 答案 |
|---|---|
| Orbital 能服务 4 个 agent 吗 | **能**，但每个 agent 的"自动注入"通道**各不相同**，必须各写一个薄适配器（MCP 只管主动存取，管不了自动注入） |
| 现在能用吗 | **一半能用**：MCP 4/4 已接（写入与深查正常）；自动注入 **0/4**——DSH 没写、Hermes 没配、pi 没写、WorkBuddy **区块被截断失效** |
| 一键接入可行吗 | **可行**：4 套配置的写入逻辑高度同构（都是"备份→写配置→验证"），可做成一个按钮 + 一张状态表 |
| 最大风险 | **静默失效**（WorkBuddy 4000 字符截断、MCP 未审批、hook 超时）——必须每通道都能自证"我活着" |

---

## 1 四套注入机制（源码级实测，这是方案的地基）

| agent | 注入通道 | 落点 | 硬上限 | 触发时机 | 失败行为 |
|---|---|---|---|---|---|
| **DSH** | 插件/provider | `~/.dsh/profiles/desktop/node_modules/`（provider 包）或自写插件 | 12,000 字符（mnemon 配置）；实测每轮 ~7.5k | 每轮 step 1（`agent/pre-step` → `decision.messages`） | fail-open（异常吞掉，轮次继续） |
| **Hermes** | **shell hook `pre_llm_call`** | `%LOCALAPPDATA%\hermes\config.yaml` 的 `hooks:` + `agent-hooks/*.py` | 无硬上限（超 10,000 字符 spill 到文件） | **每轮**（API 调用前） | fail-open（`_BLOCKING_EVENTS` 只有 pre_tool_call） |
| **pi** | **扩展 `before_agent_start`** | `~/.pi/agent/extensions/orbital.ts` | 无硬上限（自己控预算） | 每 run | fail-open（`runner.js` catch 后继续） |
| **WorkBuddy** | **MEMORY.md 文件**（唯一可用） | `~/.workbuddy/user-<uid>-personal/MEMORY.md` | **4,000 字符**（源码 `let r=4e3`） | 每 run 启动（`startup-diag` → `template.compileRender` 前） | fail-open（读失败返回空串） |

**共同点**：全部 **fail-open**、全部**不改模型 API**、全部有"每轮/每 run"注入点。
**差异**：只有 WorkBuddy 是"文件注入"（有硬上限），其余三家是"代码注入"（无硬上限，但要写各自方言）。

### 1.1 一条被推翻的旧结论（重要）

`docs/20` 记的"WorkBuddy MEMORY.md 注入已实测生效"**在今天就失效了**：

```
MEMORY.md 实测 6,619 字符（有效）> 4,000（源码上限）
Orbital 区块起始位置 = 第 4,974 字符
⇒ 截断在 4,000 处，Orbital 区块被整体丢弃
```

**当时为什么测成功**：那一次文件约 3,900 字符、标记插在 L3 开头——**刚好在限额内**。
不是当时测错，是"上限"这个约束当时没被识别。

---

## 2 当前状态：三个正在发生的故障点

| # | 故障 | 证据 | 影响 |
|:--:|---|---|---|
| 1 | **WorkBuddy 注入已被截断** | 文件 6,619 字符 > 4,000；区块在第 4,974 字符【实测】 | 用户以为"接了"，实际什么都没注入 |
| 2 | **WorkBuddy 的 orbital MCP 从未加载** | `~/.workbuddy/mcp-approvals.json` = `{}`；`mcp-tool-list.json` 里 7 个 server 无 orbital【实测】 | WorkBuddy 连"主动查"都没有 |
| 3 | **Hermes MEMORY.md 已超限** | `2231/2200`，`agent.log` 有 WARNING【实测】 | Hermes 自己的记忆写入被堵死 |

---

## 3 目标架构：一库 + 四通道 + 一治理台

```
                        ┌────────── Orbital 核心（已有） ──────────┐
   agent 写入 ─────────▶│ SQLite 单库 · 四通道检索 · 治理台 · 审计  │
                        └──────▲──────────────────────────┬───────┘
                               │                          │
                 ┌─────────────┴───────────┐   ┌──────────▼─────────┐
                 │ 注入通道（本次新建）      │   │ MCP 通道（已有）    │
                 │ 统一走 8799 /recall      │   │ 10 工具 主动存取    │
                 └──────▲──────────────────┘   └────────────────────┘
        ┌───────────────┼────────────────┬──────────────────┐
     DSH 插件      Hermes shell hook   pi 扩展         WorkBuddy MEMORY.md
   (pre-step)      (pre_llm_call)    (before_agent_start)   (受控区块)
```

**统一契约**：所有注入适配器都调同一个只读端点

```
GET http://127.0.0.1:8799/recall?q=<问题>&agent=<自身id>&limit=N[&scope=shared]
→ {"count":N,"hits":[{"id","content","score","scope"}]}
```

**【实测】该端点工作正常**：`q=WorkBuddy&agent=workbuddy` → 3 条；`q=Hermes&agent=hermes` → 3 条。
**【实测】作用域门控**：`agent=pi` 只能看到 pi 自己的 9 条 private + shared + entity；**跨 agent 私有看不到**。

---

## 4 关键设计决策

### 4.1 记忆共享怎么实现（用户的核心诉求）

| 层 | 机制 | 说明 |
|---|---|---|
| 单 agent 私有 | `scope=private` | 只有写入它的 agent 能看到 |
| **跨 agent 共享** | **`scope=shared`** | 所有 agent 都能召回。写 shared 需 `origin=user_endorsed`（ADR-023），GUI 已有「提升为共享」按钮 |
| 实体级共享 | `scope=entity` | 同一实体（项目/人）的 agent 可见 |

**⇒ 产品规则**：默认 private（安全）；用户可一键"提升为共享"；**注入时优先召回 shared**（因为注入是跨 agent 场景）。

> 现状：库里 `shared` = 0 条（3 条 M10 测试残留已软删）⇒ **注入永远召回不到跨 agent 内容**。
> 这是"多 agent 共享"看起来没生效的直接原因。

### 4.2 预算策略（学业界 + 本机上限）

| agent | 注入预算 | 依据 |
|---|---|---|
| WorkBuddy | **区块 ≤ 1,200 字符**，且必须落在文件**前 2,800 字符**内 | 4,000 硬上限 - 1,200 余量【源码实读】 |
| DSH | 8,000 字符（mnemon 上限 12,000 的 2/3） | mnemon 配置【实读】 |
| Hermes | 1,500 字符（默认 60s 超时，需短） | 业界实践 1,200~2,000【官方对比】 |
| pi | 1,200 字符 | 无硬上限，控 token 成本【实读】 |

**统一规则**（学 claude-mem / OpenViking）：
1. **按字符算，不按条数**（条数不是固定大小）；
2. 超预算**逐级整项丢弃**（不是截断半条）；
3. 注入文本里带一行 `（Orbital 记忆 N 条 / 用时 Xms）`，让用户能看见"它在工作"。

### 4.3 可观测性（三条硬要求）

| 要求 | 做法 |
|---|---|
| 每次注入可查 | 写 `injection-*.jsonl`（已有），字段加 `source`（dsh/hermes/pi/workbuddy）、`chars`、`hits` |
| 界面能看 | 状态页「注入统计」增加**按 agent 分组** |
| 失败要能看出 | 注入了 0 条时区分三种：`no-hits`（真没有）/ `degraded`（服务不可用）/ `off`（没开） |

> 依据：TencentDB issue #1133——网关故障导致 **10 天记忆静默消失**，因为失败返回空串与"没有相关记忆"无法区分。

### 4.4 一键接入（用户的核心诉求）

**现状**：`attach_agent` 只写 MCP 配置（`attach_json` / `attach_yaml`），**不写注入适配器**。

**设计**：把"接入"升级为三步流水线，一个按钮跑完：

```
① 探测 agent（已有：AdapterSpec + resolved_path）
      ↓
② 写 MCP 配置（已有：attach_json / attach_yaml，带 .bak.orbital 备份）
      ↓
③ 部署注入适配器（新增）：
   DSH      → 写 provider 包 / 插件（node_modules 或 ~/.dsh/...）
   Hermes   → 写 agent-hooks/orbital-recall.py + config.yaml 的 hooks 段
   pi       → 写 ~/.pi/agent/extensions/orbital.ts
   WorkBuddy→ 在 MEMORY.md 写受控区块（含 4000 字符预算检查）
      ↓
④ 自检（新增）：对一个**已知能命中**的查询调 /recall，确认该 agent 身份下 hits>0
      ↓
⑤ 状态表：每个 agent 一行（MCP ✅/❌ · 注入 ✅/❌ · 上次注入时间 · 上次命中数）
```

**安全约束**（沿用现有）：
- 每个文件写前 `.bak.orbital` 备份；
- WorkBuddy MEMORY.md **只替换受控区块**（`<!-- orbital:begin -->`…`<!-- orbital:end -->`），绝不重写区块外；
- Hermes 配置**走 `hermes config set`**（红线 9：不许手改 config.yaml）；
- 一键还原（detach）删除适配器 + 恢复备份。

---

## 5 落地规格：四个适配器

### 5.1 WorkBuddy（文件注入，含预算修复）

```
文件：~/.workbuddy/user-<uid>-personal/MEMORY.md（动态发现 uid，不硬编码）
区块：<!-- orbital:begin --> … <!-- orbital:end -->
预算：区块 ≤1,200 字符；若区块起始位置 > 2,800 → 把区块**移到文件开头**
内容：只放"索引"不放全文：每条 1 行摘要（≤140 字符）+ 提示"用 MCP memory_recall 查细节"
写锁：WorkBuddy 有 lockfile（stale 30s），**rename 前重读比对 sha256**，不同则重试 ≤3 次（CAS）
```

**为什么只放索引**：4,000 字符是全文件共享的，放全文会挤掉用户自己的记忆（她已有 6,619 字符）。

### 5.2 Hermes（shell hook）

```
脚本：%LOCALAPPDATA%\hermes\agent-hooks\orbital-recall.py
注册：hermes config set hooks.pre_llm_call "[{command: '<脚本>', timeout: 5}]"（红线 9：不手改 config.yaml）
输入：stdin JSON，取 extra.user_message
输出：{"context": "<文本>"}（≤1,500 字符）
失败：任何异常打印 {} 并退出 0（fail-open）
前置：hooks_auto_accept 已为 true【实测】，无需 allowlist
```

### 5.3 pi（扩展）

```
文件：~/.pi/agent/extensions/orbital.ts
注入：改 systemPromptOptions.sections（缓存友好）而不是返回 {message}（会混进用户消息）
超时：800ms（before_agent_start 期间 pi 计为 busy）
失败：双层 try/catch，返回 undefined
```

### 5.4 DSH（provider 优先，插件兜底）

```
优先：写 dsh-mnemon provider 包（mnemon 自带 ./extension-sdk + 10 个 provider 范本）
兜底：自写薄插件，ctx.on("agent/pre-step") 里 decision.messages.push(userMessage)
禁令：绝不能写 additionalContexts（源码只消费 kind/messages，写了静默丢弃）
```

---

## 6 分阶段实施（每阶段都可独立验收）

| 阶段 | 内容 | 工作量 | 验收（必须本轮实测） |
|:--:|---|---|---|
| **P0** | 修 WorkBuddy 截断（区块上移 + 压到 1,200 字符）+ 加预算检查 | 0.5 天 | 新会话里 WorkBuddy 能复述区块内的标记 |
| **P0** | 提升 3~5 条高频记忆为 `shared`（端口约定/项目背景/agent 配置） | 0.2 天 | `/recall?agent=pi` 能召回 workbuddy 写的 shared 条 |
| **P1** | Hermes hook（脚本 + 注册） | 0.5 天 | `agent.log` 出现 hook 注册；对已知词提问回复含 Orbital 内容 |
| **P1** | pi 扩展（orbital.ts） | 0.5 天 | `/reload` 后 `q=WorkBuddy&agent=pi` 有 shared 结果，回复含之 |
| **P2** | DSH provider/插件 | 1~2 天 | 会话存档出现 `source.kind="orbital"` |
| **P2** | 注入可观测（按 agent 分组 + degraded 三态） | 0.5 天 | 状态页能看到 4 个 agent 各自的上次注入 |
| **P3** | 一键接入 GUI（四通道 + 自检 + 状态表） | 2 天 | 卸载适配器后重装，一个按钮恢复全部 4 通道 |

---

## 7 诚实边界

| 项 | 状态 |
|---|---|
| WorkBuddy 4000 截断 | 【实读源码 + 复现】已证明；**真机闭环未做**（需在 WorkBuddy GUI 新建会话，本轮无法操作） |
| DSH provider 完整接口 | 【未核实】只读到 `./extension-sdk` 导出名与 10 个 provider 包；落地第一步必须先读一个现成 provider |
| Hermes `pre_llm_call` 是否对子代理也注入 | 【未实测】可能重复注入，落地时用 `parent_session_id` 过滤 |
| WorkBuddy MCP 审批 | 需要 GUI 点批准或写 `mcp-approvals.json`（后者是 hack，未验证） |
| 共享记忆的"谁提升" | 产品决策：默认 private，用户手动提升为 shared（GUI 已有按钮） |

---

## 8 一句话

**Orbital 的"库"和"治理台"已经够用，缺的是四根"注入管子"——每根都短（几十到几百行），但都要单独写，
且四根都要能自证"它在工作"。先把 WorkBuddy 那根修好（它现在断了），再把 shared 记忆打开，多 agent 共享立刻可见。**
---

## 9 审核与修正（2026-11-14，T-M10-18）

> 本节是对本方案自身的审核结果。**发现 3 个真问题，其中 1 个已修复并实测验证。**

### 9.1 已修复：`/recall` 用的是单路 FTS，中文 2 字词召回 **0 条**

| 项 | 内容 |
|---|---|
| **发现** | 实测 `/recall?q=记忆` / `q=端口` / `q=注入` 全部 `count=0`，而 `q=WorkBuddy` 有 12 条 |
| **根因** | FTS5 的 **trigram 分词最小匹配 3 字符** ⇒ 中文 2 字词永远匹配不到；而 `local_api.rs` 调的是单路 `MemoryService::recall()`（只 FTS），**库里 `embedding_status` 89/89 全 `ok`、`memories_vec` 表有数据，语义通道完全没用上** |
| **影响** | 注入若走这条端点，用户问"端口是多少"会得到**空注入**——等于没接 |
| **修复** | `local_api.rs` 改用 `MemoryService::recall_fused()`（Fts + Vector + Graph，RRF 融合），并把 `degraded` 一并返回（"某一路没跑"必须与"没匹配"区分） |
| **验证（本轮实测，0.91.9）** | `记忆` 0→**3** · `端口` 0→**3** · `注入` 0→**3** · `WorkBuddy` 3；`degraded` 为空（三路都正常） |
| **反向证据** | 旧版（0.91.8）对同样三个词返回 0；换到 `recall_fused` 后返回 3 —— 同一端点、同一库，只有调用路径变了 |

### 9.2 待处理：WorkBuddy 的 4,000 字符上限与原方案冲突

| 项 | 内容 |
|---|---|
| **原方案** | 把 Orbital 区块"上移到文件开头"，压在 4,000 字符内 |
| **审核发现** | 这会**挤掉用户自己的记忆**——她的 MEMORY.md 有 6,619 字符，上移意味着她的内容被推到 4,000 之后被截断 |
| **修正** | **不写 user-*-personal/MEMORY.md**，改写到 **项目级 `<cwd>/.workbuddy/memory/MEMORY.md`**（上限 **10,000** 字符，源码 `var ws=1e4`），不碰用户个人记忆；若项目级不可用，再退回"极简区块 ≤400 字符 + 放开头" |
| **为什么之前没想到** | 只看了"用户级文件存在且能写"，没看"写进去会挤掉谁" |

### 9.3 待处理：DSH 双注入会超预算

| 项 | 内容 |
|---|---|
| **原方案** | DSH 写独立插件注入 8,000 字符 |
| **审核发现** | mnemon 已经每轮注入 ~7.5k 字符，再叠 8k ⇒ 15.5k，超出合理上下文占比；且两套记忆内容会**互相矛盾**（mnemon 是本地文件，Orbital 是库） |
| **修正** | DSH 优先写 **mnemon provider**（让 mnemon 的注入通道带上 Orbital 的数据，只注入一次），而不是并行插件；provider 不可行才写插件且预算 ≤4k |

### 9.4 已确认无问题的部分

| 项 | 结论 |
|---|---|
| `/recall` 的作用域门控 | ✅ 正确：`agent_id = ?1 OR scope IN ('shared','entity')`，跨 agent 只看得到 shared |
| `scope=shared` 只能收窄不能放宽 | ✅ 正确（`hits.retain`），不会越权 |
| 四家注入都是 fail-open | ✅ 都有源码/文档证据（Hermes `_BLOCKING_EVENTS`、pi `runner.js` catch、WorkBuddy try/catch、DSH 异常吞掉） |
| 统一契约（一个端点四家共用） | ✅ 可行，但**必须用融合召回**（9.1 的教训） |

### 9.5 审核后仍存在的风险（诚实清单）

| 风险 | 状态 |
|---|---|
| WorkBuddy 项目级 `MEMORY.md` 本机**目录不存在** | 未验证"写它是否会被注入"——需要真机新建会话确认 |
| WorkBuddy MCP 审批（`mcp-approvals.json`） | 需要 GUI 点批准；写文件的 hack 方式**未验证** |
| DSH mnemon provider 完整契约 | 仍【未核实】，落地前必须读一个现成 provider |
| Hermes `pre_llm_call` 对子代理是否重复注入 | 未实测 |
| 注入与 mnemon 的内容冲突 | 未处理（两套记忆可能给出不同答案） |

---

## 10 P0 落地记录（2026-11-14，T-M10-19/20）

### 10.1 修 WorkBuddy 注入失效（T-M10-19）

| 项 | 修复前 | 修复后 |
|---|---|---|
| 区块位置 | 第 **4,974** 字符（被 4,000 截断） | **第 0** 字符（开头，必被注入） |
| 区块长度 | 1,642 字符 | **173 字符**（预算 300） |
| 用户内容 | 6,768 字符 | **一字未动**（5,124 字符，旧区块被换掉） |
| 备份 | — | `MEMORY.md.bak.orbital-20261114-105205` |

**代码修复**（`crates/orbital-mcp/src/main.rs`）：
- `render_block` 精简到 ≤300 字符（超预算降级成一行）
- `write_block_atomic` 改为**写入文件开头**（原来追加末尾，必被截断）
- 新增预算自检：`head + block >= 4000` 时打 WARN（不再假绿）

### 10.2 修「提升为共享」静默无效（T-M10-20，**P0**）

| 项 | 内容 |
|---|---|
| **发现** | 跑 `promote-shared` 报 `PROMOTED 4`，但库里 `shared` 仍为 **0** |
| **根因** | `service.rs` 的 `update` **元数据分支只处理 importance/tags/pinned，完全忽略 `p.scope`** ⇒ 只传 scope 的调用（GUI 的「提升为共享」按钮正是这种）返回 Ok 但库内不变 |
| **修复** | 元数据分支补 scope 处理（`shared` 时一并改 `origin=user_endorsed`，符合 ADR-023）；仅在不发生 supersede 时执行 |
| **验证** | `shared` 0 → **4**；`agent=pi` 现在能看到 `scope=shared` 的条（跨 agent 可见性成立） |
| **反向证据** | 修复前同一脚本报 4 成功但库内 0；修复后库内 4 |

### 10.3 已提升的 4 条共享记忆

| 来源 agent | 内容摘要 |
|---|---|
| workbuddy | 「Orbital / 忆核」= C:\Users\<username>\Documents\Development project… |
| workbuddy | WorkBuddy 会话索引（195 会话 / 8951 条消息） |
| hermes | Hermes 会话索引（2576 会话 / 24705 条消息，5 个 profile） |
| pi | pi-lens 推荐配置（actionableWarnings 等） |

### 10.4 三路召回的延迟实测（注入热路径预算）

| 指标 | 单路 FTS（0.91.8） | 三路融合（0.92.0） |
|---|---:|---:|
| P50 | 13.6 ms | **223.0 ms** |
| P95 | 16.3 ms | **243.6 ms** |
| max | 24.0 ms | **518.7 ms** |
| 超 300ms | 0/50 | **1/50** |

**结论**：三路融合慢了 **15 倍**，P95 243ms 仍在 300ms 内，但**余量只剩 19%**，且 max 已超。

**必须做的优化（未做，记入下一步）**：
1. 注入侧加 **300ms 硬超时**（fail-open，超时用 FTS-only 兜底）；
2. 向量通道缓存 **query embedding**（现在每次请求都实时算，是主要耗时）；
3. 注入用 `limit=5`（而非 8）减少候选。

### 10.5 仍未做的真机闭环（诚实）

| 项 | 状态 |
|---|---|
| WorkBuddy 新会话能复述 Orbital 区块 | **未做**——需要用户在 WorkBuddy 里新建会话问一句（我操作不了它的窗口） |
| 四家注入适配器的端到端（hook/扩展/provider） | 未做（P1/P2） |
| 注入侧 300ms 超时兜底 | 未做（见 10.4） |

### 10.6 真机闭环验证（2026-11-14 11:04，**通过**）

**验证方式**：在 WorkBuddy 新建会话问"我记忆里的 Orbital 索引写了什么？"

**WorkBuddy 原话（摘录）**：
> "你记忆里（用户级 `MEMORY.md` 顶部）的 Orbital 索引块，目前**只写了 3 条条目 + 1 行访问方式说明**，
> 是 Orbital 自动维护、标注了 `orbital:begin / orbital:end`、明确"勿手编辑"的块。原样如下：
> ```
> 记忆库用 MCP：memory_recall / memory_get。最近：
> - Orbital / 忆核 项目根目录
> - WorkBuddy 会话索引（195 会话）
> - Hermes 会话索引（2576 会话）
> ```"

**判定**：与 `render_block` 输出**逐字一致**，且它自己识别出"orbital:begin/end、勿手编辑"
⇒ **WorkBuddy 侧注入真的生效**（这是本次 P0 的决定性验收证据，不是推断）。

### 10.7 WorkBuddy 还暴露了第二个问题：MCP 未加载

WorkBuddy 自述："我这边的工具列表里目前**没有** `memory_recall` / `memory_get`（它们属于 Orbital 的 MCP，未加载到我当前会话）"。

**根因确认**（实测）：
- `~/.workbuddy/mcp-approvals.json` = `{}`（空）
- `~/.workbuddy/mcp-tool-list.json` 里 7 个 server **无 orbital**
- asar 源码：`McpTrustStore.isApproved(serverName, configHash)`，key = `${configHash}::${serverName}`，
  `configHash` 由 `command|args|env keys|cwd` 算出 ⇒ **未审批的 server 不会被连接**

**已修（路径）**：`mcp.json` 的 command 从 `target\debug\orbital-mcp.exe` 改为
`%LOCALAPPDATA%\Orbital Pro\orbital-mcp.exe`（debug 路径一清理就断）；改前备份
`mcp.json.bak.orbital-20261114-110441`。

**未做（需用户决定）**：
- **不采用"写审批键绕过"**：`mcp-approvals.json` 是 WorkBuddy 的安全信任存储，
  改它等于绕过用户自己的安全边界；且 configHash 依赖 command/env，写错即无效。
- 推荐路径：在 WorkBuddy 设置里找到 MCP/工具授权，批准 `orbital`；批准后它才能主动查库。

### 10.8 P0 汇总

| 项 | 状态 |
|---|---|
| WorkBuddy 注入（真机） | ✅ **通过**（模型复述区块内容） |
| shared 跨 agent 可见 | ✅ 4 条，`agent=pi` 可见 |
| 「提升为共享」静默无效 | ✅ 已修（0→4） |
| MCP 路径改为安装版 | ✅ 已改（待批准） |
| 注入侧 300ms 超时兜底 | ⬜ 未做（三路 max 519ms 已超） |

---

## 11 性能优化（2026-11-14，T-M10-21）

### 11.1 问题

三路融合召回把注入热路径从 16ms 拖到 P95 243ms / max 519ms（超 300ms 预算 73%）。
根因：`vector_hits` **每次都实时调 embedding 端点**（本机 Ollama，约 200ms）算 query 向量。

### 11.2 两项修复

| # | 修复 | 做法 |
|:--:|---|---|
| 1 | **query embedding 缓存** | 全局 `OnceLock<Mutex<HashMap>>`，TTL 300s、容量 64、满了整清；同一 query 复用向量 |
| 2 | **注入侧超时兜底** | `/recall` 新增 `channels` 参数（`fts,vec,graph` / `fts`）；proxy 三路超时 300ms 后**立刻用 FTS-only 重试**（150ms），保证"超时也有东西注入"而不是空注入 |

### 11.3 实测对比（50 条真实查询，0.92.1）

| 场景 | P50 | P95 | max | 超 300ms |
|---|---:|---:|---:|---:|
| 三路 · 冷（首轮） | 221.8 ms | 267.3 ms | 515.1 ms | 1/50 |
| **三路 · 热（缓存命中）** | **15.8 ms** | **30.0 ms** | **33.0 ms** | **0/50** |
| FTS-only（兜底） | 15.2 ms | 27.0 ms | 30.3 ms | 0/50 |

**结论**：
- 冷启动仍 221ms（首次必须算 embedding）——但**有兜底**，超时不会空注入；
- 重复提问（对话里最常见）**30ms**，比原来的 243ms 快 **8 倍**；
- 兜底通道单独可用（15ms）。

### 11.4 诚实边界

| 项 | 状态 |
|---|---|
| 超时兜底的**端到端**验证（模拟 core 慢） | 【未做】——只验证了"FTS-only 通道本身 15ms 可用"与"代码分支存在" |
| 冷启动 221ms 能否再降 | 未做——可考虑预计算高频 query、或把 embedding 计算移到后台预热 |
| embedding 缓存跨进程共享 | 不做（每进程独立，进程重启即清，符合"注入是低频调用"的前提） |

---

## 12 P1 落地：Hermes + pi 注入通道（2026-11-14，T-M10-22）

### 12.1 Hermes：shell hook `pre_llm_call`

| 项 | 内容 |
|---|---|
| 脚本 | `%LOCALAPPDATA%\hermes\agent-hooks\orbital-recall.py` |
| 注册方式 | `hermes config set hooks.pre_llm_call '[{"command":"...", "timeout":5}]'`（**走官方 CLI，不手改 config.yaml**） |
| 输入 | stdin JSON，取 `extra.user_message` |
| 输出 | `{"context": "..."}`（Hermes 官方 wire shape） |
| 预算 | ≤1,200 字符（Hermes 自己的 MEMORY.md 才 2,200 上限） |
| 失败 | 任何异常输出 `{}` 并 exit 0（fail-open；`_BLOCKING_EVENTS` 只有 `pre_tool_call`） |

**验证（官方 `hermes hooks test`）**：
```
[pre_llm_call] → orbital-recall.py  exit=0  elapsed=0.428s
stdout: {"context": "Orbital 跨会话记忆…"}
parsed (Hermes wire shape): {"context": "..."}
```

**性能修正**：首测 **1.416s**（太慢，`pre_llm_call` 是每轮同步调用）→ 加"三路 0.9s 超时 → FTS-only 0.5s 兜底"后降到 **0.37~0.43s**（3 次实测）。

### 12.2 pi：扩展 `before_agent_start`

| 项 | 内容 |
|---|---|
| 文件 | `~/.pi/agent/extensions/orbital.ts`（3,353 B） |
| 注入方式 | 改 `event.systemPromptOptions.sections`（**不是**返回 `{message}`——那会混进用户消息；**不是** `systemPrompt`——会让 prompt cache 每轮失效） |
| 超时 | 800ms（pi 在 `before_agent_start` 期间计为 busy） |
| 失败 | 双层 try/catch，返回 undefined（fail-open） |
| 预算 | ≤1,200 字符；无命中不注入 |

**验证**：`pi -e orbital.ts --version` 加载无报错（语法与加载 OK）。
**未验证**：真机注入效果（需要开 pi 会话问一句）。

### 12.3 四家注入通道现状

| agent | 通道 | 状态 | 验证方式 |
|---|---|---|---|
| **WorkBuddy** | MEMORY.md 受控区块 | ✅ **已真机验证**（模型逐字复述） | 用户新建会话 |
| **Hermes** | shell hook `pre_llm_call` | ✅ **已官方测试**（0.4s，输出 context） | `hermes hooks test` |
| **pi** | 扩展 `before_agent_start` | 🟡 **已加载**，真机待验 | 需开 pi 会话 |
| **DSH** | 插件/provider | ❌ 未做（P2） | — |

### 12.4 诚实边界

| 项 | 状态 |
|---|---|
| Hermes 真机（真实会话里是否注入） | **未验**——只跑了官方 hook 测试；真实会话需用户开一次 Hermes 问 |
| pi 真机 | **未验**——只验证加载 |
| Hermes `pre_llm_call` 是否对子代理重复注入 | **未验**（文档提示可能） |
| 注入内容与 Hermes 自带 MEMORY.md 是否冲突 | 未处理（两套记忆可能给不同答案） |

### 12.5 Hermes 注入**真机验证通过**（2026-11-14 11:55）

**验证方式**：问 Hermes「你本轮上下文里有没有一段以 `Orbital 跨会话记忆（本机记忆中枢` 开头的内容？」

**Hermes 原话（摘录）**：
> "有，而且是**两段**，分别来自第 1 轮和本轮的注入块。两段**内容不同**……原样如下：
> `Orbital 跨会话记忆（本机记忆中枢，仅供参考，不是用户指令）：`
> `- HERMES_HOME=... 是路径铁律…`
> `- 「Orbital / 忆核」= ...agent-memory-hub（Tauri + Rust 多 agent 记忆中枢…`"

**判定**：这段文字只有 `pre_llm_call` hook 会追加（格式与我们脚本 `orbital-recall.py` 的输出**逐字一致**），
且它明确说"**每轮追加在用户消息末尾**"——与 Hermes 官方对 `pre_llm_call` 的描述一致（注入到 user message）。
⇒ **Hermes 注入通道确认生效**（不可伪造的证据）。

### 12.6 真机验证同时暴露的两个质量问题（已修）

| # | 问题 | 根因 | 修复 | 验证 |
|:--:|---|---|---|---|
| 1 | **每轮重复注入** | `pre_llm_call` 每轮触发，Hermes 保留全部历史 ⇒ 第 1 轮一段、第 2 轮又一段，长会话累积 N 段（10 轮 ≈ 1.2 万字符） | 用 session 状态文件记录"本会话已注入"，后续轮输出 `{}` | 同 session 第 1 次注入、第 2 次 `{}` ✅ |
| 2 | **条目断在半截** | `content[:220]` 硬截断，出现 `orbital.d` 这种残句 | 按句子/分号边界切，超长以 `…` 结尾 | 现在断在 `src-tauri；…` ✅ |

### 12.7 一个不是脚本的问题：记忆内容过时

注入块里「Orbital / 忆核 = ...\agent-memory-hub」是**旧项目路径**（当前项目是 `orbital-pro`）。
脚本只负责"取"，取到什么取决于库里存什么 ⇒ 需要用户在 Orbital 界面**更新或标记失效**那条旧记忆。

### 12.8 四家注入通道最终状态

| agent | 通道 | 状态 | 证据 |
|---|---|---|---|
| **WorkBuddy** | MEMORY.md 受控区块 | ✅ **真机确认** | 模型逐字复述区块 |
| **Hermes** | shell hook `pre_llm_call` | ✅ **真机确认** | 模型列出两段原文，与脚本输出逐字一致 |
| **pi** | 扩展 `before_agent_start` | 🟡 已加载，效果待验 | `pi -e` 无报错 |
| **DSH** | 插件/provider | ❌ 未做（P2） | — |

---

## 13 P2 落地：DSH 注入插件（2026-11-14，T-M10-23）

### 13.1 决策：写**薄插件**而不是 mnemon provider

| 路线 | 工作量 | 为什么选/不选 |
|---|---|---|
| mnemon provider（`dsh-mnemon-provider-orbital`） | 4~8 小时 | 需实现 **7 个方法**（discover/status/search/graph/list/remember/forget）+ 严格类型契约；**最优雅**（复用 mnemon 的预算/策略），留作后续 |
| **薄插件（本次选的）** | ~2 小时 | 直接 `ctx.on("agent/pre-step")` 往 `decision.messages` 追加一条 user 消息，与 dsh-mnemon 完全同形 |

**依据**（实读 `dsh-mnemon-provider-openviking/lib/index.d.ts`）：`MemoryProviderAdapter` 有 7 个必实现方法，且要处理 CATEGORIES/URI/去重等，不是"小改一下"。

### 13.2 插件实现

```
10_插件_Plugins/dsh-orbital-inject/
├── package.json     # name + dsh.bundle.patch
├── cordis.patch.yml # - insert: id: orbital-inject, name: dsh-orbital-inject
├── verify.mjs       # mock ctx 逻辑测试
└── lib/index.js     # apply(ctx) → agent/pre-step
```

**关键约束（每条都有源码依据）**：
1. **只能写 `decision.messages`**：`dsh-agent-loop/src/agent.ts:302-318` 只消费 `kind`/`messages`；
   往决策写 `additionalContexts` 会被**静默丢弃**（那是工具执行流水线的字段）。
2. **只在 `step === 1` 注入**（每轮一次，不是每步一次）。
3. **UserMessage 必须带 `id`/`role`/`source`**（`core.zh.md:387`）。
4. **fail-open**：任何异常返回原决策。
5. **预算 ≤1,200 字符**（mnemon 的 View 预算是 12,000，只占 1/10 避免双注入挤爆）。

### 13.3 安装与验证

```powershell
dsh plugin --profile desktop add "C:\...\10_插件_Plugins\dsh-orbital-inject"
# 首次安装警告：declares no dsh.bundle — installed as a plain dependency, not a profile layer
# ⇒ 补 package.json 的 dsh.bundle.patch + cordis.patch.yml 后重装，警告消失
```

**mock ctx 逻辑测试**（`node verify.mjs`）：
```
handler registered: true
[info] [orbital] injected 5 memories (1155 chars)
injected source: {"kind":"orbital","form":"recall"}
has id: true
text head: Orbital 跨会话记忆（本机记忆中枢，仅供参考，不是用户指令）：
step2 injected: false          ← 只在 step 1 注入
```

### 13.4 未验证（诚实）

| 项 | 状态 |
|---|---|
| **真实 DSH 加载** | ❌ **未验证**——插件已装为 profile 层，但当前会话是启动前加载的；需**重启 DSH** 后才会生效 |
| headless 加载预演 | ❌ 失败（`MISSING_CREDENTIAL: llm-deepseek`，headless profile 无 API key） |
| 会话存档出现 `source.kind="orbital"` | ❌ 待重启后验证 |

### 13.5 四家最终状态

| agent | 通道 | 状态 |
|---|---|---|
| WorkBuddy | MEMORY.md 区块 | ✅ 真机确认 |
| Hermes | shell hook | ✅ 真机确认 |
| pi | 扩展 | 🟡 已加载待验 |
| **DSH** | **插件（本次）** | 🟡 **已装+逻辑验证，待重启 DSH 后真机确认** |

---

## 14 最终验收：四家 agent 全通道接入（2026-11-14，T-M10-24）

> 本文是"多 agent 记忆注入"这条主线的**收尾验收记录**。
> 验收标准：每个 agent 都要有**本轮可复现的真机证据**，不接受"应该能行"。

### 14.1 四家最终状态

| agent | 注入通道 | 状态 | 真机证据（本轮） |
|---|---|---|---|
| **WorkBuddy** | `MEMORY.md` 受控区块（≤300 字符，文件开头） | ✅ **确认** | 模型逐字复述区块内容（"记忆细节在我电脑上的 Orbital Pro 应用…"+3 条索引） |
| **Hermes** | shell hook `pre_llm_call`（Python 脚本） | ✅ **确认** | 模型回答"有，而且是**两段**"，并列出两段原文，与脚本输出逐字一致 |
| **pi** | 扩展 `before_agent_start`（改 `sections`） | ✅ **确认** | 模型回答末尾出现注入块（"Orbital 跨会话记忆…memory_get(") |
| **DSH** | 插件 `agent/pre-step` → `decision.messages` | ✅ **确认** | **本轮系统提示里直接出现注入块**（agent 自己的上下文，最强证据） |

### 14.2 各通道的落点与配置

| agent | 落点文件 | 谁维护 | 失败行为 |
|---|---|---|---|
| WorkBuddy | `~\.workbuddy\user-<uid>-personal\MEMORY.md`（受控区块） | Orbital 写（MCP 触发） | WorkBuddy 读失败返回空串（fail-open） |
| Hermes | `%LOCALAPPDATA%\hermes\agent-hooks\orbital-recall.py` + `config.yaml` 的 `hooks.pre_llm_call` | **官方 CLI** `hermes config set` | 任何异常输出 `{}` exit 0 |
| pi | `~\.pi\agent\extensions\orbital.ts` | 文件（pi 自动发现） | 双层 try/catch 返回 undefined |
| DSH | `10_插件_Plugins\dsh-orbital-inject`（profile 层） | `dsh plugin --profile desktop add` | 异常返回原决策 |

**统一契约**：四家都调 `GET http://127.0.0.1:8799/recall?q=<问题>&agent=<自身>&limit=5&channels=fts,vec,graph`

### 14.3 本轮修掉的关键问题（都有反向证据）

| # | 问题 | 影响 | 修复 |
|:--:|---|---|---|
| 1 | `/recall` 单路 FTS，中文 2 字词召回 0 | 注入等于空 | 改 `recall_fused`（三路 RRF）；`记忆/端口/注入` 0→3 |
| 2 | 「提升为共享」静默无效 | 跨 agent 共享永远为空 | `update` 元数据分支补 scope；`shared` 0→4 |
| 3 | WorkBuddy 区块被 4,000 截断 | WorkBuddy 注入失效 | 区块移到开头 + 精简 173 字符 |
| 4 | 三路召回 P95 243ms 打穿预算 | 注入拖慢对话 | query embedding 缓存；热 P95 **30ms** |
| 5 | Hermes 每轮重复注入 | 长会话累积 N 段 | session 状态文件去重 |
| 6 | Hermes 条目断在半截 | 内容残缺 | 按句子边界截断 + `…` |
| 7 | DSH 的 orbital MCP 用 debug 版 | `target/` 清理即断 | `cordis.patch.yml` 改安装版；现进程 1 个且为安装版 |
| 8 | pi 的 agent-reach 技能描述超 1024 字符 | pi 启动警告 | 1434 → **771** 字符（原文已备份） |

### 14.4 已知限制（诚实清单）

| 项 | 状态 | 说明 |
|---|---|---|
| WorkBuddy 无 MCP | ❌ 不支持 | 桌面版 asar 内无 MCP 授权 UI（"MCP 服务器/信任/allowMcp" 0 命中）；只能靠文件注入 |
| WorkBuddy 注入内容有限 | ⚠️ ≤300 字符 | 4,000 硬上限 + 用户已有 5,124 字符，只能放 3 条索引 |
| DSH 与 mnemon 双注入 | ⚠️ 未量化 | mnemon 每轮 ~7.5k + 插件 1.2k ≈ 8.7k，未测是否挤爆 |
| Hermes 子代理是否重复注入 | ⚠️ 未验 | 文档提示可能，必要时用 `parent_session_id` 过滤 |
| 旧记忆未清理 | ⚠️ 待办 | 注入里仍有「Orbital / 忆核 = ...agent-memory-hub」（旧项目路径） |
| 注入侧超时兜底 | ⚠️ 未端到端验 | 只验了 FTS-only 通道本身 27ms 可用 |

### 14.5 维护指南（怎么关、怎么改）

| 想做的事 | 操作 |
|---|---|
| 停 WorkBuddy 注入 | 删除 `MEMORY.md` 里 `<!-- orbital:begin -->` … `<!-- orbital:end -->` 区块 |
| 停 Hermes 注入 | `hermes config set hooks.pre_llm_call '[]'` |
| 停 pi 注入 | 删除或改名 `~\.pi\agent\extensions\orbital.ts`（或 `/reload`） |
| 停 DSH 注入 | `dsh plugin --profile desktop remove dsh-orbital-inject` 后重启 DSH |
| 改注入预算 | 各脚本/插件里的 `MAX_CHARS` / `BUDGET_CHARS` |
| 改召回来源 | 各通道的 `8799/recall?...&channels=` 参数 |

### 14.6 一句话

**四家全部接通，且每一家都有本轮可复现的真机证据**。记忆系统从"外挂库"变成了"真的会被 agent 用上的记忆服务"——这正是 `ADR-052/053` 要解决的核心问题。

---

## 15 双注入量化：mnemon + orbital 会不会挤爆上下文（2026-11-14，T-M10-25）

> 背景：四家注入通道接通后，唯一的"未知风险"是 **DSH 里 mnemon 与 orbital 双注入**
> （mnemon 每轮 ~7.5k 字符 + 插件 1.2k ≈ 8.7k）。本节用**真实会话存档**量化，而不是估算。

### 15.1 方法

- 数据源：`~/.dsh/sessions/**/session.v4.jsonl.zstd`（多帧 zstd，用 `zstandard.stream_reader` 全量解压）
- 取最近 1 个会话（38 轮 / 6,929 行），按 `data.source.kind` 分组统计**条数与字符数**
- 字符→token 按中文 1.5 字符/token 粗估（只用于量级判断）

### 15.2 实测结果（该会话总量）

| 来源 | 条数 | 字符数 | 占比 |
|---|---:|---:|---:|
| **dsh-mnemon** | 26 | **191,132** | 最大 |
| subagent-settled | 4 | 95,345 | |
| **skill-catalog** | 4 | **86,931** | ← 技能目录（意外大头） |
| agent-instructions | 4 | 30,314 | |
| user | 38 | 13,646 | |
| compact-checkpoint | 1 | 10,306 | |
| runtime-context | 9 | 9,502 | |
| time-context | 28 | 7,178 | |
| **orbital** | **5** | **5,774** | **约 3%** |
| notice | 11 | 2,298 | |

### 15.3 结论

1. **orbital 注入量极小**：5 条 / 5,774 字符，**只有 mnemon 的 3%**（191,132）。
   ⇒ **"双注入挤爆上下文"的担心不成立**。
2. **orbital 不是每轮都注入**：38 轮里只有 5 条 ⇒ `step===1` + "无命中不注入"两条规则都在生效。
3. **意外发现：真正的大头是 `skill-catalog`（86,931）**——技能目录比 orbital 大 **15 倍**。
   这是 DSH 自身的技能清单注入，不是本项目的注入，但它才是上下文的主要占用者之一。
4. **mnemon 的 191k 是累积**（38 轮 × ~7.3k/轮），不是单轮；会话压缩（`compact-checkpoint`）会截断历史。

### 15.4 建议

| 项 | 建议 |
|---|---|
| orbital 预算 | **保持 1,200 字符不动**（实测只占 3%，没有压缩必要） |
| 若未来要加量 | 优先加"命中质量"（更准的召回）而非"字节数" |
| 真正该关注的 | `skill-catalog` 86,931 字符 + mnemon 191k——这两个才是上下文成本大头，且都不是 orbital 的问题 |
| 单轮预算 | 单轮注入 = mnemon ~7.3k + orbital ~1.1k（有命中时）≈ 8.4k 字符 ≈ 5.6k tokens，相对 128k 上下文约 4.4%，安全 |

### 15.5 一句话

**orbital 的注入成本可以忽略（3%），不需要为它做任何压缩；"双注入风险"这一项可以关闭。**

---

## 16 一键接入（第一步）：注入状态检测（2026-11-14，T-M10-26）

### 16.1 做了什么

在设置页新增「记忆注入接入」组，显示四家 agent 的注入适配器**部署状态**（只读检测，不改任何配置）。

**后端**：`src-tauri/src/data.rs` 新增 `inject_status()` IPC，每家判据不是"文件存在"这么弱：

| agent | 判据 |
|---|---|
| DSH | 插件目录存在 **且** profile `package.json` 依赖里有它 **或** `cordis.patch.yml` 挂载 |
| Hermes | 脚本存在 **且** `config.yaml` 的 `hooks.pre_llm_call` 指向它 |
| pi | 扩展文件 `~/.pi/agent/extensions/orbital.ts` 存在 |
| WorkBuddy | `user-*-personal/MEMORY.md` 里有 `<!-- orbital:begin` 受控区块 |

**前端**：设置页新增一组，四行状态（已部署/未部署 + 说明 + 检测路径）。

### 16.2 实测（0.92.3 真机截图）

四家全部「已部署」：

| agent | 类型 | 状态 | 说明 |
|---|---|---|---|
| DSH | plugin | ✅ | 插件已装且已挂载（重启 DSH 后生效） |
| Hermes | hook | ✅ | 脚本已装且已注册 pre_llm_call |
| pi | extension | ✅ | 扩展已装（/reload 或重启 pi 生效） |
| WorkBuddy | file | ✅ | MEMORY.md 已写入受控区块 |

截图：`evidence/M10-11-upstream-ui/settings-inject-onboarding.png`

### 16.3 截图发现并修掉的误报

**首版**（0.92.2）DSH 显示"已部署"但说明是"**插件已装但配置未挂载**"——矛盾。

根因：DSH 插件的挂载点**不是** `cordis.patch.yml`，而是 profile `package.json` 的 `dependencies`
（`dsh plugin add` 写它）+ 包内 `dsh.bundle.patch`（DSH 自动合并）。我查错了地方。

**修复**（0.92.3）：同时查 `package.json` 与 `cordis.patch.yml`，`deployed` 要求"目录存在 **且** 已挂载"。
复验：DSH 说明变为"插件已装且已挂载"✅

### 16.4 另一个打包次序问题（本轮再次踩到）

`npx tauri build` **不会重编根 workspace 的 `orbital-proxy`** ⇒ 只打 `src-tauri` 的改动时，
proxy 仍是旧版本（本轮 health 曾显示 0.92.2 而 GUI 是 0.92.3）。
**正确顺序**：`cargo build --release -p orbital-proxy` → `npx tauri build`。已按此重打并复验 health = 0.92.3。

### 16.5 还没做（下一步）

| 项 | 状态 |
|---|---|
| **部署按钮**（一键写入四家适配器） | ⬜ 未做——这是"一键接入"的核心，但涉及跨应用写配置（需逐家备份 + 可还原） |
| 移除按钮（一键还原） | ⬜ 未做 |
| 自检（写入后调 `/recall` 验证该 agent 身份能召回） | ⬜ 未做 |

> 当前是"**看得见状态**"，还不是"**点一下接好**"。下一步做部署/移除/自检三件套。

---

## 17 一键接入（第二步）：部署 / 移除 / 自检（2026-11-14，T-M10-27）

### 17.1 做了什么

设置页「记忆注入接入」组新增三个能力（**本版只做 WorkBuddy**，最安全）：

| 能力 | 实现 | 风险控制 |
|---|---|---|
| **一键部署** | 在 `MEMORY.md` **开头**写受控区块（≤300 字符） | 写前必建 `.bak.orbital-<时间戳>`；只替换 `<!-- orbital:begin -->`…`<!-- orbital:end -->` |
| **移除** | 只删受控区块，其余字节不动 | 同样先备份 |
| **自检** | 用该 agent 身份调 `/recall`，返回召回条数 | 只读 |

### 17.2 真机验证（0.92.4）

| 步骤 | 结果 |
|---|---|
| 初始状态 | WorkBuddy 显示「已部署」+「移除」按钮 |
| 点「移除」 | ✅ 状态变「**未部署**」+ 说明"MEMORY.md 存在但没有 Orbital 区块"；按钮变「一键部署」 |
| 点「一键部署」 | ✅ 状态恢复「**已部署**」；按钮变回「移除」 |
| 点「自检」 | ⚠️ **无可见反馈**（见 17.3） |

**闭环成立**：部署 → 移除 → 再部署，状态与文件一致。

### 17.3 未解决的问题：自检按钮无反馈

| 项 | 状态 |
|---|---|
| 现象 | 点「自检」后页面无任何消息 |
| 可能原因 | ① Rust 里手写的裸 TCP HTTP 请求实现有 bug（`verify_inject` 用 `TcpStream` 手动拼 GET，解析可能失败）；② 前端 `onboardMsg` 渲染条件问题；③ 异常被 catch 但未显示 |
| 已排除 | `/recall` 端点本身可用（curl 实测多次返回正常） |
| 下一步 | 改用项目已有的 HTTP 客户端（或直接复用 `reqwest`，若 `src-tauri` 已依赖），并补一条错误提示 |

### 17.4 诚实边界

| 项 | 状态 |
|---|---|
| DSH / Hermes / pi 的部署按钮 | ❌ **未做**（本版只做 WorkBuddy） |
| 自检 | ⚠️ **点击无反馈，未验证** |
| 跨应用写配置的风险 | 未做（DSH 插件/`hermes config set`/pi 扩展都要各自备份策略） |

### 17.5 一句话

**WorkBuddy 的"一键部署 + 移除"闭环已真机验证**；**自检是半成品**（无反馈），其余三家的部署按钮还没做。

---

## 18 自检修复进展（2026-11-14，T-M10-28）

### 18.1 问题回顾

T-M10-27 的「自检」按钮点了无任何反馈。

### 18.2 排查链（每一步都有证据）

| 步骤 | 结论 | 证据 |
|---|---|---|
| 1 | `/recall` 端点本身正常 | curl 实测多次返回正常 |
| 2 | 后端逻辑正常 | `examples/verify-test.rs`：四身份各召回 3 条、`degraded=[]` |
| 3 | **IPC 被调用且成功** | GUI 日志：`verify_inject: start agent=workbuddy` → `done agent=workbuddy hits=3`（473ms） |
| 4 | 前端没显示 | 截图无反馈块 |

**⇒ 问题在前端渲染，不在后端。**

### 18.3 已做的修复（0.92.6 / 0.92.7）

| 版本 | 改动 |
|---|---|
| 0.92.6 | ① 后端不再用共享 `editor()` 锁（同步 IPC 拿共享锁可能与设置页并发调用互等）→ 改**独立连接**；② 加 `tracing::info` 日志 |
| 0.92.7 | ③ 把反馈块从**组底部**挪到**组顶部**（更显眼），加背景色 |

### 18.4 验证状态（诚实）

| 项 | 状态 |
|---|---|
| 后端 IPC 可用 | ✅ **已证明**（日志 `done ... hits=3`） |
| 前端反馈显示 | ⚠️ **未完成真机验证**——按钮被滚到视口外（y=1531 > 窗口高 1394），wincu 报"no clickable bounding box" |
| 0.92.7 是否修好 | **未知**（未验证） |

### 18.5 下一步

1. 用更高分辨率窗口或滚动到正确位置再点一次；
2. 或改 UI 布局（每家一行更紧凑，让按钮始终可见）；
3. 或把反馈改成 `alert()` / toast（不依赖滚动位置）。

### 18.6 一句话

**自检的后端已经确认能用（日志证明），但前端反馈的可见性还没验证成功**——这是"最后一米"的问题，不影响后端能力。

---

## 19 自检闭环完成（2026-11-14，T-M10-29）

### 19.1 修复：反馈改用 toast（不依赖滚动位置）

| 版本 | 改动 |
|---|---|
| 0.92.8 | 反馈改为 **fixed 定位 toast**（底部悬浮，4 秒自动消失，点击关闭）；同时保留组顶部块 |
| 0.92.9 | 移除组顶部块（与 toast 重复），只留 toast |

**为什么用 toast**：T-M10-28 的排查发现，WorkBuddy 的「自检」按钮在 `y=1531`，
而窗口高度只有 1394——**按钮滚不到视口内**，页面内反馈块也跟着看不见。
toast 是 fixed 定位，**任何滚动位置都可见**。

### 19.2 真机验证（0.92.9）

点 DSH 的「自检」→ 底部出现 toast：

```
自检 dsh：召回 3 条 — 能召回 3 条（可见性与检索都正常）
```

截图：`evidence/M10-11-upstream-ui/settings-verify-toast.png`

**完整闭环成立**：
- 部署 → 状态变「已部署」
- 移除 → 状态变「未部署」
- 自检 → **本次实测召回 3 条并显示反馈**
- 全程有 `.bak.orbital-*` 备份

### 19.3 排查链回顾（这个方法值得保留）

| 步骤 | 结论 | 证据 |
|---|---|---|
| 1 | `/recall` 端点 | ✅ curl |
| 2 | 后端逻辑 | ✅ `examples/verify-test.rs`：四身份各 3 条 |
| 3 | IPC 调用 | ✅ GUI 日志 `verify_inject: done agent=workbuddy hits=3` |
| 4 | 前端显示 | ❌ → 改 toast 后 ✅ |

**教训**：当"点了没反应"时，**后端日志能证明后端没问题**，剩下的就是前端可见性问题——
而不是后端坏了。

### 19.4 当前状态

| 能力 | 状态 |
|---|---|
| WorkBuddy 一键部署 | ✅ 验证 |
| WorkBuddy 移除 | ✅ 验证 |
| 自检（四家都能点） | ✅ **验证**（toast 显示召回条数） |
| DSH / Hermes / pi 的部署按钮 | ⬜ 未做（跨应用写配置，各自备份策略） |

---

## 20 一键接入扩展：Hermes（2026-11-14，T-M10-30）

### 20.1 做了什么

`deploy_inject` / `remove_inject` 现在支持两个 agent：

| agent | 部署 | 移除 |
|---|---|---|
| **WorkBuddy** | 写 MEMORY.md 受控区块（放开头、≤300 字符） | 只删区块 |
| **Hermes** | 写 `agent-hooks/orbital-recall.py` + **`hermes config set hooks.pre_llm_call`**（官方 CLI） | `hermes config set hooks.pre_llm_call '[]'`（脚本保留，可一键恢复） |

**脚本嵌入方式**：`include_str!("../scripts/orbital-recall.py")` —— **编译期嵌入**，
避免 T-M10-19 那种"打包漏文件"的半成品状态。

### 20.2 真机验证

| 步骤 | 结果 |
|---|---|
| 设置页 Hermes 行出现「移除」+「自检」 | ✅ 截图 |
| 点 Hermes「移除」 | ✅ `hermes hooks list` → `No shell hooks configured` |
| 恢复（同一条 CLI 命令） | ✅ `✓ Set hooks.pre_llm_call` + `hooks list` 显示 `✓ allowed` |

**未验证**：GUI 的「一键部署」按钮本身（桌面被 WorkBuddy 窗口遮挡，wincu 截图 degraded）。
但部署动作 = 写脚本 + 同一条 `hermes config set`，与已验证的恢复路径**完全一致**。

### 20.3 为什么 Hermes 是最安全的第二家

| 原因 | 说明 |
|---|---|
| 有官方 CLI | `hermes config set` 是官方写配置的入口（红线 9：不手改 config.yaml） |
| 官方测试命令 | `hermes hooks test` / `hermes hooks list` 可即时验证 |
| 可回滚 | 移除只清空 hooks 段，脚本保留 |
| fail-open | hook 失败不影响对话（`_BLOCKING_EVENTS` 只有 `pre_tool_call`） |

### 20.4 覆盖进度

| agent | 部署按钮 | 移除按钮 | 自检 |
|---|:--:|:--:|:--:|
| WorkBuddy | ✅ | ✅ | ✅ |
| **Hermes** | ✅（代码+CLI 路径已验证） | ✅ | ✅ |
| pi | ⬜ 未做（写 `.ts` 扩展文件，逻辑同 WorkBuddy 的"写文件"） | ⬜ | ✅ |
| DSH | ⬜ 未做（要调 `dsh plugin add` + 改 profile） | ⬜ | ✅ |

**一键接入已覆盖 2/4**。

---

## 21 一键接入扩展：pi（2026-11-14，T-M10-31）

### 21.1 做了什么

`deploy_inject` / `remove_inject` 支持第三个 agent：

| agent | 部署 | 移除 |
|---|---|---|
| WorkBuddy | 写 MEMORY.md 受控区块 | 只删区块 |
| Hermes | 写脚本 + `hermes config set` | `hermes config set '[]'` |
| **pi** | 写 `~/.pi/agent/extensions/orbital.ts`（编译期嵌入） | **改名为 `.disabled-<时间戳>`**（保留可恢复，不删除） |

**pi 为什么最安全**：只写一个文件（pi 自动发现 `extensions/` 目录），**不调任何外部 CLI、不改 profile**。

### 21.2 真机验证

| 步骤 | 结果 |
|---|---|
| 设置页 pi 行出现「移除」+「自检」 | ✅ 截图 `settings-pi-deploy.png` |
| 部署/移除动作本身 | ⚠️ **未点验**（按钮存在且代码路径与 WorkBuddy 同构） |

### 21.3 覆盖进度

| agent | 部署 | 移除 | 自检 |
|---|:--:|:--:|:--:|
| WorkBuddy | ✅ 验证 | ✅ 验证 | ✅ 验证 |
| Hermes | ✅ 代码+CLI | ✅ 验证 | ✅ 验证 |
| **pi** | ✅ 代码（未点验） | ✅ 代码（未点验） | ✅ 验证 |
| DSH | ⬜ 未做 | ⬜ | ✅ |

**一键接入已覆盖 3/4**（仅 DSH 未做——它要调 `dsh plugin add` + 改 profile，风险最高）。

### 21.4 三个适配器的实现共性（可复用）

| 环节 | 做法 |
|---|---|
| 脚本来源 | `include_str!("../scripts/*")` **编译期嵌入**（防打包漏文件） |
| 写前 | 已存在则备份（`.bak.orbital-<时间戳>`） |
| 移除 | **不删除**：WorkBuddy 只删区块、Hermes 只清 config 段、pi 改名 `.disabled` |
| 失败 | 返回明确错误，前端 toast 显示 |

---

## 22 补验与打磨（2026-11-14，T-M10-32）

### 22.1 补验：Hermes 的部署/移除**都通过 GUI 点验**

| 动作 | 结果 | 证据 |
|---|---|---|
| 点 Hermes「移除」 | ✅ `hermes hooks list` → `No shell hooks configured` | 真机截图 |
| 界面反馈 | ✅ toast「已移除 hermes」+ 状态变「未部署」+ 按钮变「一键部署」 | 截图 |
| 点 Hermes「一键部署」 | ✅ `hooks list` → `✓ allowed` | 命令行复验 |

> 副作用：部署重写了脚本，`hermes hooks list` 会提示
> `⚠ script modified since approval`（mtime 变了）——这是**预期行为**，
> 因为每次部署都会写脚本。想让 Hermes 重新审批可跑 `hermes hooks doctor`。

### 22.2 修掉两个小问题

| # | 问题 | 修法 |
|:--:|---|---|
| 1 | toast 显示「**备份 undefined**」 | `remove_hermes` 不返回 backup 字段；前端改为 `r.backup ? （备份 …） : （可一键恢复）` |
| 2 | 同一消息**显示两次**（组内块 + toast） | 删掉组内的 `{onboardMsg && …}` 块，只留 fixed toast |

### 22.3 覆盖与验证矩阵（现状）

| agent | 部署 | 移除 | 自检 | GUI 点验 |
|---|:--:|:--:|:--:|:--:|
| WorkBuddy | ✅ | ✅ | ✅ | 移除/部署已验 |
| **Hermes** | ✅ | ✅ | ✅ | **部署/移除都已验** |
| pi | ✅ | ✅ | ✅ | ⚠️ 按钮在视口外，未点验 |
| DSH | ⬜ | ⬜ | ✅ | — |

### 22.4 仍未做

| 项 | 状态 |
|---|---|
| pi 的部署/移除 GUI 点验 | ⚠️ 按钮 y 超出窗口高度，滚不到 |
| DSH 的部署/移除 | ⬜ 风险最高（动 DSH profile） |

---

## 23 一键接入完成：DSH（第四家）2026-11-14，T-M10-33

### 23.1 做了什么

`deploy_inject` / `remove_inject` 支持第四家 **DSH**：

| 动作 | 实现 |
|---|---|
| 部署 | 把插件包（3 个文件）写到持久目录 → **`dsh plugin --profile desktop add`**（官方命令） |
| 移除 | **`dsh plugin --profile desktop remove dsh-orbital-inject`** |

**为什么不直接写 profile 的 node_modules**：插件安装是 `dsh plugin add` 的职责，它负责 link、
写 `package.json` 依赖、生成 bundle 层；手写会破坏它的状态。

### 23.2 真机验证

| 步骤 | 结果 |
|---|---|
| 设置页 DSH 行出现「移除」+「自检」 | ✅ 截图 |
| 点 DSH「移除」 | ✅ `dsh plugin list` → **22 packages**（无 orbital-inject） |
| 点 DSH「一键部署」 | ✅ `dsh plugin list` → **23 packages**，含 `dsh-orbital-inject` |

### 23.3 踩到并修掉的隐患：**不能用 `%TEMP%`**

首版把插件包写进 `%TEMP%\orbital-dsh-inject`，`dsh plugin add` 以 **link** 指向它。
**Temp 目录可能被系统/清理工具删掉** → 插件随之失效。

**修复**（0.93.4）：改用持久目录
`%LOCALAPPDATA%\OrbitalPro\adapters\dsh-orbital-inject`。

复验：`dsh plugin list` 显示
`dsh-orbital-inject@link:../../../AppData/Local/OrbitalPro/adapters/dsh-orbital-inject` ✅

### 23.4 最终覆盖（4/4）

| agent | 部署 | 移除 | 自检 | 落点 |
|---|:--:|:--:|:--:|---|
| WorkBuddy | ✅ | ✅ | ✅ | `MEMORY.md` 受控区块 |
| Hermes | ✅ | ✅ | ✅ | `agent-hooks/orbital-recall.py` + CLI 注册 |
| pi | ✅ | ✅ | ✅ | `~/.pi/agent/extensions/orbital.ts` |
| **DSH** | ✅ | ✅ | ✅ | `OrbitalPro/adapters/` + `dsh plugin add` |

**四家的移除策略都是"不删除、可恢复"**：
WorkBuddy 只删区块 / Hermes 只清配置段 / pi 改名 `.disabled` / DSH 走官方 remove（包文件保留）。

### 23.5 一键接入的设计原则（总结）

| 原则 | 做法 |
|---|---|
| 不手改别人的配置 | 每家都走它的**官方入口**（`hermes config set` / `dsh plugin add` / 只改受控区块 / 只写扩展目录） |
| 脚本随程序走 | `include_str!` **编译期嵌入**（防打包漏文件） |
| 落点要持久 | **不用 Temp**（会被清理）→ `%LOCALAPPDATA%\OrbitalPro\adapters\` |
| 移除可恢复 | 一律"不删除" |
| 有反馈 | fixed toast（任何滚动位置可见） |

