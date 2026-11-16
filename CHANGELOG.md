# CHANGELOG

> ⚠️ **条目缺失说明（2026-11-16 核对）**：0.94.0 ~ 0.94.7 的托盘修复链（T-M10-38~45）**未写入本文件**，
> 只散落在 `plans/M10-详细任务卡.md` 的对应卡里。本轮补 0.94.8 并把该链的最终结论集中写在这里。

## [0.94.9] - 2026-11-16

### T-M10-47：端口冲突自动顺延（core 不再被别的软件占死）+ 端口一览判据修正

**起因**：0.94.8 交付时报出"界面 2/3，因为 xray.exe 占了 8791"，用户回"继续"= 批准治本。

**根因 1：启动器注释说顺延，实现里没有**
`launcher.rs` 的文件头写着"冲突时由启动器顺延"，但 `start()` 里只有
`wait_port_free(port, 3000)`，**返回值没人检查**——等 3 秒后照样 spawn 原端口，
core `bind` 报 `os error 10013`（WSAEACCES，本机 xray 以独占方式占着 8791）
→ exit 2 → watchdog 退避重启 3 次 → 放弃 ⇒ 界面永久"进程 2/3"。

修：新增 `pick_free_port(preferred, skip)`（扫 20 个、跳过 skip），
`start()` 与 watchdog 重启都先顺延，并写 `port_shift` 事件到 `launcher-events.ndjson`。
顺延**不持久化**（每次启动重新探测，避免把临时值写死成新默认）。

**根因 2：顺延后界面还探旧端口**
环境变量仍写 8791，而 core 实际在 8792 ⇒ 界面会继续报"2/3"。
修：新增 `actual_port(role, fallback)`，**运行态优先**（`launcher.ndjson` → env → 默认），
`collect()` / `port_probes()` / `onboarding_info()` / 诊断导出的 `env.json` 全部走它。

**根因 3（顺手修掉的假异常）：proxy 明明在听却被判"被其他服务占用"**
`port_probes` 只从 `/health` 提取 `"role":"…"`，而 proxy 的 health 返回的是
`{"service":"orbital-proxy",…}` ⇒ 状态页"端口一览"里 8790 长期挂一条**假异常**。
修：`served_role(body)` 同时识别 `role` 与 `service`。这是"判据写错、误伤被测对象"的典型。

**真机验证（独占占用 8791 复现 xray 场景）**
| 项 | 结果 |
|---|---|
| 判据自查 | `ExclusiveAddressUse` 占 8791 后，自查 127.0.0.1:8791 绑不上（同 10013）✅ |
| 顺延生效 | 日志 `want=8791 actual=8792`；`launcher.ndjson` core=8792 ✅ |
| 界面跟随 | 侧栏 **3/3**、一键体检"三进程健康 3 个进程" ✅ |
| 端口一览 | 8792 core / 8790 proxy / 8799 recall **三条全正常** ✅ |
| 反向证据 | 改前同一页面 8790 显示"异常 · 被其他服务占用（应为 proxy）" ✅ |

单测：src-tauri `cargo test` → **18 passed**（新增 6：3 条顺延 + 3 条 health 判据）。
踩坑记录：并行测试用 `bind(0)` 让内核随机分配端口会互抢（本机曾分到 2515 且立刻绑不上），
改成固定高位端口 + 扫描窗口错开。

证据：`evidence/M10-47-port-shift/`（3 张截图 + README）。

**仍未覆盖**：recall 8799 未顺延（牵连 proxy 的 `ORBITAL_CORE_URL` 约定，属独立任务）。

## [0.94.8] - 2026-11-16

### T-M10-46：界面与托盘弹窗真机修复（三个真根因）

**起因**：用户"对界面和托盘弹出的对话框进行检查、测试、优化"。
前置 0.93.9~0.94.7 已做了五轮修补（失焦隐藏 / × / Esc / toggle / work_area 定位 / 高度 520），
但用户每次都反馈"还是不对"——本轮真机取证后确认，**前几轮的修改有三次从未真正生效**。

**真根因 1：窗口状态插件把临时面板持久化了**
`tauri-plugin-window-state` 把 `tray-panel` 的尺寸/位置写进
`%APPDATA%\com.orbital.pro\.window-state.json`，实测内容是 `tray-panel: 560×735 @ (44,44)`。
下次启动插件先恢复它 ⇒ 代码里的 `inner_size(320,520)` 和 `set_position(…)` **全被覆盖**：
- 高度永远回到旧的 420 逻辑像素（改 520 也没用）⇒ 内容放不下"只显示一半"
- 位置被拉到 (44,44) ⇒ 面板跑到屏幕左上角

修：`Builder::with_denylist(&["tray-panel"])`；`--open-panel` 调试模式下额外 `skip_initial_state("main")`。

**真根因 2：逻辑像素与物理像素混算**
`inner_size(320,520)` 是 **logical**，`monitor.work_area()` 是 **physical**。本机缩放 175%
（`GetDpiForWindow` = 168），窗口真实 560×735 却被当成 320×520 定位 ⇒
右边缘多伸 240px、顶部 y 按 520 算 ⇒ **底部溢出屏幕 131px**（正是"底部被遮挡"）。

修：`position_panel` 全程物理单位（Logical 分支乘 `scale_factor`），并显式 `set_size(PhysicalSize)`。

**真根因 3：创建瞬间失焦被当成"用户点了别处"**
从**溢出区**（`NotifyIconOverflowWindow`）右键时，溢出面板收起动画把焦点还给主窗 ⇒
面板刚 `show()` 就收到 `Focused(false)` ⇒ 立刻 `hide()`（用户看到"闪一下就没"）。

修：`PANEL_ARMED` 原子标志 —— 每次 show 先"撤防" 400ms 再"布防"，避开过渡失焦；
监听器还要求窗口仍可见才 hide（此前一条链路刷 4 条重复日志）。

**界面弹层：补 Esc**
删除确认 / 合并重复 / 建立关联三个 `.modal-mask` 弹层此前**只能点遮罩关闭**（Esc 无反应）。
修：`Memory.tsx` 统一接管 Esc，按"最上层优先"关闭（关联 → 合并 → 删除）。

**其他**
- 面板高度 520 → **300** 逻辑（内容实测只需 ~257）⇒ 去掉一大片留白
- `.panel` 的 `height:100vh` → `100%`（注释早就写着"改 100%"，代码一直没改）
- 托盘 `Move` 事件日志从 INFO 降到 trace（实测一天近 200KB 全是鼠标移动）

**真机验证（本轮，2880×1800 / 175%）**
| 项 | 结果 |
|---|---|
| 面板尺寸 | 560×525 内容（320×300 @175%）✅ |
| 位置 | x=2320 y=1107，底 1632 < 工作区 1716 ✅ |
| 内容 | 标题 / 4 个 agent / 两个按钮全在窗口内 ✅ |
| × / Esc / 点别处 | 三路 `IsWindowVisible=False` ✅ |
| 溢出区右键 | 稳定显示，`focused=true` ✅ |
| 状态文件 | 退出后只剩 `main`，**无 tray-panel** ✅ |
| 界面弹窗 Esc | 建立关联 → Esc 关闭 ✅ |
| 7 页渲染 | 全部正常 ✅ |

证据：`evidence/M10-46-ui-tray/`（11 张截图 + README）。

**仍未验证**：托盘**可见区**右键 toggle（本机图标在溢出区）、175% 以外缩放档。

## [0.93.7] - 2026-11-15

### T-M10-37：agent 安装位置探测（换电脑也能找到）

**起因**：用户问"换到其他电脑上是否能自动搜索安装目录、找到对的路径进行一键部署"。
审计结论：**不能**——路径基础（USERPROFILE/LOCALAPPDATA）随机器自适应，但**没有探测层**：
只查"标准位置的文件在不在"，DSH/Hermes 还依赖 CLI 在 PATH，装在别处一律找不到。

**新增** ` `probe_agent_home(agent)` `（三级策略，从最可靠到最兜底）：
1. ` `where <cmd>` ` —— 官方 CLI（PATH 上的才是真在用的那个）
2. 常见安装目录（用环境变量拼，随机器自适应）
3. 用户级配置目录（` `~/.xxx` `）

**UI**：状态行新增「位置来源：where dsh（PATH）/ ~/.pi」——用户能看到是**怎么找到的**。

**真机验证（本轮截图）**：
| agent | 位置来源 | 状态 |
|---|---|---|
| DSH | ` `where dsh（PATH）` ` | 已部署 ✅ |
| Hermes | ` `where hermes（PATH）` ` | 已部署 ✅ |
| pi | ` `~/.pi` ` | 已部署 ✅ |

**首版 bug（真机截图发现）**：pi 把 ` `where pi` ` 放最前，找到的是**程序目录**而非**配置目录**
⇒ 误判"未安装"。修复：` `~/.pi` ` 优先，PATH 只作旁证。

**仍未覆盖**：agent **完全没装**时，UI 显示"未安装 + 空来源"（合理，但无"去下载"引导）。
## [0.93.5] - 2026-11-15

### T-M10-36：用户指南（发布基础设施第一件）+ HANDOFF 闸门

- **用户指南**：新增 ` `docs/34-用户指南.md` `（面向第一次用的人，10 节：是什么 / 装 / 首开 / 一键接入 / 写 / 召回 / 忘 / 备份 / FAQ / 速查）。
  内测必需（T-M10-02 的测试者要先知道怎么用），且不含技术细节（用户技术背景浅）。
- **HANDOFF 闸门**（T-M10-35）：新增 ` `scripts/check-handoff.ps1` `，判据是 HANDOFF 声明的版本 vs ` `tauri.conf.json` ` 的 version，
  落后即 FAIL。接入 ` `verify.ps1` `。红绿对照：0.93.3 → FAIL exit 1；0.93.4 → PASS exit 0。
  起因：规则写了但无人查，HANDOFF 停在 0.91.8 而实际 0.93.4，连续 20+ 任务无人发现。
- **dsh-dev-guard 插件**：新增 ` `checkHandoffFreshness` `（0.10.0 → 0.10.1）——
  此前只有"读 HANDOFF 当简报"，**从未有"检查"动作**，这是"要求未生效"的真因（见全局 AGENTS.md 地雷 13）。
- **约定修正**：插件读 ` `## 未开任务` ` 段落，项目 HANDOFF 原写 ` `## 下一步` ` → smoke 报「不含任务行」。
  已把 HANDOFF 段落改为「未开任务」，并让 ` `check-handoff.ps1` ` 两个标题都接受。
## [0.93.4] - 2026-11-14

### T-M10-26 ~ T-M10-33：一键接入（部署 / 移除 / 自检 × 四家 agent）

**背景**：四家注入通道接通后，配置仍是**手工**的（改文件、写脚本、注册 hook、装插件）。
换机器/重装后要重做一遍。本轮把它做成设置页里的一个可操作区块。

**设置页新增「记忆注入接入」**（`src/App.tsx` + `src-tauri/src/data.rs`）：

| 能力 | 说明 |
|---|---|
| 状态检测 | 四家适配器是否已部署（判据不只看"文件存在"，还要求"已挂载/已注册"） |
| 部署 | WorkBuddy 写 MEMORY.md 区块 / Hermes 写脚本+`config set` / pi 写扩展 / DSH 写包+`dsh plugin add` |
| 移除 | 一律**可恢复**：只删区块 / 清配置段 / 改名 `.disabled` / 官方 remove |
| 自检 | 用该 agent 身份调 `recall_fused`，toast 显示召回条数 |

**实测（真机）**：
- 四家全部显示「已部署」，部署/移除闭环成立
- Hermes：点移除 → `hooks list` 空；点部署 → `✓ allowed`
- DSH：点移除 → 22 packages；点部署 → 23 packages，link 指向持久目录

**修掉的问题**：
1. **DSH 挂载点误判**：插件挂载在 profile `package.json` 的 dependencies + `dsh.bundle.patch`，
   **不是** `cordis.patch.yml`（首版查错地方，误报"已装但未挂载"）
2. **自检无反馈**：后端 IPC 正常（日志 `done ... hits=3`），前端反馈块被滚出视口 → 改 **fixed toast**
3. **`%TEMP%` 不能放插件**：`dsh plugin add` 以 link 指向它，Temp 清理即失效 → 改
   `%LOCALAPPDATA%\OrbitalPro\adapters\`
4. **toast 显示「备份 undefined」** + **消息重复显示**（0.93.2 修）

**踩到的打包次序坑（第二次）**：`npx tauri build` **不会重编根 workspace 的 `orbital-proxy`**
⇒ 必须先 `cargo build --release -p orbital-proxy`。

## [0.92.1] - 2026-11-14

### T-M10-21：注入热路径性能优化（P95 243ms → 30ms）+ 超时兜底

- **问题**：三路融合召回把注入从 16ms 拖到 P95 243ms / max 519ms（超 300ms 预算 73%）。
  根因：`vector_hits` 每次都实时调 embedding 端点（本机 Ollama，约 200ms）。
- **修复 1（query embedding 缓存）**：全局 `OnceLock<Mutex<HashMap>>`，TTL 300s、容量 64、满了整清。
- **修复 2（注入侧超时兜底）**：`/recall` 新增 `channels` 参数；proxy 三路 300ms 超时后
  **立刻用 FTS-only 重试**（150ms），保证"超时也有东西注入"，不再整轮空注入。
- **实测（50 条真实查询）**：
  | 场景 | P50 | P95 | max | 超 300ms |
  |---|---:|---:|---:|---:|
  | 三路·冷 | 221.8ms | 267.3ms | 515.1ms | 1/50 |
  | **三路·热（缓存）** | **15.8ms** | **30.0ms** | **33.0ms** | **0/50** |
  | FTS-only（兜底） | 15.2ms | 27.0ms | 30.3ms | 0/50 |
- 重复提问（对话最常见）快 **8 倍**；冷启动仍有兜底保证不空注入。

## [0.92.0] - 2026-11-14

### T-M10-19（P0）：WorkBuddy 注入失效修复 + 「提升为共享」静默无效修复

**① WorkBuddy 区块被 4,000 字符上限截断**
- 现象：区块起始位置 4,974 > 上限 4,000 ⇒ 模型看不到，但 WorkBuddy 日志仍报 `injection_success`（假绿）。
- 修复（`crates/orbital-mcp`）：`render_block` 精简 1,642 → **173 字符**；`write_block_atomic` 改为**写入文件开头**（原为追加末尾）；新增预算自检（超 4,000 打 WARN）。
- 真实文件已按新逻辑重写：区块 4,974 → **0**，用户内容**一字未动**（备份 `.bak.orbital-20261114-105205`）。

**② 「提升为共享」按钮静默无效（P0）**
- 现象：`promote-shared` 报 `PROMOTED 4`，库里 `shared` 仍为 **0**。
- 根因：`service.rs` 的 `update` **元数据分支忽略 `p.scope`**——只传 scope 的调用（GUI 按钮正是）返回 Ok 但库内不变。
- 修复：元数据分支补 scope 处理（`shared` 时一并改 `origin=user_endorsed`，ADR-023）。
- 验证：`shared` 0 → **4**；`agent=pi` 能看到 shared 条（跨 agent 可见性成立）。
- 反向证据：修复前同一脚本报 4 成功但库内 0；修复后库内 4。

**③ 三路召回延迟实测**：P50 223ms / P95 243.6ms / max 518.7ms（超 300ms 1/50）——
慢于单路 FTS 15 倍，**注入侧需加 300ms 超时兜底**（下一步）。

测试：130 core + 1 attach + 10 proxy 全绿。

## [0.91.9] - 2026-11-14

### T-M10-18（P0，方案审核发现）：`/recall` 单路 FTS，中文 2 字词召回为 0

- **发现**：审核 `docs/32` 方案时实测 `/recall?q=记忆`、`q=端口`、`q=注入` 全部 `count=0`；
  而 `q=WorkBuddy` 有 12 条。
- **根因**：FTS5 的 **trigram 分词最小匹配 3 字符** ⇒ 中文 2 字词永不可匹配；
  而 `local_api.rs` 调的是单路 `MemoryService::recall()`（只 FTS），
  库里 `embedding_status` 89/89 全 `ok`、`memories_vec` 有数据，**语义通道完全没用上**。
- **影响**：注入走这条端点时，用户问"端口是多少"会得到空注入——等于没接。
- **修复**：`local_api.rs` 改用 `MemoryService::recall_fused()`（Fts + Vector + Graph，RRF 融合），
  响应体新增 `degraded`（"某一路没跑"必须与"没匹配"区分）。
- **验证（0.91.9 实测）**：`记忆` 0→3 · `端口` 0→3 · `注入` 0→3 · `WorkBuddy` 3；`degraded` 为空。
- 测试：根 workspace 全绿；src-tauri 12 passed。

## [0.91.8] - 2026-11-14

### T-M10-15（P0）：主程序被编译成控制台程序 ⇒ 双击弹 CMD，关掉就退出

- **用户反馈**："运行时候有一个 CMD 窗口，关闭就退出了程序"（0.91.7 仍然出现）。
- **真正根因**：`src-tauri/src/main.rs` **缺少 `#![windows_subsystem = "windows"]`**，
  于是 release 版按 **console 子系统**编译 ⇒ 双击 `OrbitalPro.exe` 附带一个 CMD 黑窗，
  关窗 = 杀主进程 = 程序退出。
- **为什么上一轮（0.91.7）误判为已修**：我在终端里用 `Start-Process` 验证，
  终端自己吞掉了控制台，`MainWindowTitle` 查不到 ⇒ 判据错了。
  （铁律：报"已修"前先确认判据本身有效。）
- **决定性证据（PE 头 Subsystem 字段）**：
  - 0.91.7 已安装 exe：`subsystem = 3`（WINDOWS_CUI，会弹 CMD）
  - 0.91.8 新构建/安装后：`subsystem = 2`（WINDOWS_GUI，不弹控制台）
