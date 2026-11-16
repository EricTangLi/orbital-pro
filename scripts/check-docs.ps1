# check-docs.ps1 · 文档自检闸门
# 用法: pwsh -File scripts/check-docs.ps1 [-Fix]
# 判据:
#   1) 必读文档必须存在且非空
#   2) docs/*.md 里出现的 `docs/NN-*.md` / `research/**.md` 引用必须真实存在（防"引用不存在的文件"）
#   3) 废弃术语不得出现在 docs/（防退回旧设计）
# 输出: PASS / FAIL 明细；FAIL 时 exit 1

param([switch]$Fix)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$mustExist = @(
  'AGENTS.md','README.md','HANDOFF.md','CHANGELOG.md','PLAN.md',
  'plans/_TEMPLATE.md','plans/M0-设计冻结任务卡.md','plans/M1-详细任务卡.md','plans/M2-详细任务卡.md',
  'scripts/check-plan.ps1',
  'docs/00-项目章程.md','docs/01-产品定义-PRD.md','docs/02-系统架构.md',
  'docs/03-UIUX设计规格.md','docs/04-记忆系统设计.md',
  'docs/05-研发路线图与验收标准.md','docs/06-AI协同开发规范.md',
  'docs/07-架构决策记录.md','docs/08-术语表.md','docs/09-踩坑与反模式.md',
  'docs/10-技术栈选型.md',
  'docs/11-设计审核报告.md',
  'docs/12-实现一致性审计.md',
  'docs/13-设计目标一致性验证.md',
  'docs/14-全链条验证-M1-09.md',
  'docs/15-第三次全链条回溯.md',
  'docs/16-Proxy审核与上游配置设计.md',
  'docs/17-接入方式设计质疑与答复.md',
  'docs/18-Hook原理与多模型影响.md',
  'docs/19-Hook能力最终结论.md',
  'docs/20-WorkBuddy记忆注入路径.md',
  'docs/21-第四次全链条回溯.md',
  'docs/22-M1收尾验收报告.md',
  'docs/23-借鉴业界的产品设计重思.md',
  'docs/24-记忆数据结构评审.md',
  'docs/25-反思与回溯-ADR031之后.md',
  'research/01-代码级调研总览.md',
  'research/evidence/01-TencentDB-代码级调研.md',
  'research/evidence/02-mnemon-代码级调研.md',
  'research/evidence/03-开源记忆系统横向对比.md',
  'research/evidence/04-CCSwitch-代码级调研.md',
  'research/evidence/05-dsh-mnemon-功能盘点.md',
  'research/evidence/06-UI选型-2026-11.md',
  'research/evidence/07-审核-架构与选型.md'
)

# 废弃设计 → 替代方案（只许追加，不许删历史）
$deprecated = [ordered]@{
  '单 exe 多模式'        = '单 exe spawn 三进程（ADR-004）'
  'embedding.provider'   = '一个模型规格 + N 个端点（ADR-011）'
  'invalid_at'           = 'valid_to + 退役候选（ADR-009）'
  '8/12'                 = '控件 4 / 卡片 8（ADR-010）'
  'AutoPrune'            = '退役候选 + 人确认（ADR-009）'
  '自绘标题栏'           = '保留系统标题栏（Win11 Snap Layouts 缺口，ADR-018）'
  '@base-ui-components/react' = '@base-ui/react 1.9.0（旧名冻结在 rc，docs/10 §2.1）'
  '合并=拼接内容'        = '合并=搬附属物、内容保留 keep（ADR-036）'
  '冲突用视图'           = '冲突收件箱是表（ADR-037）'
  '撤销靠再执行相反操作' = '按 memory_events 逆操作（ADR-038）'
  '8 个 JTBD 里 6 个能在 GUI 完成' = 'M2 承诺 5 条（J1-J4 + J8 部分），可操作率 >=7 在 M6（ADR-039）'
  '否则写入拒绝' = '写入允许；召回侧要求有 fact，否则视同 private（ADR-043）'
  '旧库（v1）拒绝服务' = '旧库迁移到当前版本并保留数据；只有未来版本才拒绝（ADR-044）'
  '缓存命中 P95 未做' = '已实测：命中 P95 = 0ms（evidence/inject-cache-20261113.md）'
  '注入降级为 skipped' = '注入端点固定在 127.0.0.1:8799（core health 是 8791，两者必须分开）'
}

