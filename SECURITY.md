# 安全策略

## 报告漏洞

**请不要公开提 Issue。** 请通过以下任一方式私下联系：

1. GitHub 的 [私密漏洞报告](https://docs.github.com/cn/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)（仓库 → Security → Report a vulnerability）；
2. 或给维护者发邮件（见仓库主页资料）。

请尽量包含：

- 影响版本；
- 复现步骤或最小 PoC（概念验证代码）；
- 影响范围（能读到别人数据？能提权？能远程执行？）；
- 你希望如何署名（也可以匿名）。

我们会在 **7 天内**首次回复，修复后在你的同意下公开致谢。

## 支持范围

| 版本 | 是否接受安全修复 |
|---|---|
| 最新 release | ✅ |
| 上一个 minor | ✅（仅高危） |
| 更早 | ❌ 请升级 |

## 本项目的安全模型（重要）

理解这几条能帮你判断哪些"看着像漏洞"其实是设计取舍：

1. **单机单用户**：没有账号体系。任何能登录你 Windows 账户的进程，都能读数据目录。
   ⇒ "同机恶意软件能读数据库"不是漏洞，是模型边界。
2. **Agent 身份靠令牌**：每个 Agent 注册时拿到独立 token，写入时校验。
   令牌存在本机配置文件里；泄漏令牌 = 该 Agent 身份被冒用。
3. **`private` 记忆不外泄**：无法确认来源的请求（例如通过 proxy 转发的外部请求）
   **只能**看到 `scope=shared` 的记忆（ADR-042）。如果发现 private 记忆能被外部读到，**是**漏洞。
4. **注入只读**：proxy 在任何情况下都不写库。发现 proxy 能写库，**是**漏洞。
5. **本地面板**：托盘面板与主窗都只监听本机，不对外暴露端口。

## 已知的非安全问题

- 安装包未签名，Windows 会提示"未知发布者"；
- 端口（8791/8790/8799）绑定在 `127.0.0.1`，仅本机可连；
- 日志可能包含记忆内容片段——分享日志前请自行脱敏。