- **修复**：加 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`
  （debug 保留控制台便于看日志，release 用 GUI 子系统）。
- **验证**：安装后 PE subsystem = 2；GUI 进程的 `conhost.exe` 子进程数 = **0**（此前有 PseudoConsoleWindow）；
  `/health` 正常 `version:"0.91.8"`。

## [0.91.7] - 2026-11-14

### T-M10-14（P1，用户正式使用时发现）：运行时弹 CMD 窗口，关掉就"退出"

- **现象**：用户正式使用时看到一个 CMD（控制台）窗口；关掉它程序就没了。
- **根因**：`--role proxy` spawn `orbital-proxy.exe` 时**没有加 `CREATE_NO_WINDOW`**。
  `orbital-proxy.exe` 是**控制台子系统**程序，Windows 会给它分配一个新控制台 ⇒ 黑窗口；
  关掉窗口 = 杀掉 orbital-proxy ⇒ 父进程 `wait()` 返回并 `exit`（watchdog 再拉起，窗口又出现）。
  `launcher.rs::spawn_role` 早就有这个标志，但这个新加的 spawn 点漏了。
- **修复**：`src-tauri/src/main.rs` 的 proxy 分支 spawn 时加 `creation_flags(CREATE_NO_WINDOW)`
  （与 `launcher.rs` 一致；`stdin` 管道保留，父子生命周期绑定不受影响）。
- **验证**：0.91.7 安装启动后，可见窗口只有 `Orbital Pro` 主窗口；
  `orbital-proxy.exe` 的 conhost 变为**隐藏**（不再弹可见控制台）；`/health` 正常 `version:"0.91.7"`。

## [0.91.5] - 2026-11-14

### T-M10-12（P2）：父进程被杀时真 proxy 不退出 ⇒ 配置改了不生效

- **现象**：保存 `proxy.conf.json` 后 `kill --role proxy`，`orbital-proxy.exe` 子进程仍占着 8790，
  watchdog 拉起的新子进程 bind 失败 ⇒ **新配置不生效**（0.91.4 验证时踩到）。
- **修复**：用 **stdin 管道做父子生命周期绑定**（标准库、跨平台）：
  - 父（`--role proxy`）spawn 子时 `.stdin(Stdio::piped())` 并把写端留在变量里（不 drop）；
  - 子进程读 `ORBITAL_PARENT_PID` 启用监控线程：读 stdin 到 **EOF** 即 `exit(0)`；
  - 父被杀 → 系统关闭管道写端 → 子 EOF → 子自杀 → watchdog 拉起新父子 → 新子读新配置。
- 手动运行 `orbital-proxy.exe`（无 `ORBITAL_PARENT_PID`）不启监控，避免终端 stdin 关闭误退。
- 说明：0.91.2/0.91.3/0.91.4 是"配置入口 + 打包次序"迭代，其中 0.91.1 曾把旧 proxy 打进包
  （`tauri build` 不重编根 workspace 的 proxy）；本节起流程固定为"先 cargo build proxy 再 tauri build"。

## [0.91.1] - 2026-11-14

### T-M10-11：设置页「上游与注入」配置入口（ADR-051）

- 新增配置文件 `%LOCALAPPDATA%\OrbitalPro\proxy.conf.json`，**文件 > 环境变量 > 默认**。
- proxy 启动时读它；文件不存在/格式坏只 warn 并回退（不拒绝服务）。
- GUI：设置页新增一组「上游与注入」（上游 API 输入框 + 自动注入开关 + 保存按钮 +
  当前生效来源 + 配置文件路径）。
- 保存流程：原子写文件 → `kill --role proxy 子进程` → 启动器 watchdog 自动拉起新进程读新配置
  （不新写 reload 逻辑，复用既有指数退避重启）。
- 新增 IPC：`proxy_config`（读）/ `save_proxy_config`（写 + 重启）。
- 仍未做：默认 `shared` 记忆为 0（开箱注入 0 条）——属产品/安全决策（ADR-042 边界），待拍板。

## [0.91.0] - 2026-11-14

### 🔴 P1：`--role proxy` 是空壳，注入功能在发布版里从未运行

- **发现**：为验证"注入从未成功"，起测试 proxy（18790）打 `POST /chat/completions`，却返回 `/health` 的 JSON。
  追代码发现 `src-tauri/src/main.rs`：`"proxy" => { serve_http("proxy", …); return; }` ——
  `serve_http` 对**任何请求**都只回 health，**不转发、不注入**；
  而真正的 `orbital-proxy`（含转发与注入）的二进制从未被打包（安装目录只有 `OrbitalPro.exe` + `orbital-mcp.exe`）。
  后果：① 核心卖点"自动注入"在发布版里根本不存在；② 界面「3/3 进程健康」是**假绿**（空壳也回 health）。
- **修复**：
  1. `--role proxy` 改为 **spawn `orbital-proxy.exe`** 并透传退出码；找不到就明确报错退出，不再用空壳假装就绪；
  2. `tauri.conf.json` 的 `bundle.resources` 新增 `orbital-proxy.exe`；
  3. 版本 0.91.0。
- **判据**：装完后 `--role proxy` 的进程是 `orbital-proxy.exe`；对 proxy 发 `POST /chat/completions`
  应返回 `502 upstream_not_configured`（而不是 health JSON）。
- 关联：`docs/07` ADR-050；`evidence/M10-01-metrics/injection-timeout-diagnosis.md`。

## [0.90.8] - 2026-11-14

### 修注入统计的空态判定漏洞（真实数据暴露）

- 真实 `metrics/injection-2026-11-13.jsonl`（400 行）里 status 全是 **`error:timeout`**——
  既不是 `on` 也不是 `off`。原判定 `injected==0 && off==total` 会漏掉它，
  界面会显示"有 400 次请求但 0 注入"，**却没有任何解释**。
- 修正：判据改为「**一次都没真正注入**」（`injected_rows == 0`），再细分两种原因：
  - `all-off`：全是 `off`（开关没开 / agent 没走 proxy）
  - `all-skipped`：**新态**——有请求但每次都被跳过/超时（如 `error:timeout`）
- 前端补 `all-skipped` 文案：说明"常见原因：core 召回端点没起，或 300ms 超时被触发"。
- 单测补第 ⑤ 种空态断言（`reports_four_empty_reasons` 现覆盖 5 种）。
- 数据层复算（python 独立实现）与 Rust 聚合口径一致：400 行 / 0 注入 / 400 跳过 / P50=P95=0。

## [0.90.7] - 2026-11-14

### T-M10-06：J6 注入统计（状态页卡片 + 只读 JSONL 聚合，ADR-049）

- 后端 `status::injection_stats(days)`：读 `%LOCALAPPDATA%\OrbitalPro\metrics\injection-YYYY-MM-DD.jsonl`
  聚合出「注入次数 / 条数 / token / 缓存命中率 / 跳过超时 / P50·P95 / 每日趋势 / 坏行数」。
  **只读文件，不建表不写库**（ADR-008）。
- **四种空态**分别返回原因（`no-dir` / `no-file` / `no-rows` / `all-off`），界面分别显示，不给 `0%` 假绿。
- 窗口按**文件名日期**截断（不是 mtime），支持今日 / 7 天 / 30 天。
- 前端：状态页新增「注入统计」卡片（在「一键体检」之上）；窗口切换按需读，不跟 3 秒轮询；
  趋势用内联柱（不引新图表库）。**MVP 不区分 agent**（JSONL 无该字段，界面已注明）。
- 单测 3 个：聚合+坏行、四种空态、窗口截断。
- 说明：0.90.6 只是文案修正（「其中」→「另有」历史版本），未单独记 CHANGELOG。

## [0.90.5] - 2026-11-14

### T-M10-05：计数口径显式化（ADR-048 落地）

- 后端 `Overview` 新增 `total_history`：
  `deleted_at IS NULL AND (superseded_by IS NOT NULL OR valid_to IS NOT NULL)` 的条数。
- 前端：
  - 记忆页标题：默认视图下追加「· 其中历史版本 N 条（已被新版本替代，按"当前有效"口径不计入召回）」；
    **带筛选时不显示**（筛选后的 total 与全局 historyCount 不同源，混用会给出错误口径）。
  - 设置页「记忆总数」：M>0 时追加「（含历史版本 M 条）」。
- `docs/04` 新增 §2.9「计数口径」：三层口径 + 各自消费方 + 为什么不全并成一个数 + 加注只在默认视图。
- 不改 `total` 的 SQL 语义（ADR-048 被否方案）：总览/托盘/体检/备份校验四处消费者零改动。

## [0.90.4] - 2026-11-14

### 修 P1 根因：`attach_json` 写完配置后又轮换一次令牌（配置与库不同源）

- **定位方式**：新增单元测试 `json_attach_writes_token_matching_db_hash`，
  用临时配置 + 临时库直调 `attach_by_spec`，断言
  `sha256(配置里的 ORBITAL_AGENT_TOKEN) == agents.token_hash`。
  **复现为红**（`022a3799…` vs `f39672b5…`）⇒ 证明是代码 bug，不是环境/并发。
- **根因**：`attach_json` 在**写文件成功之后**还调了
  `register_agent(db.conn(), agent_id, agent_id)?`（旧 `:149`）——
  配置里已写下 token T1，这行又把库里换成 T2 ⇒ agent 拿 T1 鉴权必败。
  （这正是"库 hash 每次点击都变、配置 hash 总对不上"的来源。）
- **修复**：
  1. 删除该次 `register_agent`；改为**从刚写入的配置读回 token**（配置里的才是真值）；
  2. `attach_json` 不再自己生成令牌（`db` 参数改 `_db`），令牌统一由 `attach_by_spec` 生成后传入；
  3. 同步修正 `patches_json_and_creates_backup`（旧断言 `out.token.len()==64` 反映的是旧设计），
     改为断言"返回 token == 写入配置的 token"。
- 验证：`cargo test --workspace` **141 passed**（130 core + 1 drill + 10 proxy）；clippy **0 warning**。

## [0.90.3] - 2026-11-14

### 修 P1：一键接入令牌与库不一致（串行化）+ 修设置页版本号不更新

- **P1（一键接入令牌）**：用户报告"设置里版本号不是最新"的同时，P1 仍未解。
  - 根因假设：**同一次点击触发多次 attach**（并发进入，各自 `register_agent` 轮换，最后配置与库各留一个）。
  - 修复：`data::attach_agent` 加**进程内互斥锁** `ATTACH_LOCK`，接入串行化。
    第二个调用会看到"配置已写好且 env 完整"→ 走幂等路径→不再轮换令牌。
  - 取证：`attach_by_spec` 每次真正轮换令牌时打 `tracing::info!`（只记长度，不打印令牌）。
- **设置页版本号不更新（用户实测发现）**：`App.tsx:80` 硬编码 `const APP_VERSION = "0.72.0"`，
  0.90.x 打包后「关于」仍显示 0.72.0。
  - 修复：`vite.config.ts` 用 `define` 从 `package.json` 注入 `__APP_VERSION__`；
    `App.tsx` 改为读取该常量（`tsconfig` 已开 `resolveJsonModule`）。
  - 验证：`dist` 产物里 `0.72.0` 出现 **0 次**、`0.90.x` 出现 2 次。
- 测试：`cargo test --workspace` **140 passed**；clippy **0 warning**；`npm run build` 通过。

## [0.90.2] - 2026-11-14

### 修 P2：一键接入的幂等路径会轮换 token 却不写回配置

- **背景（0.90.1 实测遗留）**：0.90.1 装好后 detach → 点「一键接入」，配置里写入了 64 位真实 token，
  但 `sha256(配置里的 token)` **不等于**库里 `agents.token_hash`（实测 `04fe74aa…` vs `5411fbcf…`）。
- **根因**：`attach_json` 的 `!changed`（幂等）分支仍调用 `register_agent` 轮换令牌，
  但不把新 token 写回配置 ⇒ 配置里是旧 token、库里是新 hash，**agent 立刻失联**。
  前端一次点击可能触发两次 attach，第二次正好落进该分支。
- **修复**：
  1. `attach_by_spec` 的 JSON 分支：`json_attach_needs_rewrite == false` 时**直接返回 changed=false**
     （返回配置里已有的 token，不碰库、不写文件）；
  2. `attach_json` 的幂等分支**移除** `register_agent`（只返回 `token: ""`，调用方不再依赖它）；
  3. 新增 `json_read_env` helper。
- 验证：`cargo test -p orbital-core attach` 20 passed + drill 1；clippy 0 warning。

## [0.90.1] - 2026-11-14

### 修 P2：一键接入把 agent 的 `ORBITAL_DB_PATH`/`ORBITAL_AGENT_TOKEN` 写成空串

- **触发**：T-M10-07 真机验 J7（detach pi → 点「一键接入」）时实测发现
  `%USERPROFILE%\.pi\agent\mcp.json` 的 orbital 条目 env 变成 `""`（此前有 token 与库路径）。
- **根因**：`server_spec_for` 写死 `"ORBITAL_DB_PATH": ""` / `"ORBITAL_AGENT_TOKEN": ""`。
  - 空 token → agent 无身份，写入被门控拒绝；
  - 空 DB 路径 → `orbital-mcp` 的 `env::var(...).unwrap_or_else(default)` 只在**变量缺失**时回退，
    空串会拿去做 `Db::open("")`，直接打不开库。
  - 后果：点一次「一键接入」反而把本来好的配置弄坏。
- **修复**：
  1. `server_spec_for` 增加 `token`/`db_path` 参数，写真实值；
  2. `attach_by_spec` 的 JSON 分支在需要写入时 `register_agent` 生成令牌、用 `PRAGMA database_list` 取本库绝对路径；
  3. 新增 `json_attach_needs_rewrite`：**已接入且 env 完整时不轮换令牌**（幂等），
     **env 为空则视为需要重写**（旧坏配置可自愈）。
- 验证：`cargo test --workspace` **140 passed**（129+1+10）；clippy **0 warning**；attach 测试 20 + drill 1 全绿。

## [0.90.0] - 2026-11-13

### M10 首个打包版本：版本号统一 + 合并撤销 P2 修复 + J8 一键体检进包

- **版本号四处统一为 `0.90.0`**：`tauri.conf.json`（单一真源）/ `package.json` / `Cargo.toml` / `src-tauri/Cargo.toml`
  - 之前：`0.1.0` / `0.72.0` / `0.1.0` / `0.1.0`（四处不一致，用户报 bug 无法定位）
- **P2 修复（ADR-047）**：合并撤销会误删 keep 原有的边
  - `merge_inner` 记录 `inserted`；`undo_merge` 按标志条件删除
  - 红绿：新测试修复前 `left:0 right:1` 红 → 修复后绿
  - 文案：结果提示追加「N 条边变成自环已丢弃」
- **J8 一键体检进包**：源码 `App.tsx:1090-1110` 早于本次构建，重新打包后用户可见
- **J7 一键接入进包**：连接页卡片按钮（`!c.attached` 时显示）
- 测试：`cargo test --workspace` **129 passed**（128 core + 1 drill + 10 proxy）；clippy **0 warning**
- 顺手：补 `conflict_memories_are_excluded_from_recall_until_resolved` 的 `#[test]`（此前从未运行）
- ⚠️ 仍未签名（安装时显示"未知发布者"）；仍无自动更新（证书与 updater 待 T-M10-03）

## [0.83.0-m10-kickoff] - 2026-11-13

### M10「可验证（Beta）」立项 —— 只改计划文档，**未出新安装包**

- 触发：用户问「完整工程项目现在该做哪一步」→ `docs/28` 调研（Google SRE Launch Checklist /
  Production Readiness Review / Release Readiness Gate 16 维 / Tauri 官方 Distribute+Updater）
- 关键判据：**里程碑验收 41/41 全勾，但 `docs/05` 全局成功指标 0/6 被测**
- 新建 `plans/M10-详细任务卡.md`（T-M10-01 指标实测 / T-M10-02 内测 / T-M10-03 发布基础设施）
- `docs/05` 补 M7~M10 里程碑行（编号归位）、新增 M10 章节（8 条未勾验收），并修 3 处不一致：
  体检 27 项→**11 项**、`scripts/recall-bench` 未建→**已建**（46/50·MRR 0.9267）、全局指标"未测"加去向
- `PLAN.md` 当前任务改 `T-M10-01`；`HANDOFF.md` 当前状态与下一步同步
- 门禁：`check-plan` **PASS**（串跑 `check-docs` **PASS**）
- ⚠️ 版本号仍未统一（本行 `0.83.0` 是能力版本号；安装包仍是 `0.72.0`）→ 归 T-M10-03①

## [0.82.0-acceptance-complete] - 2026-11-14

### T-M9-01 完成 12/12 —— **`docs/05` 验收全勾完（41 已勾 / 0 未勾）**

最后两条的收官方式：

**① 公式参数改动后候选变化可解释（抽样 10 条）** —— 上一轮**失败过一次**，这轮换方法成了

上一轮失败原因（已诊断）：免疫规则 `importance≥4||access≥3` 先筛掉一批；老化脚本 `days` 与 `access` **同余**
（60 天档必配 access=0、365 天档必配 access=3 全被免疫）→ 两档都是 34 条候选、进入/退出都是 0。

这轮修正：
- `scripts/age-fixture.py`：days **4 档 × access 3 档交叉**（不再同余）
- `examples/retirement-diff.rs`：加**阈值扫描**（0.35 时常"全入选"看不出差异，自动找到 0.15 才分得开）

结果（`evidence/M9-01-retire-diff.txt`）：

```
半衰期 30 天（阈值 0.15）：36 条候选
半衰期 10 天（阈值 0.15）：50 条候选
进入候选：14 条 —— 全部「40 天未访问 + access 0~1 + importance 3」
退出候选：0 条
可解释性判据（变短 → 新增应更老）：PASS
```

方向正确（变短 → 加速老化 → 更多入候选）且**无一条因变短而退出**（单调性）。

**② `docs/04` 每条设计可追溯** —— 新增 **§10 设计追溯表**

20 个设计点逐条标依据：**有 ADR/调研证据的 15 个**，标「我们的选择」的 5 个（都写了理由），**无空条目**。

**③ 顺带：`check-plan.ps1` 支持"全部完成"状态**

项目所有里程碑完成后，PLAN 当前任务没有 id 可填。加了严格受限的放行分支：
仅当 **PLAN 标「—」且 `docs/05` 未勾 = 0** 才 PASS。
**红绿对照**：临时加一条未勾 → `FAIL 无法从 PLAN.md 解析出当前任务 id`；删掉 → PASS。

**验证**：`cargo test --workspace` → 127+1+10 = **138 passed**；
`check-links` / `check-docs` / `verify` / `check-plan` → 全 PASS；真实库 50 条未污染。

## [0.81.0-tray-panel] - 2026-11-13

### T-M8-01 完成 6/6（**M8 里程碑完成**）

**本轮修的真缺陷：`?panel=1` 从未被消费**

`tray::show_panel` 一直加载 `index.html?panel=1`，但 `main.tsx` 从来没读这个参数 ——
右键托盘实际是把**整个应用**塞进 320×420 的窗口（布局被裁切）。

- 新增 `src/PanelView.tsx`：紧凑面板，**一项一 Agent**（状态灯 / 记忆条数 / 接入状态 / 配置格式）
- `main.tsx` 按 `?panel=1` 分流；`styles.css` 补 `.panel*`；`tray.rs` 加 `show_main_window` / `quit_app`
- 新增 `--open-panel` 启动参数（与右键托盘走**同一条** `show_panel` 路径，同时给自动化验证一个入口）

**真机截图**（`evidence/M8-01-tray-panel.png`）
```
Orbital                        50 条记忆 · 进程 3/3
  ● DSH         1   已接入   yaml
  ● Hermes      1   已接入   yaml
  ● pi          1   已接入   json
  ● WorkBuddy  47   已接入   json
  [ 打开主窗口 ]  [ 退出 ]
```

**6 项总账**

| # | 项 | 结果 |
|:--:|---|---|
| 1 | M6 6.3 设置页 6 组 + 页内搜索 | ✅ 真机 |
| 2 | M6 6.4 托盘一项一 Agent | ✅ 真机（本轮） |
| 3 | M6 卸载"询问" | ✅ 改口径「默认保留 + 安装时明示」+ 文档写明 |
| 4 | M3 冲突不进入注入（对抗测试） | ✅ 并修出两个裁决缺陷 |
| 5 | M4 宽窄事实退役 | ✅ 废止（ADR-045） |
| 6 | M1 用户确认冻结 | ✅ 标历史项 |

**验证**：`cargo test --workspace` → 127+1+10 = **138 passed**；tsc/build/tauri build 通过；
`docs/05` 6.3/6.4 均标 ✅ 附截图；check-docs / check-plan PASS。**remaining: 0**

## [0.80.0-docs] - 2026-11-13

### T-M7-03 文档与代码零漂移收官（**M1~M7 全部里程碑完成**）

**修了 3 处真漂移**
1. **`docs/02 §3.2` 缺 8799** —— 只写了"检查端口占用（8790/8791）"，完全没有召回端点的 8799。
   这正是 M5 那个 P0（core 与召回抢 8791）的文档根源。已补完整端口表 + "必须是两个端口"的血的教训。
