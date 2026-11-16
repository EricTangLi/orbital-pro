# 贡献指南

感谢你愿意花时间改进 Orbital Pro。

先说清楚边界：本项目采用 [PolyForm Noncommercial 1.0.0](LICENSE)，**仅限非商业用途**。
提交贡献即表示你同意你的贡献以同一许可发布。

## 一、提 Issue 之前

请先确认这几件事，能省掉大量来回：

1. **搜一下已有 Issue**（包括已关闭的），很多问题已经讨论过；
2. 说明**版本号**（界面「设置 → 关于」或安装目录右键属性）；
3. 说明**复现步骤**：点了什么、期望什么、实际什么；
4. 附上**证据**：截图（界面问题必附）、日志片段（`%LOCALAPPDATA%\OrbitalPro\logs\gui-*.log`）。
   > 贴日志前请先删掉本机用户名与绝对路径。

## 二、Bug 报告模板

```markdown
### 环境
- Windows 版本：
- Orbital 版本：
- 是否装过其它 Agent（DSH / Hermes / pi / WorkBuddy）：

### 复现步骤
1.
2.

### 期望行为

### 实际行为

### 证据（截图 / 日志）
```

## 三、提交 PR

### 开发环境

- Rust **1.90+**
- Node.js **20+**
- Windows 10/11

### 本地跑起来

```powershell
npm install
npm run build
cargo build --release                    # 根 workspace（mcp / proxy）
cargo test --workspace
cargo test --manifest-path src-tauri/Cargo.toml
powershell -NoProfile -File scripts/verify.ps1
```

### 提交前必须通过

```powershell
cargo clippy --workspace --all-targets -- -D warnings
npm run typecheck
powershell -NoProfile -File scripts/check-links.ps1
```

### 代码约定（不接受的口子）

- 写库**只能**走 `orbital-core` 的写队列 / `MemoryService`，不许在 GUI 层另写 SQL；
- 注入是热路径：**不许**在 proxy 里写库、不许阻塞超过超时预算；
- 用户可见交互（窗口、托盘、文案）改动要**附真机截图**，不接受"应该没问题"；
- 不引新的 UI 库 / 图标库 / 图表库，除非在 PR 里说明为什么现有方案做不到；
- 不许提交密钥、令牌、个人路径、真实记忆数据。

### PR 描述里请写

1. 改了什么、为什么（一句话）；
2. 怎么验证的（命令 + 关键输出）；
3. 有什么副作用 / 未覆盖的地方。

## 四、文档类贡献

同样受欢迎，尤其是：

- 文档里的失效链接、过时截图；
- 术语不一致（以 `docs/08-术语表.md` 为准）；
- 新人看不懂的段落——**读不懂本身就是 Bug**，请指出具体哪一句。

## 五、行为准则

参与本项目即表示同意遵守 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。