$fail = 0

Write-Output '=== 1) 必读文档存在性 ==='
foreach ($f in $mustExist) {
  if (-not (Test-Path $f)) { Write-Output "FAIL 缺失: $f"; $fail++ ; continue }
  $len = (Get-Item $f).Length
  if ($len -lt 200) { Write-Output "FAIL 过短($len B): $f"; $fail++ }
  else { Write-Output "ok   $f ($len B)" }
}

Write-Output ''
Write-Output '=== 2) 文档交叉引用可解析 ==='
$refPattern = '(?<![`\w/])((?:docs|research)/[^\s`\)''"]+\.md)'
$broken = 0
$docFiles = Get-ChildItem -Path 'docs','research' -Recurse -Filter '*.md' -File
foreach ($doc in $docFiles) {
  $text = Get-Content $doc.FullName -Raw -Encoding UTF8
  $matches = [regex]::Matches($text, $refPattern)
  foreach ($m in $matches) {
    $ref = $m.Groups[1].Value
    # 只校验 docs/ 前缀与 research/ 前缀的实际路径
    if (-not (Test-Path $ref)) {
      Write-Output ("FAIL 死链: {0} -> {1}" -f $doc.FullName.Replace((Get-Location).Path + '\',''), $ref)
      $broken++; $fail++
    }
  }
}
if ($broken -eq 0) { Write-Output 'ok   无死链' }

Write-Output ''
Write-Output '=== 3) 废弃术语扫描（按语境判断，不机械匹配） ==='
$hit = 0
$negation = '废止|不照抄|不采用|旧项目|旧版|旧包名|违反|禁止|已否|取代|不引入|事故|失败|错误|反例|不要|不许|拿不到|冻结|停更|不能|不支持|不是|不可复用|旧做法|历史'
foreach ($term in $deprecated.Keys) {
  # ⚠️ 2026-11-13 修：这里原来同时用了 `[regex]::Escape($term)` 与 `-SimpleMatch`。
  # .NET 的 Regex.Escape **会把空格转义成 `\ `**，而 -SimpleMatch 模式下反斜杠是字面量
  # ⇒ 含空格的废弃词（如"8 个 JTBD 里 6 个能在 GUI 完成"）永远匹配不到 = 静默失效。
  # SimpleMatch 本身按字面量匹配，不需要 Escape。
  $files = Select-String -Path 'docs/*.md' -Pattern $term -SimpleMatch -ErrorAction SilentlyContinue
  # docs/07 的"已废止清单"允许出现（它是历史记录）
  $files = $files | Where-Object { $_.Path -notlike '*07-架构决策记录.md' }
  foreach ($f in $files) {
    # 取命中行 ±1 行的上下文：若在描述"历史/否定/反例"，则放行
    $lines = @(Get-Content $f.Path -Encoding UTF8)
    $i = [Math]::Max(0, $f.LineNumber - 1)
    $lo = [Math]::Max(0, $i - 1)
    $hi = [Math]::Min($lines.Count - 1, $i + 1)
    $ctx = ($lines[$lo..$hi] -join ' ')
    if ($ctx -match $negation) {
      Write-Output ("skip  废弃术语 '{0}' @ {1}:{2}（上下文为历史/否定，放行）" -f $term, (Split-Path $f.Path -Leaf), $f.LineNumber)
      continue
    }
    Write-Output ("FAIL 废弃术语 '{0}' 出现在 {1}:{2}（应改为：{3}）" -f $term, (Split-Path $f.Path -Leaf), $f.LineNumber, $deprecated[$term])
    $hit++; $fail++
  }
}
if ($hit -eq 0) { Write-Output 'ok   未发现"推荐使用型"的废弃术语（历史记录/否定语境已放行）' }

Write-Output ''
if ($fail -gt 0) {
  Write-Output ("CHECK-DOCS: FAIL ({0} 项)" -f $fail)
  exit 1
} else {
  Write-Output 'CHECK-DOCS: PASS'
  exit 0
}