2. **`docs/05` 写"旧库拒绝服务"但实现是迁移** —— 写 **ADR-044**：旧库走迁移（保留数据），
   只有未来版本才 `SCHEMA_TOO_OLD`；理由：拒绝服务 = 升级即不可用，与 M6"升级不丢数据"冲突。
   同步改 `docs/05` 口径 + 已废止清单。
3. **`check-docs` 废弃词太松** —— 加 3 条：
   `旧库（v1）拒绝服务` / `缓存命中 P95 未做` / `注入降级为 skipped`

**docs/05 勾选 22 → 25**（每条附证据）
- 缓存命中 P95 < 10ms（`evidence/inject-cache-20261113.md`，命中 P95 = 0ms）
- 安装包干净目录安装（`evidence/installer-20261113.md`）
- 无本机配置路径完成安装与首启（`evidence/fresh-machine-20261113.md`）
- v1 迁移并保留数据（`evidence/migration-20261113.md`）

**红绿对照**：造旧写法 → `FAIL 废弃术语 '旧库（v1）拒绝服务' 出现在 09-踩坑与反模式.md:201`；
删掉 → `CHECK-DOCS: PASS`。（脚本 BOM 已核 `239,187,191`）

**仍未勾的 6 项如实列出**（不硬凑）：M6 6.3 设置页 6 组、6.4 托盘一项一 Agent、卸载"询问"、
M3 冲突对抗测试、M4 宽窄事实、M1 用户确认冻结 → 已建 `T-M8-01` 逐项清理。
报告 `evidence/doc-drift-20261113.md`

## [0.79.0-a11y] - 2026-11-13

### T-M7-02 键盘可达（真机走查通过）

- **焦点环**（`src/styles.css`）
  - 全局 `:where(a,button,input,textarea,select,summary,[tabindex]):focus-visible` → `outline: 2px solid var(--accent)`
  - 修掉两处历史隐患：`.edit-area` / `.tag-input` 原本 `:focus { outline: none }`（**焦点完全不可见**），
    改为"只对鼠标聚焦关、键盘聚焦保留"
  - `.item:focus-visible { outline-offset: -2px }` 让侧栏导航的环贴圆角
  - 全部用 token，无裸色值
- **快捷键**（`src/App.tsx`）
  - `Ctrl/Cmd + K` → 跳记忆页（清空待办筛选）
  - `Esc` → 关闭「确认从备份恢复」弹层

**真机 4 张截图**（`evidence/M7-02-*.png`）
```
① Tab → 「DSH」条目出现蓝色焦点环
② 连按 5 次 Tab → 焦点沿侧栏走到「记忆」（顺序符合视觉）
③ Ctrl+K → 切到记忆页（搜索框 + 列表）
④ 设置页点「恢复」→ 弹层 → Esc → 弹层关闭
全程未碰鼠标（只用 keypress / invoke）
```

> 上一轮因 `COMPUTER_USE_BUSY`（另一会话占用桌面）阻塞，当时**没有**标完成；
> 本轮桌面释放后补齐。教训：键盘操作前必须先 `activate_window`，否则 Tab 会发给别的窗口。

**验证**：`tsc` / `build` / `tauri build` 通过；工作区测试 **133**

## [0.78.0-inject-cache] - 2026-11-13

### T-M7-01 注入缓存 + P95（补上 M3 遗留的缓存验收）

`docs/05` M3 那条"缓存命中 P95 < 10ms"一直没勾（当时如实标注"proxy 无会话缓存"）。本轮补上：

- **`orbital-proxy`**
  - `InjectCache`：TTL **5s** + LRU **32**，键 = `fnv1a(最后一条 user 文本)`
  - **只缓存"要注入的文本行"**，不缓存用户请求体（对话内容不留内存）
  - **默认关闭**，`ORBITAL_INJECT_CACHE=1` 才启用 → 不改变默认行为
  - `try_inject` 拆成 查缓存 / `fetch_and_budget` / `build_injected`
  - metrics jsonl 加 `cache: "hit" | "miss"`
- **`scripts/inject-p95.py`**：200 次 × 开/关两轮，从 metrics 读 `elapsed_ms` 算分位

**实测（本轮新跑）**

| 模式 | samples | cache | p50 | **p95** | max |
|---|---:|---|---:|---:|---:|
| 关缓存 | 200 | miss 200 | 4 ms | **6 ms** | 20 ms |
| 开缓存 | 200 | **hit 199 / miss 1** | **0 ms** | **0 ms** | 8 ms |

判据（命中 P95 < 10ms）→ **PASS**。

**回归**：`cargo test -p orbital-proxy` → 10 passed（新增 4 条缓存测试）；
`cargo test --workspace` → **133 passed**；`smoke-inject.py` → **11/11 PASS**；
真实库 50 条未污染。报告 `evidence/inject-cache-20261113.md`

**排查记录**（三处坑已写进报告）：`recall-server` 参数是 `<db> <bind地址>` 且端口读 stdout；
proxy metrics 在 `%LOCALAPPDATA%\OrbitalPro\metrics`；只 `cargo check` 不重建二进制导致 `cache` 字段恒 0。

## [0.77.0-e2e] - 2026-11-13

### T-M6-05 干净机器端到端（**M6 里程碑 5/5 完成**）

**隔离方式（一个失败尝试 + 最终方案）**
- ❌ 整包隔离 `%LOCALAPPDATA%` → **GUI 起不来**（Tauri/WebView2 与单实例锁依赖真实用户目录）
- ✅ 小侵入：`ORBITAL_DB_PATH` → 临时空库；`USERPROFILE` → 临时目录（其下造 `.pi/agent/mcp.json={"mcpServers":{}}`）；
  `LOCALAPPDATA` 保持真实

**真机跑通（4 张截图 `evidence/M6-05-*.png`）**
```
① 首启引导出现，②走"还没配嵌入端点"分支（该分支首次真机验证）
② 连接页 pi「配置文件存在，未接入」+ 按钮亮；DSH/WorkBuddy「配置文件未找到」按钮灰；Hermes「已配置」
③ 点「一键接入」→ confirm 弹窗列明目标文件（隔离路径）+ 会备份 + 需重启
④ 确定后：「已接入 pi …（原文件已备份）」；卡片变「MCP 已配置」；同目录生成 mcp.json.bak.orbital
   写入内容 command = 安装目录\orbital-mcp.exe
```
> 这一条**补上了 T-M6-04 遗留的真机按钮验证**（真实环境 4 个 agent 都已接入，按钮不显示，无法验证）。

**新功能：`SCHEMA_TOO_OLD` 界面提示**
- `main.rs` 迁移失败时存 `SchemaIssue` → `status.rs` IPC `schema_issue` → `App.tsx` 顶部红条
- 文案明确说明"库来自更新版本请升级程序，**不要**手动删库（数据可能完好）"
- 为什么必须做：否则用户看到"记忆 0 条"会以为数据丢了

**验证**：`tsc` / `build` / `tauri build`（6.89 MB）通过；隔离环境 3 进程、空库创建；
清理隔离目录后真实库复核 **50 条未污染**；报告 `evidence/fresh-machine-20261113.md`

## [0.76.0-migration] - 2026-11-13

### T-M6-03 升级路径（旧库 → 当前版本，不丢数据）

迁移逻辑（v1→v2→v3）一直在，但从没演练过。本轮补上**单测 + 真机**两层证据：

- **`db.rs::migration_tests`（4 条）**
  - `v1_migrates_to_v3_keeping_data`：记忆/事件行数不变 + 新表就位
  - `v2_migrates_to_v3_keeping_data`：conversations 数据保留
  - `future_version_is_rejected`：v4 库 → `SCHEMA_TOO_OLD`，且**被拒绝时没改库**
  - `migrated_v1_db_is_readable_by_service`：迁移后能**真的读出来**（原属主读到 v1 数据 + 新身份能写）
- **`scripts/make-v1-db.py`**：从 `schema.rs` 正则抽取 `SCHEMA_V1` 造 v1 库（不手抄 DDL，避免漂移）
- **真机（release 版，与安装包同源）**

```
启动前：user_version=1 · memories=1 · events=1 · 无 conversations/conflicts 表
启动后：user_version=3 · memories=1 · events=1 · 两表已建 · 进程 3 个
```

**演练中发现的测试假设错误**（不是产品 bug，如实记录）：v1 的 `memories` 早已不含 `layer` 列；
`conversations` 的列是 `session_id/round_index/role/content`（没有 `title/updated_at`）。

**验证**：`migration_tests` 4 passed；`cargo test --workspace` → **126 + 1 + 6 = 133 passed**；
报告 `evidence/migration-20261113.md`

## [0.75.0-attach-gui] - 2026-11-13

### T-M6-04 GUI 一键接入（连接页真实状态 + 一键写入）

- **`orbital-core::adapter`**
  - `AgentConnection { id, display_name, config_path, format, exists, attached, supports_mcp, note }`
  - `detect_connections()`：4 条内置 spec → `resolved_path()`（展开 `%VAR%`/`~/`）→ 读文件 → 判断
  - `looks_attached(text)` 抽成**纯函数**（可单测；`detect_connections` 读真实路径没法安全注入）
- **IPC**：`agent_connections()` / `attach_agent(id)`
  - attach 复用 `attach_by_spec`（**先备份原文件**、幂等、拒绝 `supports_mcp=false`）
  - **不回传明文令牌**（红线 9）
- **连接页**：状态行（已配置/未接入/agent 没装）+「一键接入」+ `window.confirm`（列明目标文件、会备份、需重启）

**真机**（`evidence/M6-04-connect-page.png`）：4 张卡全显示「MCP 已配置 · 配置文件存在」，
与手工 grep 4 个真实配置文件**完全一致**；因此都不显示接入按钮（条件是 `!attached`，逻辑正确）。

**单测**：`connection_tests` 4 passed（JSON/YAML 判断、空文本、4 条 spec 路径已展开成绝对路径）。

**遗留（如实）**：**未在真机点过「一键接入」**——真实环境 4 个 agent 全部已接入，且项目红线禁止
为测试去动真实 agent 配置；写入路径已有 `attach_drill.rs` 在配置**副本**上的演练覆盖。

## [0.74.0-onboarding] - 2026-11-13

### T-M6-02 新机全流程 + 首启引导（含一个真缺陷修复）

**真缺陷**：安装包**只打了主程序**，没带 `orbital-mcp.exe` → 安装完无法接入任何 agent。
修复：`bundle.resources` 打包 mcp 二进制，安装包 4.08 → **6.81 MB**。
安装后目录：`OrbitalPro.exe`(13.91 MB) + **`orbital-mcp.exe`(7.78 MB)** + `uninstall.exe`。

**验证：用安装版 MCP 跑冒烟 → `SMOKE: PASS`**（16 项断言）。

**首启引导**（`evidence/M6-02-onboarding.png`）
- 空库（`ov.total === 0`）时出现「第一次使用 · 三步就能跑起来」
- ① 数据目录（真实路径）② 语义召回状态（读 `~/.orbital/secrets.json` 动态判定）
  ③ agent 接入片段（含本机真实 mcp 路径，可直接粘贴）
- **不引入"已看过"持久化**（红线：不用 localStorage 存业务状态）；有记忆后自动不出现

**验证**：`cargo build --release -p orbital-mcp` → 7.78 MB；`npx tauri build` → 6.81 MB；
安装 `/S` 退出码 0；空库启动看到引导 + 总览 0 条；真实库复核仍 50 条（未被污染）

**遗留（已拆 T-M6-04）**：`attach` 接入能力仍未进 GUI，连接页没有「一键接入」按钮

## [0.73.0-installer] - 2026-11-13

### T-M6-01 安装包（M6 第一项）+ src-tauri 零告警收尾

**根因**：`src-tauri/tauri.conf.json` 的 `bundle.active` 一直是 **`false`** —— 所以 `npx tauri build`
只产出裸 exe，从不跑 NSIS。

**改动**
- `bundle.active: true`、`targets: ["nsis"]`、版本 `0.1.0` → **`0.72.0`**
- 真实元信息：`publisher`/`copyright`/`category`/`shortDescription`/`longDescription` + 三图标
- `installMode: "currentUser"`（装到 `%LOCALAPPDATA%\Orbital Pro`，免管理员、不写 Program Files）
- 产物：`Orbital Pro_0.72.0_x64-setup.exe`（**4.08 MB**）

**真机四项验收全过**
```
① 安装  /S → 退出码 0；注册表卸载项 + 开始菜单快捷方式就位
② 启动  /health → {"status":"ok","role":"core","pid":42728}；core 8791 + proxy 8790；界面 3/3 健康
③ 目录  OrbitalPro.exe(14.2MB) + uninstall.exe(79.6KB)；data/backups/logs 均不存在
④ 卸载  /S → 目录消失、注册表清、快捷方式清；而 orbital.db / backups / logs 全部保留
```
截图 `evidence/M6-01-installed-running.png`；报告 `evidence/installer-20261113.md`

**顺带：src-tauri clippy 7 → 0 warning**（删未用 `exe` 字段与两个方法、`Children` 类型别名、
3 处 `if let` 收成 `flatten()`、1 处 collapsible-if；测试 9 passed）

## [0.72.0-status] - 2026-11-13

### T-M5-05 状态页完善（**M5 里程碑 5/5 完成**）

- **`status.rs` 新增 `port_probes()`**：`get_http` 最小探测（600ms 超时）
  - 8791/8790 用 `/health` 判断 `role` 是否符合端口表
  - 8799 用 `/recall?q=probe&limit=1` 判断是否返回 `"count"`
- **IPC** `port_probes`；状态页新增三张卡片：**备份状态** / **向量健康** / **端口一览**
- **修 bug**：补算成功后「已算/待算」不刷新（父组件 `ov` 未更新，用户会以为没生效）
  → `StatusPage` 加 `onChanged` 回调，备份/补算后刷新 overview

**真机证据**
```
3/3 健康 · 备份 1 份（11-13 14:15, 592KB）
点「补算缺失向量」→ "补算完成：尝试 3，成功 3，失败 0"
向量健康：已算 50 · 待算 0（共 50 条）
侧栏「维护」角标 3 → 2（缺向量一项归零）
端口一览：8791 core / 8790 proxy / 8799 recall —— 全部「正常 · 符合端口表」
查库复核：embedding_status='ok' 50、pending/failed 0
```
截图 `evidence/M5-05-status-page.png` / `M5-05-reembed-done.png`

> 本轮同时**覆盖了 M3 遗留的「有端点补算成功路径」**——`docs/05` 里那条一直没有真机证据的项。

**验证**：`tsc`/`build`/`tauri build` 通过；报告 `evidence/status-page-20261113.md`

## [0.71.0-diag] - 2026-11-13

### T-M5-04 日志与诊断（日志落盘 + 诊断包 + 脱敏）

- **新模块 `src-tauri/src/diag.rs`**
  - `append_log(role, bytes)`：`logs/{role}-YYYYMMDD.log`；失败静默（日志不该拖垮主流程）
  - `prune_old_logs()`：启动清理 >7 天，判据用**文件名日期**（mtime 会被拷贝改掉）
  - `export_diagnostics()`：目录形式诊断包（7 文件：3 份日志 + `launcher.ndjson` + `launcher-events.ndjson` + `db-info.json` + `env.json`）
  - 脱敏：命中 `sk-`/`Bearer `/`token=`/`api_key`/`password` 的**整行**替换 `<REDACTED len=N>`
- **`main.rs`**：`TeeWriter` 让 tracing 同时写 stderr + 文件（gui）；`serve_http` 三处 `append_log`（core/proxy）
- **IPC** `export_diagnostics`；设置页「日志与诊断」卡片（按钮 + 落点路径 + 文件清单）

**真机**：点「导出诊断包」→ "已导出 7 个文件、脱敏 0 行"；
`db-info.json` 只有计数（memories 50 / events 50 / agents 4 / schema v3）；
`env.json` 含 M5-03 确立的端口表（core 8791 / proxy 8790 / recall 8799）。
截图 `evidence/M5-04-diag-export.png`

**脱敏红绿对照**：往 gui 日志注入 `sk-... / Bearer ...` → 再导出 → 该行变 `<REDACTED len=83>`，
包内搜 `sk-` **0 命中**；测试行已从真实日志清理。

**验证**：`cargo test --manifest-path src-tauri/Cargo.toml` → **9 passed**（新增 3 条）；
`npm run build` / `tauri build` 通过；报告 `evidence/diag-20261113.md`

## [0.70.0-selfheal] - 2026-11-13

### T-M5-03 崩溃自愈（含 **P0 端口冲突修复**）

**审计先说结论**：崩溃自愈的核心（监控线程 + 1/2/4s 退避 + 上限 3 次）**T-M1-06 就已实现**。
本任务实际是"补可观测性 + 修 P0 + 真机验收"，任务卡口径已按实现修订（原写"上限 30s / 5 次"）。

**P0：core 根本起不来**
- `--role core` 的 health 端点与 GUI 的只读召回端点**都绑 8791**，谁先绑谁活
- 本次 GUI 先绑上 → core `bind` 失败 **exit code 2** → 自愈 3 次全失败 → 放弃 → **记忆核心完全不可用**
- 修复：召回端点改 **8799**（`main.rs` + proxy 默认召回 URL）；core 保持 8791

**可观测性**
- `MAX_RESTARTS = 3` 提为常量；重启分支带出 `status.code()`
- 新增 `launcher-events.ndjson`（`at/role/event/exit_code/restarts`）
- `runtime_state()` / `restart_events(limit)` + IPC `launcher_state` / `launcher_events`
- 状态页：非 GUI 角色显示「崩溃自愈重启 N 次（上限 3 次）」+「最近的重启记录」

**真机证据**
```
修复前（M5-03-before-portfix.png）：Core 不可达 · 重启 3 次 · 5 条「已放弃 · 退出码 2」
修复后（M5-03-after-selfheal.png）：3/3 健康 · Core 重启 1 次 · 「已重启 · 退出码 -1 · 第 1 次」
GET 8791/health → {"status":"ok","role":"core","pid":54456,...}
GET 8799/recall?q=orbital&limit=2&scope=shared → {"count":0,"hits":[]}
杀 core → 5s 后 restarts:1、新 pid 60292、/health 200
```
报告：`evidence/selfheal-20261113.md`

## [0.69.0-restore] - 2026-11-13

### T-M5-02 从备份恢复（M5 门禁后半句）

- **core `db`**
  - `stage_restore(db, backup)`：校验备份（`integrity_check` + schema 版本不高于当前）→ 落成 `restore-pending.db`，**不改当前库**
  - `apply_pending_restore(db)`：启动早期（**任何连接之前**）——checkpoint 当前 WAL → 自动备份为 `pre-restore-*.db` → 覆盖 → 删 pending
- **IPC**：`stage_restore`
- **GUI**：设置页备份行「恢复」→ 弹层要求输入「恢复」（未输入时按钮 **disabled**）

**关键设计决定**（报告 `evidence/restore-20261113.md`）：
不用"进程内逐表替换"（外键/FTS 触发器/AUTOINCREMENT + writer 线程并发），
也不用"直接覆盖文件"（Windows 下被占用）；改为**待恢复 + 重启生效**——启动时还没有连接，文件替换最可靠。

**验证**：`restore_tests` 2 passed（往返 4→3 条 + 保底 4 条 + 损坏备份被拒且不留 pending）；
`cargo test --workspace` → **125 passed**；`clippy` **0 warning**；
真机：`M5-02-restore-confirm.png`（按钮灰）→ `M5-02-restore-typed.png`（输入后变亮）

## [0.68.0-audit] - 2026-11-13

### 全项目回归 + 文档一致性 + 代码质量审计

报告：`evidence/audit-20261113-full/README.md`

- **静态检查清零**：`cargo clippy --workspace --all-targets` **0 warning / 0 error**（修掉 12 处）；
  生产路径 `-W clippy::unwrap_used -W clippy::expect_used` **0 命中**
- **修复 4 项**：
  1. **P1** `docs/04 §4.1` 的 entity-fact 门禁**未落地且写入时不可执行** → **ADR-043**
     （写入允许；**召回侧**要求有 fact，否则视同 private；`docs/04` 同步修订；已废止清单 + check-docs 词已加）
  2. **P1** core `limit`/`query` 无上限 → `clamp_limit(1..=200)` + `check_query(≤4096B)`，`recall`/`recall_fused` 共用
  3. **P2** `link` 的 `weight` 无范围 → 只允许有限数 `0..=1`（**红绿对照**：实现前测试实测 `Ok(())` 失败）
  4. **P2** 前端搜索 `LIKE` 未转义 `%`/`_` → `ESCAPE '\'` + `escape_like()`
- **记录未修（P3，理由见报告 §3.2）**：scope 非法值靠 DB CHECK 兜底、`format!+VISIBLE` 可 concat、
  `attach::server_spec_for` 参数冗余、批量降权逐条 IPC
- **回归**：`cargo test --workspace` → **116 + 1 + 6 = 123 passed**；`tsc`/`build`/三项门禁全绿；
  **两条端到端冒烟本轮重跑**：`SMOKE: PASS` + `INJECT SMOKE: 11/11 PASS`

## [0.67.0-backup] - 2026-11-13

### T-M5-01 备份与校验（M5 第一项）

- **core**
  - `backup_to(dst)`：**SQLite Online Backup API**（不是文件拷贝——WAL 下拷贝会漏未 checkpoint 的页）
    - 备份后 `wal_checkpoint(TRUNCATE)` + `journal_mode=DELETE` ⇒ **单文件**（不留 `.db-wal/.db-shm`）
  - `verify_backup(path)`：只读打开 → `integrity_check` + `user_version` + 记忆数 + 事件数，**与当前库逐项比对**
  - `list_backups(dir)`：只列 `.db`，最新在前
- **IPC**：`create_backup` / `list_backups` / `verify_backup`
- **GUI**：设置页「备份」区（立即备份 + 每条「校验」+ 列表）

**踩到并修掉的坑**：备份目标复用 WAL 模式会生成 `.db-wal/.db-shm` 伴生文件——
用户只拷 `.db` 时会漏页。备份完成后切 `journal_mode=DELETE`，单测断言伴生文件不存在。

**真机**：点「立即备份」→ **"备份完成且校验一致（50 条记忆 · schema v3）"**；
目录 `%LOCALAPPDATA%\OrbitalPro\backups\` 只有 `orbital-20261113-061540.db`（606208 B）。
截图：`evidence/M5-01-backup.png`

**验证**：`cargo test -p orbital-core backup_tests` → 3 passed
（一致性 / 截断备份红绿对照 / 列表顺序）；`cargo test --workspace` → **118 passed**；
报告 `evidence/backup-20261113.md`

## [0.66.0-retire-page] - 2026-11-13

### T-M4-05 GUI 退役页（M4 里程碑完成）

- **新页面** `src/pages/Retirement.tsx`
  - 候选 / 可恢复 两个 tab + 分页（10 条/页）
  - 逐条降权 + **批量降权**（勾选 + confirm）
  - **5 秒页内撤销条**（批量时逐条 `undo_event`；超时自动消失）
  - 恢复：`demoted` → `restore_to_baseline`；`soft_deleted` → `undo_event`
- **`App.tsx`**：`PageKey` 加 `"retire"`、NAV 加「退役」、渲染新页（维护页前 8 条保留作快速入口）

**真机证据（fixture 造 200 天前）**
```
退役页：50 条候选 · 0 条可恢复 · 分页 1/5
点第一条「降权」→ 顶部「撤销降权 1 条」+ 副标题变「1 条可恢复」+ 该行 ei 0.004→0.002、重要性 3→2
查库：importance (2,1),(3,49)、retire 事件 {"from":3,"to":2}、有效行数仍 50
5 秒后再找「撤销降权」→ 已消失（超时机制生效）
```
截图：`evidence/M4-05-retire-page.png`、`evidence/M4-05-after-retire-undo-bar.png`

**验证**：`tsc` / `npm run build` / `tauri build` 均过；`cargo test --workspace` → **115 passed**；
报告 `evidence/retire-page-20261113.md`。**M4 里程碑 5/5 完成**

## [0.65.0-maint] - 2026-11-13

### T-M4-04 维护任务（孤儿边清理 + 向量补算）

- **core**
  - `cleanup_orphan_edges()`：只删"指向**不存在**记忆"的边
    - **软删记忆的边保留**（恢复后还要用）——这是"孤儿"与"软删"的关键区别
    - 写 `memory_events('maintenance:edges', {before, after, removed})`
  - `reembed_pending(limit)`：只处理 `embedding_status in (pending, failed)`；
    无端点时回 `degraded: ["no-embedding-endpoint"]`（**不假装成功**）
- **IPC**：`cleanup_orphan_edges` / `reembed_pending`
- **GUI**：维护页「维护任务」区块两个按钮 + 结果数字

**一个真实发现**：`edges` 有外键（→ `memories.id`），**正常操作下孤儿边根本插不进去**
⇒ cleanup 是**防御性维护**（对付历史数据/外部 SQL）；单测必须临时 `PRAGMA foreign_keys=OFF` 才能造出孤儿。

**真机**：点「清理孤儿边」→ "没有孤儿边（现有 0 条都指向真实记忆）"
截图：`evidence/M4-04-maint-tasks.png`、`evidence/M4-04-cleanup-result.png`

**验证**：`cargo test -p orbital-core maintenance_tests` → 2 passed；
`cargo test --workspace` → 108 + 1 + 6 = **115 passed**；报告 `evidence/maintenance-20261113.md`

## [0.64.0-recovery] - 2026-11-13

### T-M4-03 恢复入口（events 链回放）

- **core**
  - `restore_to_baseline(va, id)`：取该记忆**最早一条** `retire` 的 `from` 写回
    （不是"当前+1"——连降两次 3→2→1 时，当前+1 只回到 2）
  - 拒绝条件：无降权记录 / 当前已不低于最初值（不误操作）
  - `recovery_list(limit)`：两类可恢复——`demoted`（当前 < 最初）+ `soft_deleted`（带 `undo_event_id`）
  - 本地特权版 `restore_to_baseline_local_owner`
- **IPC**：`recovery_list` / `restore_to_baseline_memory`
- **GUI**：维护页「可恢复」区块（降权行「恢复」、软删行「撤销删除」）

**真机证据（fixture：一条降权 + 一条软删）**
```
可恢复 2 条：
  pi attach ve…  已删除（软删，可撤销）  [撤销删除]
  hermes att…    已降权：当前 2，最初 3   [恢复]
点「恢复」后：
  该条 importance 2 → 3
  restore_baseline 事件 {"from":2,"to":3}
  可恢复剩 1 条（只剩软删）
```
截图：`evidence/M4-03-recovery-list.png`、`evidence/M4-03-after-restore.png`

**验证**：`cargo test -p orbital-core recovery_tests` → 3 passed；
`cargo test --workspace` → 106 + 1 + 6 = **113 passed**；报告 `evidence/recovery-20261113.md`

## [0.63.0-retire2] - 2026-11-13

### T-M4-02 可逆降权（降权 ≠ 删除）

- **core**
  - `retire(va, id)`：`importance - 1`（最低 1 拒绝）+ `memory_events('retire', {from,to})` + 返回 `event_id`
  - `restore_retire(va, id)`：把最近一次 `retire` 的 `from` 写回
  - `undo_event` 新增 `retire` 逆操作分支（撤销降权）；活动日志 `undoable` 加 `retire`
  - 本地特权版 `retire_local_owner` / `restore_retire_local_owner`（GUI，ADR-034）
- **IPC**：`retire_memory` / `restore_retire_memory`
- **GUI**：维护页候选行「降权」按钮；`OP_LABEL` 加「降权 / 恢复降权」

**真机证据（fixture 造 200 天前）**
```
点「降权」前：importance 分布 [(3, 50)]，行数 50
点「降权」后：importance 分布 [(2, 1), (3, 49)]，行数 50
retire 事件：{"from":3,"to":2}
```
截图：`evidence/M4-02-candidates.png`、`evidence/M4-02-after-retire.png`

**踩过的坑**：`conn.execute()` 返回**受影响行数**不是 rowid —— 最初拿它当 `event_id`，
`undo_event` 指向了别的（write）事件，单测当场报"写入没有逆操作"。改用 `conn.last_insert_rowid()`。

**验证**：`cargo test --workspace` → 103 + 1 + 6 = **110 passed**；报告 `evidence/retire-20261113.md`

## [0.62.0-retire] - 2026-11-13

### T-M4-01 退役候选（ei 公式落地）

- **core**
  - `effective_importance()`：**纯函数**，严格按 `docs/04 §6.1`
    （base / max(1,ln(1+acc)) / 半衰期衰减 / 边数加成 / origin 权重 / pinned 免疫）
  - `retirement_candidates(va,…)` + `retirement_candidates_all(…)`（GUI 本地特权版）
    - **只读**：不写库、不降权、不删除
    - 免疫：`pinned` / `importance≥4` / `access_count≥3` 永不进候选
    - 24h 规则：创建不足 24 小时不进候选
    - 每条带 `reasons`（低 ei / 长期未访问 / agent 派生）
- **example** `retirement-report.rs`（只读跑分布）
- **GUI**：维护页「退役候选（ei 低于阈值）」卡片

**数据（如实）**
| 场景 | 结果 |
|---|---|
| 真实库原样 | **total 50 / candidates 0** —— 50 条都是今天导入的，24h 规则生效（不是机制坏了） |
| 副本造 200 天前 | **47/50 候选**，ei≈0.004，原因含「200 天未访问」「agent 派生」 |

**验证**
- `cargo test -p orbital-core retirement_tests` → 2 passed（公式 7 项 + 免疫/24h/只读）
- `cargo test --workspace` → 101 + 1 + 6 = **108 passed**
- `tsc` / `npm run build` / `tauri build` 均过；真机截图 `evidence/M4-01-retirement-page.png`
- 报告：`evidence/retirement-20261113.md`

## [0.61.0-inject] - 2026-11-13

### T-M3-06 proxy 注入实现（M3 里程碑完成）

- **ADR-041**：proxy 通过 core 的**本地只读 HTTP 端点**召回
  - 被否：proxy 直读库（绕过作用域门控）、proxy 复用 MemoryService（会启动 writer/迁移，违反 ADR-008）、MCP 子进程（生命周期复杂）
- **core `local_api.rs`**：`spawn_recall_server`，**只绑 127.0.0.1**、只有 `GET /recall`、复用 `MemoryService::recall` 全部门控
  - src-tauri 启动时 spawn（`127.0.0.1:8791`）；地址非回环直接拒绝（单测）
- **proxy 注入**（`crates/orbital-proxy/src/main.rs`）
  - `ORBITAL_INJECT=1` 才开；只对 `POST */chat/completions`
  - 取最后一条 user 消息 → 调端点（**300ms 超时**）→ 按 ~1024 token 预算拼 system 段 → 转发
  - 失败/超时一律跳过注入、原请求照发（红线 7）；`X-Orbital-Inject: on(N)/skipped(原因)/off`
  - 统计写 `%LOCALAPPDATA%\OrbitalPro\metrics\injection-YYYY-MM-DD.jsonl`（**不写库**，ADR-008）
- **ADR-042**（冒烟发现的真实缺口）：proxy 无法确认请求来自哪个 agent ⇒ **只注入 `scope='shared'` 的记忆**，private 绝不进注入

**验证**
- `scripts/smoke-inject.py`（假上游 + 真 core 端点 + 真 proxy）→ **INJECT SMOKE: 11/11 PASS**
  - 硬证据：假上游**实际收到的 body** 含"长期记忆"段（不是只看响应头）
  - 统计 `status=on, hits>0`；core 不可达 → `skipped(core-error:…)` + 上游仍收到原请求
- `cargo test --workspace` → 99 + 1 + 6 = **106 passed**
- 未做（如实标注）：注入缓存 P95 < 10ms 的对照——proxy 目前无会话缓存，`docs/05` M3 该条**不勾**

## [0.60.0-graphfix] - 2026-11-13

### T-M3-05 图扩展通道改造（ADR-040 + 补充：真平局仲裁）

- **按 ADR-040**：图扩展不再产出独立 hits，只对**已在候选集里的 id** 生效，**不引入新 id**
  - 新增 `graph_bonus()`（种子 = FTS top10；邻居须 JOIN memories 过门控；只处理候选集内的 id）
  - `RecallOptions` 加 `graph_decay`（默认 0.5）；`FusedHit.channels` 标 `graph_linked`
- **实施时再收紧**：实测"加成"本身也会伤 hit@1（RRF rank0/rank1 只差 0.0005，任何加法都能反超）
  ⇒ 最终改为**真平局仲裁**：图连接**不改分数**，只在分数完全相同时决胜
  - 数学上保证 hit@1 不被改变
  - ADR-040 追加"补充"段，写明为什么"继承分"也不够、以及 RRF 下图的真实局限

**对照（fixture 96 条边，同一 50 条查询集）**
| 配置 | hit@1 | hit@5 | MRR |
|---|---|---|---|
| `fts` 基线 | 46/50 | 47/50 | 0.9267 |
| **`fts,graph` (2,1)** | **46/50** | **47/50** | **0.9267**（改造前 7/50） |
| `fts,vec,graph` (2,1,0.5) | 46/50 | 47/50 | 0.9300 |

**验证**：`cargo test --workspace` → **100 passed**；`fused_recall_tests` 4 passed（含新 `graph_bonus_only_boosts_existing_candidates`）

## [0.59.0-graph] - 2026-11-13

### T-M3-04 图关系自动生成 + 图扩展通道缺陷定位（ADR-040）

- **自动建边**（`crates/orbital-core/src/writer.rs`，写入事务内）
  - `temporal`：同 agent + 同 `session_id` 的相邻两条（上一条 → 新一条），权重 `1/(1+小时差)`
  - `entity`：写入 fact 后，共享同一 `(entity_type, entity_id)` 的**同 agent** 记忆互连，权重 0.9
  - `INSERT OR IGNORE` 幂等、`metadata.derived=true`（回放按规则重建）、每批上限 50
- **测试**：`graph_auto_tests` 5 passed（同 session 建边 / 跨 session 不建 / 共享实体建边 / 跨 agent 不建 / duplicate 不建）
- **fixture 边脚本**：`scripts/recall-baseline/seed-edges.py`（确定性，只写副本）

**对照暴露的严重问题（如实记录）**
| 配置 | hit@1 | hit@5 | MRR |
|---|---|---|---|
| `fts` 基线 | 46/50 (92%) | 47/50 | 0.9267 |
| `fts,graph` 等权 | **7/50 (14%) ⬇⬇** | 44/50 | 0.4940 |
| `fts,graph` (2,1) | 40/50 (80%) ⬇ | 44/50 | 0.8307 |
| `fts,vec,graph` (2,1,0.5) | 44/50 (88%) ⬇ | 47/50 | 0.9007 |

**根因（调试证据）**：融合 top5 全是 `channels=["fts","graph"]`（≈0.044），只被 FTS 命中的正确答案仅 0.0328。
① 图通道把"邻居"当独立排名（邻居相关性来自种子，与查询无关）；
② 无关邻居与 FTS 弱命中双通道叠加，分数反超正确答案。

**ADR-040**：图扩展改为"继承种子分数、只对已有候选加成、不引入新 id"；改造列为 **T-M3-05**。
报告：`evidence/graph-auto-20261113.md`

## [0.58.0-facts] - 2026-11-13

### T-M3-03 结构化属性通道（偿还 ADR-034 的 writer 债务）

- **writer**（`crates/orbital-core/src/writer.rs`）
  - `Job` 从"只支持 `WriteInput`"扩成 `JobKind::{Memory(WriteInput), Fact(FactWrite)}`
    —— 这正是 ADR-034 记录的债务：批处理语义按 Job 类型分流，**先 COMMIT 再回执**不变
  - `Writer::write_fact()` + `process_fact()`（事务内）：记忆存在性兜底 → 同值去重 → 插入 →
    **写入时增量冲突检测**（同实体同属性已有不同值 → `INSERT OR IGNORE memory_conflicts`，a/b 按 id 排序）
  - journal 按类型写两种行（`op: write` / `op: fact`）
- **service**
  - `add_fact()`：先 `get()` 做可见性门控（不能给别人的 private 加事实），再走队列
  - `FactFilter{entity_id, property}` + `RecallOptions.fact_filter`；FTS/向量两路都加子查询过滤
- **MCP**
  - 新增 `memory_fact_add` 工具；`memory_recall` 加 `fact_entity_id` / `fact_property`
  - 冒烟脚本从 9 工具扩到 10 工具，并加 3 步断言

**验证**
- `cargo test -p orbital-core fact_tests` → **5 passed**（写入+过滤 / 增量冲突 / 去重 / 可见性 / 混合批）
- `cargo test --workspace` → 91 + 1 + 3 = **95 passed / 0 failed**
- `scripts/smoke-mcp.py`（真实进程 stdio）→ **SMOKE: PASS**（16 项全 PASS）
- 基线未回归：`fts` 46/47/0.9267；`fts,vec (2,1)` 46/47/0.9300

## [0.57.0-fused] - 2026-11-13

### T-M3-02 三路召回 + RRF（含如实记录的负面结果）

- **core**（`crates/orbital-core/src/service.rs`）
  - `RecallChannel`（Fts / Vector / Graph）+ `RecallOptions{channels, weights, k=60}`
  - `recall_fused()`：RRF `score = Σ w_ch/(k+rank+1)`，同一 id 多路命中只累加一次并记录通道
  - `FusedRecall{ hits, degraded }`：**降级必须可见**（如无嵌入端点时 `vector: no-endpoint`）
  - FTS 抽成 `fts_hits()`（recall 与融合共用，不含打点）；向量通道**先门控后算余弦**（ADR-019）
  - 图扩展：FTS 种子节点的 1 跳邻居（JOIN memories 再过 VISIBLE 门控）
- **example** 支持 `channels` + `weights` 参数（四组对照 + 权重实验）

**四组对照（同一 50 条查询集，副本库）**
| 配置 | hit@1 | hit@5 | MRR |
|---|---|---|---|
| `fts`（基线） | 46/50 (92.0%) | 47/50 (94.0%) | 0.9267 |
| `fts,vec` 等权 (1,1) | **43/50 (86.0%) ⬇** | 47/50 | 0.9000 ⬇ |
| `fts,graph` (1,1) | 46/50 | 47/50 | 0.9267（当前 `edges=0`） |
| `fts,vec,graph` (1,1,1) | 43/50 ⬇ | 47/50 | 0.9000 ⬇ |
| **`fts,vec` (2,1)** | **46/50** | 47/50 | **0.9300 ⬆** |

**机制**：等权时两路第 1 名同分 `1/61`，向量会把 FTS 的第一名挤到第 2（实测掉 3 条）；
加权 (2,1) 后 FTS 第 1 名 `2/61` 重新锁定，MRR 微升。`(3,1)`/`(1,0.5)` 同结果 = 结论稳定。

**如实记录**：①等权融合降 3 条，未藏；②3 条同构句 miss **向量也没救回来**（hit@5 仍 94%），
只能靠结构化属性区分 → 交给 T-M3-03。

**验证**：`cargo test -p orbital-core fused_recall_tests` → 4 passed
（单路一致 / 降级可见 / 图扩展带邻居 / 门控不破）；报告 `evidence/recall-fused-20261113.md`

## [0.56.0-baseline] - 2026-11-13

### T-M3-01 召回基线建立（M3 的第一个可测量数字）

- **查询集**：`scripts/recall-baseline/gen-queries.py` 从真实库 50 条记忆生成 50 条查询
  - 英文/数字词优先 + 中文 **3-gram**（trigram FTS 需要 ≥3 字符，2 字查询不可用）
  - 跳过虚词片段；**刻意不复制整句**——测关键词召回能力，不是测逐字复制
  - 每条带 `expected_id`（标注客观）
- **评分器**：`crates/orbital-core/examples/recall-baseline.rs`
  - 调 **core 的真实 `recall()`**（不是用 Python 复写 SQL，避免掩盖实现差异）
  - 输出 hit@1 / hit@5 / MRR；只读副本库（`VerifiedAgent::for_local_owner`）
- **基线数字**（副本库跑两次，完全一致）：
  | 指标 | 值 |
  |---|---|
  | hit@1 | **46 / 50 = 92.0%** |
  | hit@5 | **47 / 50 = 94.0%** |
  | MRR | **0.9267** |
  | miss | 3（q48/q49/q50，全是 `<agent> attach verified pid <N>` 同构句） |
- **miss 根因**：同构句式让 trigram+bm25 区分度不足，expected 那条被同类记录挤掉——
  即**单路关键词召回的真实短板**，T-M3-02（三路召回 + RRF）的对照对象
- 报告：`evidence/recall-baseline-20261113.md`（复现命令 / 查询构造规则 / miss 分析 / 局限）

## [0.55.0-gate] - 2026-11-13

### T-M2-08 M2 门禁走查完成（M2 里程碑通过，进入 M3）

- **走查**：8 个 JTBD 逐条真机验证 → `evidence/M2-gate-walkthrough.md`
  - 可独立完成 **4 条**：J1（总览）/ J2（编辑删除撤销）/ J3（合并+维护）/ **J4（建关系，本轮补齐）**
  - J8 部分（维护页待办可处理 + 进程健康）；J5 属 M4、J6 属 M3（**proxy 注入未实现**）、J7 属 M6
- **补 J4**（GUI 建关系）：
  - core `link_local_owner`（本地特权，身份从库查）+ `link` 增加**自环拒绝**
  - src-tauri `link_memory` / `related_memories`
  - 前端详情页「建立关联」弹窗（搜索目标 + 选关系类型）+「关联（N）」列表（点击跳转）
- **ADR-039 修正两处设计错误**
  1. M2 门禁"8 个 JTBD 里 6 个可操作"**不可达**（J4/J5/J6/J7 都不在 M2 任务清单）→ 改为 5 条口径，可操作率 ≥7 移到 M6
  2. "界面与 `ui-hifi-v2.html` 1:1"在 ADR-035 后过期 → 基准改为 ADR-032/035 token 表
  - 同步修正 `docs/00/01/05` 表述（避免三份文档三种口径）
- **J6 的真相**：`orbital-proxy` 的 `inject_enabled: false`，`/health` 返回 `skipped(not_implemented)`——
  注入统计不是"加个页面"，是 M3 的第一块硬骨头（ADR-039 决策 3 锁定前提）

**验证**
- `cargo test --workspace` → **86 passed / 0 failed**（82 core + 1 drill + 3 proxy）
- 新增 `link_local_owner_creates_edge_and_rejects_bad_input`
- 真机（fixture 副本库）：建立关联弹窗 → 选目标 + semantic → 「建立关系」→ 反馈"已建立 semantic 关系"；
  库内 `edges=1`；真实库仍 50 条 / 0 edges
- 截图：`evidence/M2-08-j1-overview.png` / `-j4-link-dialog.png` / `-j4-linked.png` / `-j8-maintain.png`

## [0.54.0-undo] - 2026-11-13

### T-M2-07 撤销 / 回滚完成（M2 任务 7/7）

- **core**（`crates/orbital-core/src/service.rs`）
  - `undo_event(event_id)`：按 `operation` 分派逆操作——
    `update` 写回 `detail.before` 的 importance/tags/pinned；`supersede` 新条软删 + 旧条恢复有效 + 删 supersedes 边；
    `forget:soft` 清 `deleted_at`；`forget:invalidate` 清 `valid_to`；`merge` 委托 `undo_merge`；
    **`forget:hard` 明确拒绝**
  - supersede 撤销带安全前提：新版本必须仍有效且未被再次取代（否则会制造第三个版本）
  - `list_activity(limit)`：活动日志，`undoable` / `undone` 由服务端判定
  - `UpdateOutcome.event_id` + `forget` 返回 event_id（撤销句柄）
- **src-tauri**：`undo_event` / `list_activity`；`forget_memory` / `invalidate_memory` / `toggle_pin` 返回 event_id
- **前端**：记忆页操作成功后出现「撤销X（5 秒内）」，5 秒自动消失；维护页新增「最近操作」区块可撤销历史操作，硬删标"不可撤销"

**验证**
- `cargo test --workspace` → **85 passed / 0 failed**（81 core + 1 drill + 3 proxy）
- 新增 7 条测试：元数据撤销 / supersede 撤销 / 软删撤销 / 失效撤销 / 硬删拒绝 / 撤销幂等+留痕 / 活动日志判定
- 真机（fixture 副本库）：Pin → 出现「撤销Pin（5 秒内）」→ 5 秒后消失 → 维护页活动日志点撤销 →
  「已撤销编辑：恢复了重要度/标签/Pin」→ 库内 `pinned=0`、events 51(update)/52(undo, undo_of=51)；
  硬删事件显示「彻底删除 … 不可撤销」无按钮
  截图：`evidence/M2-07-undo-5s.png` / `-activity-undo.png` / `-hard-not-undoable.png`

## [0.53.0-maintain] - 2026-11-13

### T-M2-06 维护页待办全覆盖（五类待办可处理 + 冲突收件箱）

- **schema v3**（`crates/orbital-core/src/schema.rs` + `db.rs`）
  - 新增 `memory_conflicts` 表（docs/11 P1-4 定稿：**表不是视图**，要能存裁决结果）
  - `SCHEMA_VERSION` 2 → 3；迁移改**逐步**（v1→v2→v3 每步幂等），线上 v2 库可单独迁
- **core**（`service.rs`）
  - `detect_conflicts`：同 `(entity_type, entity_id, property)` 多有效值且值不同 → 两两登记（幂等）
  - `resolve_conflict`：`keep_a`/`keep_b`（失效另一侧 fact）/ `keep_both`（ignored）/ `merged`
  - **`recall` 排除 open 冲突条目**（`NOT IN (memory_a, memory_b)`，docs/04 §7「冲突条目不参与注入」）
- **src-tauri**（`data.rs`）
  - `maintenance_todos`（五类待办 + 重复组 + 冲突详情）/ `detect_conflicts` / `resolve_conflict`
  - 启动时 `ensure_migrated()`：只读页面不会触发懒初始化，不迁一次维护页会一直显示"表尚未迁移"
- **前端**（`App.tsx` 维护页重写 + `styles.css`）
  - 五类待办 → 就地展开处理（重复组三选一 / 冲突四选一 / 其余跳记忆页带筛选）
  - 侧栏角标改用 `todo_total`（待办**类别数**），与页面「待处理 N 项」同源

**修的两个口径错误（真问题）**
1. 重复组原按 `GROUP BY agent_id, content_hash` 统计，但唯一索引 `ux_mem_agent_content` 保证同 agent 同 hash 只有一条有效 ⇒ **该指标恒为 0**（假指标）。改为跨 agent + 排除版本链旧条（`valid_to IS NULL`）。
2. 侧栏角标用 `embeddings_pending`（单一类别条数），与页面「待处理 N 项」必然不同源（旧坑 #8）。

**验证**
- `cargo test --workspace` → **78 passed / 0 failed**（74 core + 1 drill + 3 proxy）
- 新增 6 条测试：冲突检测/同值非冲突/注入排除对抗测试/ignored 不复活 + 2 条 v3 迁移
- `npx tsc --noEmit` / `npx tauri build --debug --no-bundle` 全过
- 真机（fixture 副本库）：v2 库 → 维护页 4 项 + 如实提示"冲突表尚未迁移" → 扫描 → 5 项 →
  冲突四选一「留 A」→ 4 项 + 角标同步 → 库内 `resolved/keep_a`、B 侧 fact 失效
  截图：`evidence/M2-06-todos-v2.png` / `-conflict-detected.png` / `-conflict-compare.png` /
  `-conflict-resolved.png` / `-duplicate-groups.png` / `-drilldown.png`
- 真实库自动迁 v3（user_version=3），50 条记忆 / 0 标签 / 0 facts / 0 edges 未变

## [0.52.0-merge] - 2026-11-13

### T-M2-05 合并重复完成（搬标签 / 搬关系 / 搬事实 + 可撤销）

- **core**（`crates/orbital-core/src/service.rs`）
  - `merge_preview(keep, drop)`：预览搬运量，与真正合并**同一套查询**
  - `merge(va, ..)` / `merge_local_owner(..)`：`BEGIN IMMEDIATE` 事务内
    tags/entities 并集 → facts 改指 → edges 改指（`INSERT OR IGNORE`、自环丢弃）→
    更新 keep → 软删 drop → 写 `memory_events(merge)`，**内容保留 keep 原文不拼接**（ADR-036）
  - `undo_merge(event_id)`：按 `memory_events.detail` 的完整清单逆操作，幂等，写 `undo` 事件
- **顺手修的静默降级 bug**：`snapshot_for_hard_delete` 查 facts 用了不存在的 `key` 列，
  且被 `if let Ok(...)` 吞错 ⇒ 硬删快照的 facts **永远为空数组**。改为真实列名（property 等）+ 错误上抛
- **src-tauri**：新增 `merge_preview` / `merge_memories` / `undo_merge` 三个 IPC
- **前端**：详情按钮行「合并重复」→ 弹窗（候选搜索 300ms 防抖 + 预览 + 三选一）；合并后「撤销合并」
- **UI 缺陷修复**：详情按钮行从 3 个涨到 5 个后固定单行，按钮被压成竖排（width=62/height=129）；
  `.edit-bar` 加 `flex-wrap: wrap`，反馈文案独占一行

**验证**
- `cargo test --workspace` → **72 passed / 0 failed**（68 core + 1 drill + 3 proxy）
- 新增 3 条测试：`merge_moves_tags_edges_and_facts` / `merge_refuses_deleted_drop_and_leaves_keep_untouched` / `merge_can_be_undone`
- `npx tsc --noEmit` / `npm run build` / `npx tauri build --debug --no-bundle` 全过
- 真机（fixture **副本**库，未碰用户真实库）：预览「标签 3（搬来 1）· 关系 1 · 事实 2（1 自环丢弃）」→
  合并 53→52 → 撤销 52→53，库内 tags/facts/edges/events 逐项核对还原
  截图：`evidence/M2-05-merge-preview.png` / `-merge-done.png` / `-merge-undone.png` / `-buttons-fixed.png`

## [0.51.0-overview] - 2026-11-12

### T-M2-01 记忆总览页完成（第一次能看见真实记忆）

- 后端 `src-tauri/src/data.rs`（**只读**）：`list_memories` / `get_memory` / `filter_options`
  - 服务端分页 50/页、四筛选（关键词 + agent + category + 三态 status）
  - 摘要按**字符**截断（不按字节）
- 前端主从双栏（对齐 `docs/03 §4.2`），只用 token
- 分层卡按 **ADR-031** 跨表统计：`L0 = count(conversations)`、`L1 = count(memories)`
  —— **不 `GROUP BY layer`**（该字段已在 schema v2 删除）

**真机证据**
| 态 | 看到什么 |
|---|---|
| 列表 | `L0 0 / L1 50 / L2 0 / L3 0`，50 条真实记忆（workbuddy · 11-12 · imported · imp 3） |
| 详情 | 点第一行 → Hermes 审计全文 + 来源路径 + 双时态 + 向量状态 |

截图：`evidence/M2-01-overview-list.png` / `-detail.png`

### 过程中修的 4 个问题
1. **`design/ui-hifi-v2.html` 不在本项目**（`design/` 空）→ 从旧项目复制
2. `.row` 类名与 M1 状态页冲突 → 列表项改 `.mem-row`
3. **我误覆盖了 `styles.css`**（token 全丢）→ git 恢复后改为追加
4. 详情元数据重叠（`.kv` 4 列 + 无断行）→ `word-break` + 详情区单列

### remaining
1 项：编辑 / Pin / 删除 / 标记失效四个写操作未接（当前只读，页面已如实标注）

## [1.0.0-m1-complete] - 2026-11-12

### 🎉 M1 可运行骨架完成：15/15 验收全部通过

按 `docs/05` 的 15 条逐条**重跑**（11 条本轮重跑 + 4 条引用本会话内真机证据）：

```
cargo test -p orbital-core    -> 49 passed
cargo test -p orbital-proxy   -> 3 passed
scripts/verify.ps1            -> VERIFY: PASS
scripts/smoke-mcp.py          -> 13/13 PASS
scripts/smoke-launcher.ps1    -> 9 项 PASS（含杀 core 自动重启、孤儿接管、退出 0 残留）
verify-remain.py              -> 回查 100/100、headless 无 WebView2、单实例 3 进程
```

**修的一个"假指标"**：`smoke-mcp.py` 断言"tools/list 8 个"，实际 9 个
（T-M1-10 加了 `orbital_health`）→ 修断言后 13/13 PASS。
**教训**：加功能必须同步改测试期望值，否则出现"测试红、产品对"的假信号。

### M1 交付的能力
- 记忆写入（单队列 + 审计 + 去重）
- 身份硬门控（SQL 层 `VISIBLE` + `VerifiedAgent` 类型门控）
- 关键词召回（FTS5 + 特字符转义）
- **向量写入**（双端点一致性 0.9992）
- MCP 9 工具
- **静态索引层**（`--sync` + 写入后自动同步到 WorkBuddy MEMORY.md）
- **文件反向导入**（`--import` / `--maintain`，60 条多 agent 记忆入库）
- **卸载接入**（`--detach`，4 个 agent 都可退）
- 三进程 + 孤儿回收 + 托盘 + 单实例
- 三进程状态页（真实探测）
- proxy 转发（注入未做，M3）

### 遗留的 2 个"名义差异"（不影响验收）
1. exe 名 `orbital-app.exe` vs 文档 `OrbitalPro.exe`（加 `mainBinaryName` 即可）
2. MSI/NSIS 未产出（沙箱拦 WiX，需真实会话终端）

### 新增
`docs/22-M1收尾验收报告.md`（15 条逐条判定表 + 完整输出）

## [0.50.0-installer] - 2026-11-12

### T-M1-14 安装包：release exe 干净目录启动成功（M1 外壳收口）

- 装 `@tauri-apps/cli` 2.12.4
- `NODE_OPTIONS= npx tauri build --no-bundle` → **release 8.6 MB，内嵌前端**
- 干净目录（`%TEMP%\orbital-clean-test`）启动：**不起 Vite** 也能显示状态页

**真机证据**
```
干净目录启动: GUI 42200 + core 47948 + proxy 41492
状态页: 3/3 全部正常（真实 PID）
截图: evidence/M1-14-clean-dir-release.png
同一目录 exe 数量: 1
```

**孤儿回收复验**（验收第 5 条）
```
杀 GUI -> core/proxy 成孤儿（端口仍占）
重启   -> 日志「发现孤儿进程，接管中 pid=47948 role=core」/「pid=41492 role=proxy」
       -> 旧进程安全终止、新进程拉起（45544/42904）
```
符合 `docs/05` 第 5 条"接管**或安全终止**"。

### 诚实标注的两个差异
1. **exe 名是 `orbital-app.exe`**，验收文档写 `OrbitalPro.exe`（`productName` 只影响 bundle 名）。
   要改需在 `tauri.conf.json` 加 `mainBinaryName`——**未做**，不影响"一个 exe + 3 进程"的实质。
2. **未产出 MSI/NSIS**：沙箱拦 WiX（`light.exe`），需真实会话终端。

### remaining
2 项（`mainBinaryName` 改名 / MSI 安装包）

## [0.49.0-status-page] - 2026-11-12

### T-M1-13 三进程状态页完成（真机截图 + 真实数据）

- `src-tauri/src/status.rs`：GUI 用本进程 pid；core/proxy 向 `/health` 发**真实** HTTP 请求
- `App.tsx`：3/3 大数字 + 三张卡片（PID/端口/版本/探测时间）+ 3 秒轮询 + 手动刷新
- `styles.css`：状态页样式，**全用 docs/03 token**
- `tauri.conf.json`：开 `withGlobalTauri`（免装 `@tauri-apps/api`）

**真机证据**
| 时刻 | 看到什么 |
|---|---|
| 首次 | 3/3 全部正常，PID **40840 / 37532 / 40628**，探测 13:15:24 |
| 杀掉 core 后 | launcher 自动重启（新 pid **47284**），状态页 PID 跟着变、时间戳 13:15:42 |

⇒ 数据是**真实探测**，自动刷新有效。

### 过程中修的 4 个真问题
1. **`window-state.json` 持久化 `visible:false`** → 主窗口永不显示（无任何报错）
2. **`cargo build` 的 exe 走 dev 模式** → 连 `localhost:5173` 得 `ERR_CONNECTION_REFUSED`
3. **`5173` 在 Windows 排除范围 `5097-5196`** → `EACCES` bind 被拒 → 换 `14321`
4. **Vite 默认监听 `::1`** → 本机不可用 → 显式 `127.0.0.1`

### 新增
- **ADR-030**：窗口可见性不交给 window-state；dev 端口避开排除范围；交付必须 `tauri build`
- 截图证据：`evidence/M1-13-status-page.png` / `M1-13-status-after-restart.png`

### remaining
1 项：`tauri build` 产内嵌前端的正式 exe（属 T-M1-14）

## [0.48.0-finish-rule] - 2026-11-12

### 强化「任务收尾三件事」——用户明确要求"每次任务完成后都要核实设计、确认正确、修订计划"

**落了两处**：
1. **热记忆**（`~/.mnemon/runtime/MEMORY.md`，importance=critical）：
   写入可执行的三步（核实设计 → 修正设计 → 修订计划），并明确"证据必须本轮新跑的"；
2. **`AGENTS.md` 红线 12**：从只要求"设计对比验证"扩展为**三件事缺一不可**：
   ```
   ① 核实设计：设计约束逐条对代码，产出对比表（约束 / 代码位置 / 判定）
   ② 修正设计：设计本身错就写 ADR（含被否方案 + 不许回退的理由）
   ③ 修订计划：更新 PLAN 的当前任务/上一任务/测试数 + 任务卡执行记录
   ```

### 为什么原来不够
红线 12 原文只要求"对比表"，**没要求更新计划**——
而 `PLAN.md` 的实际状态曾落后 6 个任务（当前任务停在早已完成的 T-M1-06），
正是"只做对比、不修订计划"的直接后果。

### 本轮按新规则执行的结果
- 核实设计：本轮改的是规则文件，已对照 `AGENTS.md §1/§7` 与 `docs/05` 检查无冲突；
- 修正设计：把"计划更新"从 §7 的交接提示**提升为红线**（违反即返工）；
- 修订计划：`PLAN.md` 当前任务 → 三进程状态页 + 安装包；上一任务 → detach；测试数 → 49。

## [0.47.0-detach] - 2026-11-12

### 实现 detach（卸载接入）+ 澄清 detach 与 restore 的区别

```
orbital-mcp --detach <agent>
```

**两个动作，语义不同**（ADR-029）：

| 操作 | 做什么 | sha256 | 丢用户其它修改吗 |
|---|---|---|:--:|
| **detach** | 只删 orbital 条目 | 可能变（重新格式化） | ❌ 不丢 |
| **restore** | 用 `.bak.orbital` 整体覆盖 | **必然还原** | ⚠️ 会丢 |

**默认用 detach**（更安全）。这也是对 `docs/05` 第 9 条口径的修正——
原写"detach 后 sha256 还原"是把两件事混在一起了。

### 真机验证（WorkBuddy，完整红绿对照）
```
detach 前: 有 orbital = True
DETACH OK: changed=true removed=1 sha256=d8e397af...
detach 后: 有 orbital = False，JSON 合法
恢复后:   有 orbital = True，sha256 = b840dc32...（与 detach 前完全一致）
```

### 修的两个真 bug
1. **detach 用错 server key**：原来删 `spec.id`（agent 名），
   实际写入的 key 是 `orbital`。加 `SERVER_KEY` 常量修正。
2. **YAML detach 匹配不到**：DSH 的实际格式是列表项 `- id: mcp-orbital`，
   首版只匹配 `id: xxx` → 删不掉。改为同时匹配两种形式。

### 新增单测（3 个）
- `detach_json_restores_original_sha256`（删条目 + JSON 合法）
- `detach_without_entry_is_noop`（无条目时不写文件）
- `detach_yaml_removes_only_our_block`（注释与其它条目保留）

测试 46 → **49 passed**

## [0.46.0-maintain] - 2026-11-12

### 一键全量维护 `--maintain`：60 条多 agent 记忆入库

```
orbital-mcp --maintain [--dry]
```
① 对 dsh/pi/hermes/workbuddy 各跑一次 import ② 更新静态索引。

**真实验证**：
```
dry-run:  dsh +15  pi +15  hermes +24  workbuddy +6   = 60 条候选
真导入:   新增 39 / 已存在 21
复跑:     新增 0  / 已存在 60        ← 完美幂等
```

**注意**：`dsh` 与 `pi` 共用 `~/.mnemon/runtime/MEMORY.md` →
第一次导入 dsh 后，pi 的 15 条判定"已存在"（内容相同不重复存，**这是正确行为**）。

### 修的两个真问题

**① 解析器只认 `## ` 标题 → 3/4 agent 导入 0 条**
实测本机三种格式：
| agent | 格式 | 分隔 |
|---|---|---|
| WorkBuddy | `## ` 标题 | 7 个 |
| DSH/mnemon | 段落 + `§` | 14 个 |
| Hermes | 段落 + `§` | 9 个 |

修：先按标题切，切不出来**降级按 `§` 切**。

**② 身份门控拦住跨 agent 导入**（`configured=workbuddy claimed=dsh`）
这是 ADR-005 **设计正确**：每个 agent 只能写自己的记忆。
修：导入用**当前 token 的 agent 身份**写入，来源记 `source_ref`。
想让某 agent 导入它自己的文件，必须用**它的** token 跑。

## [0.45.0-import] - 2026-11-12

### 实现文件反向导入（ADR-028 路 B）+ 修 FTS5 转义 bug

**新能力**：`orbital-mcp --import <agent> [--dry]`
读各 agent 自己的记忆文件 → 按 `## ` 标题解析条目 → 写库（幂等）。

支持：`dsh`/`pi`（`~/.mnemon/runtime/MEMORY.md`）、`hermes`（`memories/MEMORY.md`+`USER.md`）、
`workbuddy`（`user-<uid>-personal/MEMORY.md`）。

**三条硬约束都实现**：
1. 幂等——以 `(agent, content_hash)` 去重
2. 保来源——`source_ref = "import:<agent>:<file>"`
3. 只增不改——只读文件，绝不写 agent 自己的记忆文件
4. **额外**：跳过 `<!-- orbital:begin/end -->` 区块（防止自己写的索引被反向导回，形成回环）

**真实验证（WorkBuddy）**：
```
dry-run: 6 条候选
真导入:  6 条新增 / 0 已存在
复跑:    0 条新增 / 6 已存在   ← 幂等成立
```

### 修的真 bug：FTS5 查询未转义
`safe-delete` 里的 `-` 被 FTS5 当成 NOT 操作符，`delete` 被当成列名：
```
INTERNAL 500: no such column: delete
```
修法 `fts_escape()`：每词加双引号（内部 `"` 翻倍）+ OR 连接。
新增单测 `fts_escape_neutralizes_special_chars`。测试 45 → **46 passed**。

### 诚实记录的局限：中文同义词匹配不到
查"浏览器书签" → 0 命中；而库里有"浏览器**收藏夹**"。
**关键词检索天生对同义词无能** —— 这正是必须有向量召回的原因（M3）。

## [0.44.0-write-symmetry] - 2026-11-12

### 认清并记录：写入只做了一半（ADR-028）

**用户提问**："你现在解决的是读取记忆，那各个 agent 写入记忆是如何解决的？"

**诚实回答**：写入只有一半。

| 方向 | 自动（推送） | 主动（拉取） |
|---|---|---|
| 读 | 静态索引层 ✅ 已实现 | MCP `memory_recall` ✅ |
| **写** | **自动采集 ❌ 未实现** | MCP `memory_write` ✅ 4/4 已实测 |

`memory_write` 工具就绪，但 **agent 不会自觉调**（除非用户说"记住这个"）——
所以写入能力目前形同虚设。

### 自动采集的两条路（ADR-028）
| 路 | 做法 | 优先级 |
|---|---|---|
| **B. 文件反向导入** | 读各 agent 自己的记忆文件（`~/.mnemon/`、`hermes/memories/`、WorkBuddy `MEMORY.md`）→ 解析 → 写库 | **高（先做）** |
| A. hook 事件采集 | `Stop` hook 读 `transcript_path` → 摘要 → 写库 | 中（需各 agent hook） |

**本机素材已就位**（路 B 的输入，合计约 23 KB）：
- `~/.mnemon/runtime/MEMORY.md` 9413 B
- `hermes/memories/MEMORY.md` 3455 B + `USER.md` 2557 B
- WorkBuddy `user-<uid>-personal/MEMORY.md` 8286 B（含 orbital 区块）

**目前一条都没采集进 Orbital。**

### 三条硬约束（文件导入）
1. **幂等**：以「来源文件 + 内容 hash」去重
2. **保来源**：`source_ref = "import:<agent>:<file>"`
3. **只增不改**：只 append，绝不改/删 agent 自己的文件

## [0.43.0-auto-sync] - 2026-11-12

### 自动 sync：写入记忆后静态索引自动更新（闭环成立）

`memory_write` 成功后**异步** `spawn` 一次 sync（等 300ms 让事务落盘）：
- **不阻塞写入回执**
- **失败不影响写入**（索引只是辅助层）
- 成功写 `tracing::info!`，失败 `tracing::warn!`（可见）

### 真实验证（本轮实跑）
```
写入前索引条数: 1
写入 id: mem_b20dfcbf...  duplicate: False
写入后索引条数: 2          ← 自动增加
PASS 索引条数增加
PASS 新记忆出现在索引里
PASS 原文仍保留
```

**当前区块内容**：
```markdown
<!-- orbital:begin (由 Orbital Pro 自动维护，勿手工编辑) -->
## Orbital 记忆索引（自动生成 · unix:1794453431）
跨 agent 记忆库在 Orbital。需要细节时用 MCP 工具 `memory_recall(query)` 或 `memory_get(id)`。
最近 2 条（agent=workbuddy）：
1. 自动同步验证 AUTO-SYNC-TEST-1794453428 — workbuddy
2. workbuddy attach verified pid 5564 — workbuddy
<!-- orbital:end -->
```

### 完整闭环
```
写入记忆 → SQLite（含向量）→ 自动 sync → MEMORY.md 索引
                                              ↓
                              WorkBuddy 每次会话自动读
                                              ↓
                        agent 看到索引 → 调 MCP recall 取细节
```

### 已知待改进
- 索引里的时间戳是 `unix:1794453431`（不友好），应格式化为本地时间
- 每次写入都 sync（无节流），高频写入时会有重复写

## [0.42.0-sync] - 2026-11-12

### 实现 `orbital-mcp --sync`：静态索引层生成器（ADR-027 落地）

**新能力**：
```
orbital-mcp --sync <agent_id> [--limit N]
```
读库最近 N 条 → 生成「索引 + 调用指引」→ **原子写入**（temp + rename）
`~/.workbuddy/user-<uid>-personal/MEMORY.md` 的受控区块（`<!-- orbital:begin/end -->`）。

**核心保证**：
- **只放索引**（标题 ≤40 字 + agent + 时间），不放全文
- **受控区块**：只替换 `orbital:begin/end` 之间，**绝不碰用户自己的记忆**
- **原子写**：同目录 temp → rename

**真实验证**（本轮实跑）：
```
目标: ~/.workbuddy/user-<uid>-personal/MEMORY.md
同步前 7837 B，无区块
SYNC OK: 1 条索引
同步后 8209 B，有 orbital 区块
原文标记「# 用户级长期记忆（跨项目）」保留 ✅
```

**过程中修的真 bug**：
`updated_at` 在库里是 **REAL**（带小数的 unix 秒），我按 `i64` 取 → 运行时
`Invalid column type Real`。改为 `f64` 后通过。**这类错误编译期发现不了**。

### 新增单测
`summarize_respects_char_boundary`：按**字符**（非字节）截断，中文/emoji/换行全覆盖。

## [0.41.0-arch-clarified] - 2026-11-12

### 接入架构定稿（ADR-027）：MCP 是主干，hook/扩展/文件是三种注入适配器

**不是"mcp + hook + MEMORY.md"三者并列**，而是：

| 能力 | 手段 | 覆盖 |
|---|---|---|
| 存取（pull） | **MCP**（9 工具） | 4/4 已实测 |
| 自动注入（push） | hook / 扩展 / 文件（按宿主择一） | 4/4 已找到路径 |
| 观察全流量（可选） | proxy | 默认关闭 |

| agent | 注入适配 |
|---|---|
| DSH | hook（`dsh-hooks-claude-code`） |
| Hermes | hook（`hooks_auto_accept`） |
| pi | 扩展（`before_agent_start` 返回 `{message}`） |
| WorkBuddy | **文件注入**（`user-<uid>-personal/MEMORY.md`） |

### 关键澄清：注入分两层，MEMORY.md 注入的不是"对应记忆"

| 层 | 机制 | 解决什么 | 局限 |
|---|---|---|---|
| 静态索引层 | hook `additionalContext` / 扩展 `{message}` / MEMORY.md 区块 | 让 agent **知道有记忆** + 看到最近几条 | **不知道下次问什么** |
| 动态检索层 | MCP `memory_recall(query)` | 按**当前问题**取**对应**记忆 | 需 agent 主动调用 |

**"通过 MEMORY.md 注入对应记忆"这个问法需纠正**：
MEMORY.md 注入的是**索引 + 调用指引**；"对应"是 agent 在对话中调 `memory_recall` 时才算出来的。

**这不是妥协**：①上下文预算有限，全文会挤爆；②会话前不可能预判问题，猜必错；
③注入索引让模型自己决定要不要查，比硬塞省 token 且更准（与 claude-mem 结论一致）。

### 静态索引层三条硬约束
1. 只放索引（标题 + ≤20 字摘要），不放正文
2. `<!-- orbital:begin/end -->` 受控区块，只替换这段，**不碰用户记忆**
3. 原子写（temp + rename）

## [0.40.0-workbuddy-verified] - 2026-11-12

### ✅ WorkBuddy 注入路径验证成功：`~/.workbuddy/user-<uid>-personal/MEMORY.md`

**WorkBuddy 原话**：
> "测试码是 ORANGE-42。（这是你**个人空间 MEMORY.md** 里 'Orbital Pro 注入测试' 条目记的码——能读到就说明注入生效了。）"

**它自己指出了正确路径** —— `~/.workbuddy/user-<uid>-personal/MEMORY.md`。

### 第一次为什么失败（我的两个错误）
1. **选错文件**：写了根目录 `MEMORY.md`（6407 B, 9/16 旧版），
   而 `user-<uid>-personal/MEMORY.md`（7837 B, 10/8）才是新版（多一段"pi 两条铁律"）
2. **写错位置**：放在 6407 B 文件末尾（L64），若只读前 N 字节则看不到

### 最终：4 agent 自动注入全覆盖（且都不是 proxy）
| agent | 注入方式 | 路径 |
|---|---|---|
| DSH | hook | `cordis.patch.yml` 挂 `dsh-hooks-claude-code` |
| Hermes | hook | `config.yaml` 的 `hooks_auto_accept` |
| pi | 扩展 | `~/.pi/agent/extensions/*.ts` |
| **WorkBuddy** | **MEMORY.md 注入** | **`~/.workbuddy/user-<uid>-personal/MEMORY.md`** |

**WorkBuddy 不需要 hook、不改 models.json、不影响官方积分。**

### 环境已还原
个人空间 7837 B（SHA256 `2A1352F81B50A085`）、根目录 6407 B（`003B87A5BA5D1327`），
ORANGE-42 已清除，`.bak.orbital-test` 备份保留。

### 新增
`docs/20` 追加 §6 验证成功 + §7 正式方案（受控区块 + 索引 + MCP 取细节）

## [0.39.0-workbuddy-memory] - 2026-11-12

### 找到 WorkBuddy 的第三条注入路：MEMORY.md（官方文档确认）

**官方文档原文**（腾讯云开发者社区，WorkBuddy 官方产品文）：
> "助手支持把一份 Markdown 记忆文件在**每次会话开始时自动注入上下文**"

> "每次会话结束后写入记忆文件，**下次启动时自动读取**"

**本机实证**：`~/.workbuddy/MEMORY.md`（6407 B）内容全是 WorkBuddy 自己积累的
跨项目技术记忆（safe-delete、浏览器书签、npm 坑、tauri build 经验）→ 证明该文件确实在用。

**与 hook 的对比**：
| 路径 | 可行性 |
|---|:--:|
| hook（`~/.codebuddy/settings.json`） | ❌ 实测不支持（桌面版不读） |
| **MEMORY.md 注入** | ✅ **官方文档确认** |

**已做测试**：备份（6407 B, SHA256 `003B87A5BA5D1327`）→ 追加 ORANGE-42 → 6633 B
待用户在 WorkBuddy 新开会话验证。

**风险**：官方有"限额、蒸馏与覆盖"治理规则 → **不能塞全文**，只能塞"索引 + 调用指引"。

**方案**（验证成功后）：
在 MEMORY.md 末尾维护一个受控区块（HTML 注释标记边界），只放索引 + "去 Orbital 查"的指引；
agent 看到索引后按需调 MCP `memory_recall` 拿细节——即"索引 + 按需拉取"两层结构。

### 新增
`docs/20-WorkBuddy记忆注入路径.md`

## [0.38.0-hook-tested] - 2026-11-12

### hook 红绿对照实测：**WorkBuddy 不支持**，覆盖率修正为 3/4

**测试方法**：备份配置 → 挂 `SessionStart` hook（输出 `additionalContext: ORANGE-42`）
→ 手工验证脚本正确 → 在 WorkBuddy 新开会话问「测试码是什么」

**结果**：
- 模型答"我不知道测试码是什么"（未看到注入）
- **决定性证据**：hook 日志文件**不存在** = 脚本从未执行 = **hook 未被调用**
  （不是"调用了但注入没生效"，是"根本没调用"）
- 旁证：WorkBuddy 是纯 Electron（app.asar 173.6 MB），无 codebuddy CLI；
  `~/.workbuddy/settings.json` 无 hooks 键；活跃日志无 hook 记录

**结论**：官方文档的 hooks 是 **CodeBuddy Code（CLI）** 的，**桌面版不读**。
"共享 session 布局 ≠ 共享 hooks 配置"。

### 最终覆盖率
| agent | hook |
|---|:--:|
| DSH | ✅ |
| Hermes | ✅ |
| pi | ✅ |
| **WorkBuddy** | ❌ **实测不支持** |

**hook = 3/4**。WorkBuddy 只能 MCP（无法自动注入），除非接受 proxy（代价：丢官方积分）。

### 还原
`~/.codebuddy/settings.json` 已还原为原始 425 B（SHA256 `4AF502813AA31FB5` 一致），
测试 hook 目录已删除，备份保留。

## [0.37.0-hook-4of4] - 2026-11-12

### hook 能力最终结论：4/4 全覆盖（修正上一轮的 2/4）
上一轮说 WorkBuddy/pi "没找到 hook"是**我的判据错**——我只在 `.workbuddy` 目录找，
而 WorkBuddy 的 hook 落在 **`~/.codebuddy/settings.json`**。

| agent | hook 机制 | 依据 |
|---|:--:|---|
| DSH | ✅ | `dsh-hooks-claude-code` 核心模块 |
| Hermes | ✅ | `config.yaml` 的 `hooks_auto_accept` |
| **WorkBuddy** | ✅ | [CodeBuddy Code Hooks 官方文档](https://www.codebuddy.cn/docs/cli/hooks)（9 个事件完整规范） |
| **pi** | ✅ | 扩展 API（`before_agent_start` 返回 `{message}`），本机已有 `mnemon.ts` 活例 |

### 关键发现
- WorkBuddy 的注入字段 = `hookSpecificOutput.additionalContext`（与 Claude Code 一致）
- pi 的注入 = `before_agent_start` 返回 `{ message }`
- **Windows 限制**：WorkBuddy 的 hook **强制 Git Bash**（不支持 cmd/PowerShell）
- WorkBuddy 的 hook 官方标注 **Beta**

### 修正 ADR-025
从"按 agent 能力分层"改为"**统一能力、不同实现**"（4 个都能自动注入）

### 新增
`docs/19-Hook能力最终结论.md`

### 待验证
WorkBuddy **桌面版 GUI** 是否读 `~/.codebuddy/settings.json`（官方文档写的是 CodeBuddy **Code** CLI）

## [0.36.0-hook-survey] - 2026-11-12

### hook 能力实测：覆盖 2/4（比预期悲观，如实记录）
| agent | hook 机制 | 证据 |
|---|:--:|---|
| DSH | ✅ | 本机有 `dsh-hooks-claude-code` 核心模块（需挂载） |
| Hermes | ✅ | `config.yaml` 的 `hooks_auto_accept: true` |
| WorkBuddy | ❌ | `.workbuddy` 无 hook 条目；settings.json 无 hooks 字段；db 无 hook 表 |
| pi | ❌ | `settings.json` 无 hooks 字段 |

### 设计修正（ADR-025）
"hook 优先"改为**按 agent 能力分层**：
- 有 hook（DSH/Hermes）→ hook 注入
- 只有 MCP（WorkBuddy/pi）→ MCP 主动查，**无法自动注入**
- 都没有 → proxy（可选，代价高）

### 承诺必须分开
- **统一存取（MCP）**：4/4 已实现并验证 ✅
- **自动注入（hook）**：2/4 有机制、**0/4 已验证生效** ⚠️

### 待验证 4 项
DSH 挂载后 additionalContext 生效 / Hermes hook 协议 / WorkBuddy 别处 hooks / pi 扩展注入

## [0.35.0-hook-principle] - 2026-11-12

### 澄清 hook 注入原理 + 多模型场景下 proxy 的影响

**hook 原理**（[Claude Code 官方规范](https://code.claude.com/docs/en/hooks)）：
- agent 在生命周期点执行外部命令；**stdin 收事件 JSON**（session_id / transcript_path / tool_name）
- **stdout 回 JSON**，其中 `hookSpecificOutput.additionalContext` **就是注入**
- 采集侧读 `transcript_path` 指向的对话文件

**多模态模型受 proxy 影响吗**：取决于配置结构
- WorkBuddy/pi 是"**每模型独立 url**"→ **可以只接管文本模型，图像/音频/视频不受影响**
- 统一 base_url（环境变量）→ 全部受影响
- 关键约束：**多模态端点禁止接管**（body 极大 + 无注入收益 + 单点风险）

**本机实测依据**：WorkBuddy 的 3 个模型各有独立 url，其中 `deepseek-flash` 确实 `supportsImages=true`

### 新增
- `docs/18-Hook原理与多模型影响.md`
- **ADR-026**：hook 注入机制（additionalContext）+ proxy 多模态约束

## [0.34.0-proxy-audit] - 2026-11-12

### DeepSeek 上游可行性 + CC Switch 调研 + proxy 审核
**结论：能接。** DeepSeek 是 OpenAI 兼容协议；CC Switch 本机实例正在用
`https://api.deepseek.com`（codex）/ `https://api.deepseek.com/anthropic`（claude）。

### CC Switch 架构调研（本机 DB 实证）
- 18 张表，核心：`providers`(14) / `provider_endpoints`(4) / `proxy_config`(4) / **`proxy_live_backup`**
- `proxy_config` 真实参数：流式首字节 90s / 空闲 180s / 非流式 600s；
  熔断 连续失败 8(4) / 恢复 3(2) / 冷却 90(60)s / 错误率 0.7(0.6) / 最少请求 15(10)
- `live-state.json`：每 app 有 `mode: direct|proxy` + `proxy_route`
- 架构演进：v3.8 SQLite 化 → v3.9 Local API Proxy + takeover + 熔断

### proxy 审核结果
**P0（已修）**：URL 拼接 bug —— 上游 `.../v1` + 请求 `/v1/...` 拼成 `/v1/v1/...` → 404
（实测修前 404、修后 200；`join_url()` + 3 个单测）
**P1（已修）**：超时一刀切 300s → 改为只限建连 30s，不设总超时（流式长对话不被截断）
**P1（已修）**：`no_proxy()` 硬编码 → `ORBITAL_PROXY_USE_SYSTEM_PROXY=1` 可开启
**P1（未修）**：无熔断（P2 排期）
**P2 差距**：单一上游 / 无接管快照 / 无请求日志 / 无用量统计

### 设计产出
`docs/16-Proxy审核与上游配置设计.md`：三个概念（Upstream / Takeover / ProxyRuntime）+ 设置页布局 +
与 CC Switch 的明确分工（它是配置切换器+网关，我们是记忆层）

## [0.33.0-proxy-forward] - 2026-11-12

### 澄清：MCP 与 proxy 如何协同（回答用户架构疑问）
- **MCP = pull**（agent 主动调工具，能写能查，但**改不了已发出的 prompt**）
- **proxy = push**（拦到 LLM 请求后往 prompt 里塞，只读）
- **注入点 = 改写 agent 的模型 base_url**（ADR-024）：
  `~/.workbuddy/models.json` 的 `url`、`~/.pi/agent/models-store.json` 的 `baseUrl`、
  或 `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL`
- **不改 base_url，proxy 永远收不到请求** —— 这是原设计缺失的前提

### 实现：proxy 透明转发（M1 任务表补 1.13）
- `crates/orbital-proxy/src/main.rs` 重写：兜底路由全量转发 `ORBITAL_UPSTREAM`
- SSE 用 `Body::from_stream` **原样透传**（不缓冲）
- 响应带 `X-Orbital-Inject: skipped(not_implemented)`（不假装注入）
- 未配置上游 → **502 + 明确错误**
- `no_proxy()`：本机 `HTTP_PROXY` 是 SOCKS5，会静默卡死

### 验证（本轮实跑）
1. `/health` → `upstream_configured=true`
2. 通过 proxy 转发 `/api/tags` → **200、模型数=2**
3. 上游未配置 → **502**
4. **core 不在时仍转发成功** ← M1 验收第 2 条的核心

### 计划修正
`docs/05` M1 任务表**补 2 条**（1.13 proxy 转发、1.14 三进程状态页）——
原表 12 项里没有它们，但验收标准要求，属**计划自身不一致**。

## [0.32.0-dsh-verified] - 2026-11-12

### DSH 接入真机验收通过（用户重启 DSH 后）
三条证据：
1. **进程**：`orbital-mcp` pid=40804，**父进程 = DeepSeek Harness.exe (pid 12680)**，DSH 起来后 11 秒 spawn
2. **协议**：直接探测 → `initialize` 返回 serverInfo=orbital；`tools/list` **9 个工具**、schema 完整
3. **连接保持**：CPU≈0（stdio 等待），未被 DSH 杀掉

⚠️ 当前会话工具列表未出现 `mcp__orbital__*`——会话恢复时工具列表已固定，**需新开会话**才可见。

### 4 agent 接入最终结果
| agent | 方式 | 状态 |
|---|---|---|
| Hermes | MCP | ✅ 实测能写/能查/归属正确 |
| pi | MCP | ✅ 实测通过 |
| DSH | MCP | ✅ 已拉起并保持（工具需新会话） |
| WorkBuddy | proxy 旁路 | 设计如此（无 MCP 配置，M3 覆盖） |

## [0.31.0-test-coverage] - 2026-11-12

### 测试覆盖补到 43（M1 验收线 ≥40 已达标）
新增 9 个测试（原 34）：

**attach.rs（3）**
- spec 走 YAML 分支：保注释与其他条目
- 二次变更不覆盖用户原始备份
- 非法边类型被拒

**db.rs（1）**
- FTS UPDATE 触发器：旧内容不再命中、新内容可命中

**service.rs（5）**
- update 改成重复内容 -> 可读的业务错误（非 INTERNAL）
- invalidate 写 valid_to 且行保留（可逆）
- hard 物理删除后查不到
- health 健康库 ok=true 且 reasons 为空
- 无端点时 backfill 为 no-op 不 panic

### 结果
`cargo test -p orbital-core` → **43 passed**

## [0.30.0-p1-gaps] - 2026-11-12

### T-M1-12 设计缺口补齐（4/12）
| 编号 | 结果 |
|---|---|
| P1-3 软删边过滤 | ✅ 已修 + 单测（软删 B 后 A 的 related 不再返回 B） |
| P1-6 写入限流 | ✅ 已修 + 单测（200/分钟/agent，超限 429 可重试） |
| P1-7 64KB UTF-8 字节 | ✅ 已在 T-M1-03 完成 |
| P1-12 agent token | ✅ 已在 T-M1-04 完成 |

**未做 8 项均有明确里程碑归属**（P1-1→M4、P1-2/4/10→M2、P1-5 待确认口径、P1-8/9→M3、P1-11→M5），详见 `docs/11 §3 状态更新`。

### 踩坑
两个 P1 测试并行跑且都改全局 env（限流阈值）→ 互相干扰误判失败；
改 `static P1_LOCK: Mutex<()>` 串行 + 临时目录按 tag 隔离后同时通过。

### 测试
`cargo test -p orbital-core` → **34 passed**（M1 目标 ≥40，仍差 6）

## [0.29.0-embed-write] - 2026-11-12

### 完成：T-M1-11 写入链路接入嵌入
- `MemoryService::write` 成功后调 `embed_one()`：算向量 → UPSERT `memories_vec` → `embedding_status='ok'`
- **失败只标 `failed` 并 warn，绝不回滚已写入的记忆**（嵌入是后置增强，F7）
- 新增 `backfill_embeddings(limit)`（幂等补算）与 `mark_embedding_failed()`

### 真实验证
```
PASS 写入 3 条成功
PASS health 可读
  vectors_total=3 vectors_missing=0      ← 从 100 降到 0
PASS 向量已落库（total>=3）
PASS 无缺失（missing==0）
PASS 记忆本身仍可读
EMBED-WRITE: PASS
```

### remaining
召回用向量路（属 M3 四路召回）；M1 只要求"能算能存"

## [0.28.0-design-sync] - 2026-11-12

### 研发回顾：设计文档与计划全量同步
- docs/04 §2.8 新增「嵌入配置（ADR-022）」：一个模型规格 + N 端点、探测句验证判据、本机实测表
- docs/05 追加「M1 实际进度」：13 张任务卡 vs 12 个原表项的真实映射 + **验收差距如实记录**
- docs/10 记录本轮新增依赖（sha2 / uuid / serde_yaml / reqwest / sysinfo），全部由真实需求驱动
- PLAN.md 当前任务转为 T-M1-11（partial），新增「测试数 33 / 目标 40」状态行
- 新建 HANDOFF.md（≤40 行，会话交接）

### 如实记录的差距
| 项 | 目标 | 现状 |
|---|---|---|
| 测试数 | ≥40 | **33** |
| 4 agent 能写能查 | 4/4 | **3/4**（WorkBuddy 走 proxy） |
| 向量缺失 | 0 | **100**（写入链路未接） |

## [0.27.0-embed-verify] - 2026-11-12

### T-M1-11 第一段：嵌入配置 + 端点一致性验证（ADR-022）
- `crates/orbital-core/src/embed.rs`：`ModelSpec`（全局唯一）+ `Endpoint`（cloud/local，任意多个）
- 配置优先级：环境变量 > `~/.orbital/secrets.json` > 内置默认；密钥不进仓库
- `check_dimension()` 写入前校验维度；`verify_endpoints()` 用 5 条探测句比对
- `examples/verify-embed.rs`：真实端点验证

### **真实验证通过**
```
PASS siliconflow  dim=1024  1135ms  基准端点
PASS ollama       dim=1024  263ms   最小余弦 0.9992（阈值 0.99）
EMBED-VERIFY: PASS（所有端点同一向量空间）
```
即：硅基流动与本地 Ollama 算出的向量**是同一个空间**，可以混用。

### 踩坑
1. 本机 `HTTP_PROXY=127.0.0.1:10808` 实为 SOCKS5 → reqwest 静默超时；改默认 `no_proxy()`（可用 `ORBITAL_EMBED_USE_PROXY=1` 开启）
2. 云端 `timeoutMs=2000` 太短（实测需 1135ms）→ 调 15000

### remaining
写入链路接入 / 召回向量路

## [0.26.0-merge-update] - 2026-11-12

### attach_json 改为键级合并（不再整体替换）
真实需求：Hermes/DSH 的旧条目里有用户自己加的 `HTTP_PROXY` / `NO_PROXY` 等设置，
如果"整体替换"会把它们丢掉，导致用户环境行为改变。

- 完全一致 → `changed=false`（幂等）
- 已存在但 `command` 不同 → **合并更新**：`command`/`args` 用新值，`env` 做**键级合并**
- 单测 `update_merges_and_keeps_user_fields`：`command=OLD-EXE` + 用户字段 → 更新后 command 变、用户字段保留
- `cargo test` → **28 passed**

### 状态
T-M1-09 remaining = **0**

## [0.25.0-attach-spec] - 2026-11-12

### 配置化闭环：attach 接受 AdapterSpec
- `attach::attach_by_spec(db, spec, mcp_command)` 成为**唯一入口**
  - `format=json` → 结构化 patch（保其他字段）
  - `format=yaml` → 文本级插入（保注释）
  - `supports_mcp=false` → **明确拒绝并提示走 proxy**（不假装支持）
- 单测 2 项：spec 驱动接入 / 不支持 MCP 时拒绝；`cargo test` → **27 passed**

### 状态
ADR-021 的"接入配置化"从数据模型到入口全部闭环：接新 agent 只需放一个 json 到 `~/.orbital/adapters/`。

### remaining
"更新已存在条目"（条目存在但指向旧版本时替换）尚未沉淀进 attach.rs

## [0.24.0-adapter-spec] - 2026-11-12

### 设计修正：Agent 接入配置化（ADR-021）
**用户指出的产品级问题**：原接入写死成"本机 4 个 agent + 一次性脚本"，交付给其他用户完全用不了。

- 新增 `crates/orbital-core/src/adapter.rs`：`AdapterSpec`（id / mcp_config_path / format / mcp_key / supports_mcp / supports_proxy / note）
- 内置 4 条（路径全用 `%VAR%`，**不写死本机绝对路径**）
- 用户可在 `~/.orbital/adapters/*.json`：**同名覆盖、新 id 追加**——接新 agent 不改代码不重编译
- `workbuddy` 按实测标 `supports_mcp=false`（走 proxy 旁路）
- 单测 4 项：路径展开 / 同名覆盖 / 新 id 追加 / 非法 JSON 报错；`cargo test` → **25 passed**

### 参考
旧项目 `agent-memory-hub` 的 `agents/adapter.rs` 已用同一思路解决过；本项目额外要求
"幂等判断必须比较 command 指向"（Hermes/DSH 都踩到过"已有键但指向旧版本"）。

### remaining
`attach` 尚未改为接受 `AdapterSpec`（胶水待补）

## [0.23.0-health] - 2026-11-12

### 完成：T-M1-10 静默失败防护
- `service.rs::health()` + MCP 工具 `orbital_health`
- `HealthReport`：ok / degraded / **reasons** / schema_version / missing_tables / memories
  / soft_deleted / vectors_total / **vectors_missing** / pending_intents
- 原则：**任何异常都进 reasons 并令 degraded=true，不再"只 warn"**

### 验收（`scripts/verify-health.py` 7 项全 PASS）
tools/list 含该工具 / health ok=true / 字段齐全 / 写 100 条全拿 id / **回查 100/100**
/ 向量缺失不误判故障 / 向量缺失数可见 = 100

### 踩坑
编译报"无法删除 exe"——旧 MCP 进程占用（B12 老坑）；taskkill 后仍占用，改**重命名旧 exe** 再编译成功。

### 待 M1-11 补验
"人为让嵌入失败 → /health 变黄"（当前嵌入未接入，向量缺失属正常态）

## [0.22.0-attach-done] - 2026-11-12

### 完成：T-M1-09 4 agent 接入（3/4 MCP + 1 走 proxy）
| agent | 路径 | 结果 |
|---|---|---|
| Hermes | `%LOCALAPPDATA%\hermes\config.yaml`（YAML） | ✅ 实测通过 |
| pi | `~/.pi/agent/mcp.json`（JSON） | ✅ 实测通过 |
| DSH | `~/.dsh/profiles/desktop/cordis.patch.yml`（YAML） | ✅ 实测通过（需重启生效） |
| WorkBuddy | Electron/VSCode 系，`settings.json` 只有 `codingcopilot.*` | ❌ 无 MCP 配置 → 走 M3 的 proxy 旁路 |

### WorkBuddy 调查结论
安装 `%LOCALAPPDATA%\Programs\WorkBuddy`，数据 `%APPDATA%\WorkBuddy`；
全目录扫描零 mcp 关键字；**拒绝写猜测性配置**（无法证明 CodeBuddy 会读它）。
⇒ 按 `docs/04 §3.5` 三层采集设计走 B 层（proxy 旁路），由 M3 覆盖。

### 三个备份均可一键回滚
`config.yaml.bak.orbital` / `mcp.json.bak.orbital` / `cordis.patch.yml.bak.orbital`

## [0.21.0-dsh-attached] - 2026-11-12

### T-M1-09 第三个 agent 接入：DSH（3/4）
- 真实位置：`~/.dsh/profiles/desktop/cordis.patch.yml`
- **发现该文件早已有 `mcp-orbital`，但指向旧项目**（agent-memory-hub + 旧 db + `--agent dsh`）
- 精确替换：command 换新 exe、args 清空、env 补 DB 与令牌（保留代理清空设置）
- 校验：块外内容逐字未变、旧路径无残留；PyYAML 因 `!!js` 自定义标签报错属误报，已用 `!!str` 替换后验证通过
- **实测通过**：写入 ✓ / 查回 ✓ / 归属 agent=dsh ✓
- 备份：`cordis.patch.yml.bak.orbital`

### 接入进度
Hermes ✓ / pi ✓ / DSH ✓ / WorkBuddy 待确认（未发现 MCP 配置文件）

## [0.20.0-pi-attached] - 2026-11-12

### T-M1-09 第二个 agent 接入：pi（2/4）
- 配置位置查明：`~/.pi/agent/mcp.json`（标准 `mcpServers` JSON）
- 备份 `.bak.orbital` → 写入 orbital 条目 → JSON 校验（server 集合不变，共 7 个）
- **实测通过**：写入 ✓ / 查回 ✓ / 归属 agent=pi ✓

### DSH / WorkBuddy 调查结论
- DSH：`~/.dsh/cordis.patch.yml` 仅 3 字节，非实际 MCP 位置；需查 desktop profile 内的配置
- WorkBuddy：`WorkBuddy` 目录下只有 automation 与 Claw，**未发现 MCP 配置文件**

### 当前接入进度
Hermes ✓ / pi ✓ / DSH 待查 / WorkBuddy 待查

## [0.19.0-hermes-attached] - 2026-11-12

### T-M1-09 首个 agent 真实接入：Hermes（1/4）
- 在新库注册 hermes 令牌；备份 `hermes\config.yaml.bak.orbital`
- orbital 块从**旧项目**（agent-memory-hub exe + `~/.orbital-dev/orbital.db`）切到**新项目**
- 改后校验：PyYAML 解析通过（76 顶层键）、旧路径无残留、其余内容逐字未变
- **实测通过**：写入 ✓ / 查回 ✓ / 归属 agent=hermes ✓ / stats ✓

### remaining
pi / WorkBuddy / DSH 尚未接入（需先确认配置位置与格式）

## [0.18.0-verify] - 2026-11-12

### 第二次全链条验证（docs/14）：69 项逐条核对
- 20 条 ADR 全覆盖 + docs/02/03/04/10 关键条款 + M1 每个任务验收
- 结果：56 一致 / 4 已修 / 3 待修（现 1 待授权）/ 6 计划内

### 本轮修掉 2 处
- **D1** `has_yaml_key` 幂等判断：原来只看键名存在，不看指向 → Hermes 接的是旧项目却被判为"已接入"，静默无操作。
  改为比较 `command` 是否等于我们写入的 `orbital-mcp`。
  红绿对照（副本演练）：修复前 `hermes-main changed=false backup=false` → 修复后 `changed=true backup=true`。
- **D3** `memory_forget` 的 `hard` 档：设计要求"需用户确认"，已实现为必须显式传 `confirm:true`，否则拒绝。

### 仍待用户授权
- **D2** 真实接入（会改动你现有的 agent 配置，不可逆）

## [0.17.0-yaml-patch] - 2026-11-12（部分完成）

### T-M1-09 补做：YAML patch 器 + 副本演练
- `attach_yaml()`：**文本级插入**（保住注释与其他条目），插入后用 `serde_yaml` 校验整份文本，
  不合法就放弃写入（原文件不动）；无 `mcp:` 锚点则拒绝，不猜结构
- `crates/orbital-core/tests/attach_drill.rs`：把真实配置**复制到临时目录**再演练，真实文件只读
- `cargo test -p orbital-core` → **21 passed**（新增 YAML 3 项）

### 演练结果（3 个真实配置副本）
| 配置 | 结果 |
|---|---|
| `~/.dsh/cordis.patch.yml`（3 B） | REFUSE（无锚点） |
| `~/.hermes/config.yaml`（1.5 KB） | REFUSE（无锚点） |
| `%LOCALAPPDATA%\hermes\config.yaml`（18 KB） | changed=false |

### 🔴 演练发现的真缺陷
Hermes 主配置里已存在 `orbital:` 条目，但指向 **旧项目**（`agent-memory-hub` 的 exe + `~/.orbital-dev/orbital.db`）。
`has_yaml_key` 只判断键名存在 → 把"需要更新"误判成"已接入"。
**修法**：幂等判断要比较 `command` 是否指向当前项目的 exe。

### 仍未做
真实接入（会改用户现有 agent 配置，不可逆）——待用户明确授权。

## [0.16.0-attach-engine] - 2026-11-12（部分完成）

### T-M1-09 接入引擎（引擎就绪，真实接入待授权）
- `crates/orbital-core/src/attach.rs`：`attach_json()`
  - 只 patch `mcpServers.<key>`，其余字段原样保留
  - 原子写（临时文件 → fsync → rename）
  - 首写字节级备份 `.bak.orbital`（不覆盖用户备份）
  - `.orbital-pending` 意图日志（成功才删，失败靠前滚）
  - 幂等 + 非 JSON 明确拒绝且不动原文件
- `cargo test -p orbital-core` → **18 passed**（attach 4 项：备份/幂等/拒绝非 JSON/清 pending）

### 未完成（remaining=2，已如实标注）
1. **YAML patch 器**：DSH（cordis.patch.yml）与 Hermes（config.yaml）都是 YAML，当前引擎只支持 JSON
2. **真实接入**：会改动用户现有 agent 配置（不可逆），需用户授权后再执行

## [0.15.0-single-instance] - 2026-11-12

### 完成：T-M1-08 单实例唤醒
- single-instance 回调改为**唤醒已有窗口**（`spawn_blocking` + `Mutex` 串行化 + `unminimize/show/set_focus`）
- 退出时移除托盘图标（`set_visible(false)`），防更新重启后任务栏残影

### 验收（本轮实跑）
- 第一个实例 3 进程；启动第二个 → 进程数仍 3、第二个进程退出、日志 `second instance -> wake main window`
- 连点 5 次 → 进程数仍 3；唤醒日志累计 6 条
- 第二个实例日志无 `three processes`（没重复拉起 core/proxy）
- 正常退出 → 残留 0、运行态已删、日志 `launcher stopped` + `tray icon hidden on exit`

### 已知预期行为
强杀 GUI 会留下 2 个孤儿进程（`RunEvent::Exit` 不触发）；T-M1-06 已验证重启时可接管。

## [0.14.0-tray-done] - 2026-11-12

### 完成：T-M1-07 托盘与生命周期（四项全验）
- 托盘图标显示：UIA `name="Orbital Pro · 记忆中枢"` / `NotifyItemIcon` / 带 28×28 Image
- 图标来源改为项目正式 logo（`agent-memory-hub/32x32.png`，245 色），替换 Spike 的紫色占位方块
- 关闭主窗 = 隐藏不退出（进程仍在）
- 左键切换主窗：**修复两处真 bug** 后双向验证通过
- 右键自绘面板：320×420 逻辑像素（系统 DPI 1.825 → 584×749 物理像素）
- 静默启动：`on_page_load` 首次才 show（消除白屏）

### 修掉的 3 个真缺陷
1. `TrayIcon` 被丢弃 → 引用计数归零 → 图标建完即移除（官方文档原文依据）
2. `on_page_load` 无条件 show → 覆盖 hide（窗口刚隐藏又被显示回来）
3. `w.is_visible()` 在 config 初始 `visible:false` 后失真（窗口在屏上却返回 Ok(false)）→ 改为自维护 `AtomicBool`

### 修掉 1 处设计偏差
原实现绑定 `.menu()` → 右键弹**原生菜单**，与 ADR-016（自绘面板）矛盾；已去掉 `.menu()`，右键直接弹面板。

## [0.13.0-tray] - 2026-11-12（部分完成）

### T-M1-07 托盘与生命周期
- src-tauri/src/tray.rs：托盘 + 左键 toggle 主窗 + 右键菜单（打开/面板/退出）+ 自绘面板窗 320×420
- 关闭主窗 → prevent_close + hide（进程继续）；静默启动 → on_page_load 再 show
- 图标：从 icon.ico 生成 	ray.png（32×32）

### 已验证
三进程 / 主窗可见 / **关闭主窗后进程仍在** / 到点退出残留 0 / 运行态已删

### 未验证（remaining=3，下轮优先）
① 托盘图标真机截图 ② 左键点击 toggle ③ 托盘菜单"退出"实际点击

## [0.12.0-launcher] - 2026-11-12

### 完成：T-M1-06 三进程启动器
- src-tauri/src/launcher.rs：回收孤儿 → spawn core/proxy（同 exe + --role）→ 写运行态 → 监控重启
- 孤儿判据三条同时成立：pid 存活 + 命令行含 --role + exe 路径一致（比 Spike 更严，防误杀）
- 崩溃重启：800ms 轮询 + 指数退避 1s/2s/4s，上限 3 次
- 运行态 launcher.ndjson 整体重写（不追加，防 pid 复用误杀）
- core/proxy 从占位改为真实 HTTP 监听（/ 返回 role+pid，作为行为证据）
- GUI 退出（RunEvent::Exit）→ stop()：杀子进程 + 删运行态
- 依赖调研：sysinfo 0.40.1（官方文档确认 System::new_all 返回 Result；Process 不可 Clone）

### 验收
cargo test --manifest-path src-tauri/Cargo.toml → **4 passed**
scripts/smoke-launcher.ps1（真实三进程）→ **8 项全 PASS**
三进程 / 8791=core / 8790=proxy / 运行态两角色 / 杀 core 自动重启且 pid 变化 / 强杀 GUI 留 2 孤儿 / 重启接管回 3 进程 / 退出 0 残留且运行态已删

### 踩坑
手写 .ps1 未带 BOM → PowerShell 5.1 按 GBK 解析，中文脚本乱码、代码被当字符串打印（老坑 C8/C9）

## [0.11.0-mcp] - 2026-11-12

### 完成：T-M1-05 MCP 8 工具
- `crates/orbital-core/src/service.rs`：`MemoryService`（write/get/recall/update/forget/link/related/stats），读走只读连接、写走队列；所有读 SQL 复用同一 `VISIBLE` 门控常量
- `crates/orbital-mcp/src/main.rs`：stdio JSON-RPC 2.0（`initialize` / `notifications/initialized` / `tools/list` / `tools/call`）；8 个工具带 `inputSchema`；错误返回 `isError:true` + `{code, httpStatus, message}`
- 新增 `--register <agent>` 入口（接入向导用，明文令牌只输出一次）
- `scripts/smoke-mcp.py`：起真实进程走 stdio 的端到端冒烟

### 验收
`python scripts/smoke-mcp.py target/debug/orbital-mcp.exe` → **13 项全 PASS**
register 64 位令牌 / initialize / tools-list 8 个齐全 / write(含 write_mode+deletion_mode) / 重复写 duplicate 同 id / get / recall / update / link / related / stats / soft 删后 404 / **错误令牌 → AGENT_TOKEN_INVALID 且进程不崩溃**

### 设计对比
14 项（12 一致 / 2 未到任务 M3）

## [0.10.0-identity] - 2026-11-12

### 完成：T-M1-04 身份硬门控 + agent 令牌
- `crates/orbital-core/src/identity.rs`：`VerifiedAgent`（品牌类型，只能由 `verify_token` 构造）、`register_agent`（返回明文令牌，库里只存 sha256）、`verify_token`、`assert_claimed_matches`
- `agents` 表新增 `token_hash` / `token_issued_at`；错误码新增 `AGENT_TOKEN_MISSING`(401) / `AGENT_TOKEN_INVALID`(403)
- `Writer::write` 签名改为 `(&VerifiedAgent, WriteInput)`——**类型级门控**，绕过校验在编译期就过不去

### 验收
`cargo test -p orbital-core` → **14 passed**（原 10 + identity 3 + 门控 1）
- 明文令牌不在库里；缺令牌(401) 与错令牌(403) 可区分
- 声明与令牌不一致 → `AGENT_IDENTITY_MISMATCH`
- **拿 dsh 令牌写 hermes 内容 → 403 且库内 0 行**

### 过程中的一个"失败"其实是门控生效
`different_agents_same_content_not_deduped` 最初用 dsh 身份写 hermes 内容 → 被判 403。这证明门控真的在拦；给 hermes 单独发令牌后通过。

### 文档同步
- `docs/04 §10` 错误码表 +2 行；`§2.5` agents 表 +2 列
- **ADR-020**：v1 schema 在 M1 交付前允许直接扩展，交付后 DDL 变更必须写迁移

## [0.9.0-writer] - 2026-11-12

### 完成：T-M1-03 单写队列（writer）
- `crates/orbital-core/src/writer.rs`：单消费线程 + 队列；批提交（100 条 / 50ms）；`BEGIN IMMEDIATE`；**COMMIT 后才回执**；去重 `(agent_id, content_hash)`；写审计事件；COMMIT 后追加 journal；BUSY 退避 50/100/200ms
- `Db::conn_mut()`：写路径需要 `&mut Connection` 开事务
- 依赖新增：`sha2 0.11.1`（内容指纹）、`uuid 1.29.2`（id 生成）

### 验收
`cargo test -p orbital-core` → **10 passed**（原 4 + writer 6）
- 写入后可查回；同 agent 重复 → `duplicate`；**跨 agent 相同内容不合并**（设计：共识信号）
- 100 条并发写零丢失；审计事件 + journal 行齐全；空/超 64KB 均 `INVALID_ARGUMENT`

### 制度（用户 2026-11-12 明确要求）
- `AGENTS.md` 红线 **12**：任务收尾必须做「**设计对比验证**」并写进任务卡
- `docs/05 §11.3` 改为「收工四步」，第 2 步即设计对比
- T-M1-03 的执行记录里已附 **15 项**对比表（12 一致 / 1 未到任务 / 2 明确不在本卡）

## [0.8.0-schema] - 2026-11-12

### 完成：T-M1-02 schema v1（+ 一致性审计）
- `crates/orbital-core/src/schema.rs`：`SCHEMA_V1` 幂等 DDL——6 张实体表（memories / memory_facts / edges / memory_events / agents / **memories_vec**）+ FTS5 虚表 + 3 个同步触发器
- `db.rs`：`user_version` 0→建表并置 1；非 0 且不等于 1 → `SCHEMA_TOO_OLD`（数据保留）；`PRAGMA` 补齐为 5 条；新增 `table_names()` / `missing_v1_tables()` 供体检
- **ADR-019**：向量用普通表 + BLOB（不引 sqlite-vec），升级触发条件 = 活跃记忆 >5 万条 或 recall P95 >200ms
- `docs/12-实现一致性审计.md`：M1-00/01 与设计的对照（一致 21 / 偏差 5 已修 / 未到任务 9 / 风险 1）

### 验收
`cargo test -p orbital-core` → **4 passed**
1. 建库后表齐全 + bundled SQLite ≥3.54
2. 旧 schema(99) → `SCHEMA_TOO_OLD` 且旧表保留
3. FTS 触发器：插入后 `MATCH 'port'` 命中
4. 向量表按 `agent_id` 分区

`verify.ps1` → **VERIFY: PASS**

### 关键结论
**FTS5 + trigram 在 rusqlite `bundled` 里直接可用**，无需额外 feature 或加载扩展（此前标"待核实"）。

### 审计修的 5 处偏差
`src-tauri` 端口硬编码 → env 可配；字体未内置 → `@fontsource-variable/*`；错误码文档 6→10；`docs/03` 补色值表；`docs/10` 补 `@types`。

## [0.7.0-skeleton] - 2026-11-12

### 完成：T-M1-01 工作区骨架与依赖锁定
- **Rust workspace**：`crates/orbital-core`（lib：`error` 统一错误码 / `db` 打开+版本自检 / `config` 集中配置）、`crates/orbital-mcp`（stdio 占位）、`crates/orbital-proxy`（axum `/health`）
- **`src-tauri`**（独立 workspace）：`--role core|proxy` 在 `tauri::Builder` 之前 return（Spike 已验证的写法）；插件 single-instance / store / window-state
- **前端**：React 19.3.1 + Vite 8.3.8 + TS 7.0.2 + Tailwind 4.3.5；`styles.css` 落地 `docs/03 §3` 的 token（5 字号 / 3 字重 / 3 间距 / 2 圆角 / 颜色，深浅色跟随系统）
- **`scripts/verify.ps1`**：五步一键校验

### 验收（本轮实跑）
`powershell -File scripts/verify.ps1` → **VERIFY: PASS**
- `cargo check --workspace`（30.2s）
- `cargo test -p orbital-core`：2 passed（bundled SQLite ≥3.54 / 旧 schema 拒绝服务）
- `npm run typecheck`（TS 7.0.2）
- `npm run build`：css 10.64 kB / js 220.99 kB（gzip 69.31 kB）
- `cargo check src-tauri`

### 踩坑（已写进任务卡）
1. `expect_err` 要求 `T: Debug` → `Db` 加 `#[derive(Debug)]`
2. TS 7 对 CSS 副作用导入报 **TS2882** → 加 `src/vite-env.d.ts`
3. `tauri-build` 需要 `icons/icon.ico` 且 `frontendDist` 目录必须存在 → 先 `npm run build` 再 `cargo check src-tauri`

### 版本实测修正（已同步 `docs/10`）
`rusqlite 0.41 → 0.42.0`、`tauri-plugin-single-instance 2.4.0 → 2.5.5`、`window-state 2.4.1 → 2.5.0`；新增服务端 crate 版本（axum 0.8.9 / tokio 1.54.1 / thiserror 2.0.21 等）

## [0.6.0-spike] - 2026-11-12

### 完成：T-M1-00 技术 Spike（架构假设全部成立）
- `spike/m1-spike/`（纯 Rust，无 Tauri）：同一 exe 按 `--role` 分派 + 启动器监督 + 孤儿接管
  - 实测 6 项 PASS：三进程 / 双端口 / 运行态文件 / 杀 core 后重启 / 强杀 GUI 后接管孤儿 / 退出清理
- `spike/m1-tauri/`（Tauri 2.12.5 + single-instance 2.5.5）：验证 headless 与单实例
  - A1 GUI 有 `msedgewebview2` 子进程（1 个）；A2 `--role core` **没有**（0 个）
  - B1 启动 core **不触发** single-instance 回调；B2 第二个 GUI **触发**
- `spike/run-spike.ps1`、`spike/run-spike-tauri.ps1`：可重跑的验收脚本（日志落 `spike/evidence/`）

### Spike 修出的 2 个真实设计缺陷（已改并复测）
1. 孤儿 kill 后不等端口释放就 spawn → `BIND_FAIL 10048`；加 `wait_port_free(3s)`
2. 重启后不刷新运行态文件 → 退出清理杀错 pid 留孤儿；加 `update_state()`

### 记录
- `docs/07` ADR-015 补充实测证据
- `docs/09` 新增 B9–B14（端口释放 / 运行态刷新 / pid 不可作判据 / exe 被锁 / 函数返回展开 / tauri-build 的 icon 与 frontendDist）
- `PLAN.md`：M1 进度 1/13，当前任务转 T-M1-01

## [0.5.0-plan] - 2026-11-12

### 新增：可执行的计划体系
- `PLAN.md`：仪表盘（当前里程碑/任务/下一步/进度总表/命令速查），**每次开工必读、收工必更新**
- `plans/_TEMPLATE.md`：任务卡模板（8 节：需求/必读文档/技术方案/功能点/验收/风险/待确认/执行记录）
- `plans/M0-设计冻结任务卡.md`、`plans/M1-详细任务卡.md`（13 张卡）、`plans/M2-详细任务卡.md`（7 张卡）
- `scripts/check-plan.ps1`：**开工门禁**——校验 PLAN.md、任务卡存在、8 节齐全、待确认清空、串跑 check-docs
- `docs/05 §11-13`：计划流程（开工五步/收工三步）、任务→必读文档矩阵、工作量总表
- `AGENTS.md §0` 改为"开工前四件事"（新增"读计划 + 过门禁"）；红线新增第 11 条

### 门禁设计要点
- **能红能绿**：当前任务 `T-M0-01` 留有"待确认（等签字）"→ `CHECK-PLAN: FAIL(1) exit 1`；指定合格任务 `T-M1-00` → `PASS exit 0`
- **不许带着猜测开工**：任务卡「待确认问题」有未决行即阻断
- **不猜规则**：不清楚先调研，调研不了问用户（用户 2026-11-12 明确要求）

### 修复
- `check-plan.ps1` 两个真实 bug：① 按 `## N 标题` 匹配导致粗体小节被误判缺失 → 改为按小节语义匹配；② 待确认段正则把表格 `---` 当终止符 → 只按「执行记录」截断
- 两张任务卡补写内容（M2 5.7KB → 11.6KB），不是降阈值

### 验收
- AWO-Task `t_orbitalpro_plan02`：`verdict=pass`（D1–D8）

## [0.4.0-design] - 2026-11-12

### 新增（设计深化）
- `docs/10-技术栈选型.md`：框架/组件/图标/图表/动效/字体全部选型，含版本锁定表与不选清单
- `docs/02` 新增 §3.3「一个 exe 到底指什么」、§3.4「托盘常驻与单实例」、§3.5「防杀的边界」
- `docs/03` 新增 §4.1.1 回合记忆栏、§4.1.2「存入记忆」三步、§4.1.3 三态空态、§5.4 预算触顶与空结果可区分、§3.6 组件来源
- `docs/04` 新增 §10.1 写入/删除的「承诺强度」（`write_mode` / `deletion_mode`）
- `docs/00` 新增 §2.3 竞品定位（CC Switch 已覆盖 Dsh/Hermes/Pi，差异化唯一在"记忆"）
- ADR-015（单 exe spawn 自身多角色）、ADR-016（托盘+单实例，不做防杀）、ADR-017（配置 patch+原子写+前滚）、ADR-018（UI 全用成熟开源）
- `research/evidence/04-CCSwitch-代码级调研.md`（59 KB）
- `research/evidence/05-dsh-mnemon-功能盘点.md`（51 KB）
- `research/evidence/06-UI选型-2026-11.md`（48 KB）
- `docs/05` M1 增补 1.9–1.11（托盘/单实例/开机自启）与对应验收

### 关键结论（改变设计）
- **"一个 exe"≠"一个进程"**：同一 exe 用 `--role core|proxy` spawn 自身 ⇒ 单文件 + 进程隔离（ADR-015）
- **托盘不是原生菜单**：自绘 WebView 面板（320×420，失焦隐藏），左键开主窗、右键弹面板（CC Switch 实证）
- **单实例回调必须 `spawn_blocking`**：Windows 下同步建窗会卡死 WebView2
- **配置写入刻意"前滚"而非回滚**：回滚会抹掉用户的应用外改动（ADR-017）
- **图表不能用一库硬扛**：`recharts@3.10.1` 类型里 `Force`/`Graph` 命中 0（无网络图）⇒ recharts + Cytoscape，合计 277 KB < ECharts 一把梭 359 KB
- **shadcn 是"三底座可选"**，不是"已换成 Base UI"；Base UI 新包名 `@base-ui/react` 1.9.0（旧名冻结在 rc）
- **CC Switch 的 Cargo.toml 写 2.8.2，Cargo.lock 实锁 2.10.3** ⇒ 版本一律以 lock 为准
- **竞品重叠已发生**：CC Switch `AppType` 已含 Dsh/Hermes/Pi ⇒ 差异化只在记忆层

### 验收
- `scripts/check-docs.ps1` 已纳入 docs/10 与 evidence 04–06，红绿对照通过

## [0.3.0-design] - 2026-11-12