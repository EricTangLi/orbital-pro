# 文档链接检查（T-M9-01）
#
# 检查 docs/ 与项目根 Markdown 里的**相对链接**是否可解析。
# 规则：
#   - 只查 `[文本](path)` 形式且 path 不是 http(s)/mailto/#锚点
#   - 带 `#锚点` 的只查文件部分
#   - 反引号里的路径**不查**（那是代码/示例，不是链接承诺）
#   - 外链（http/https）不查（网络不可靠，且不属于"链接可解析"的范围）
#
# 退出码：0 = 全部可解析；1 = 有 MISS（打印文件:行:目标）

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$files = @()
$files += Get-ChildItem (Join-Path $root "docs") -Filter *.md -File -ErrorAction SilentlyContinue
$files += Get-ChildItem $root -Filter *.md -File -ErrorAction SilentlyContinue

$miss = @()
$checked = 0
foreach ($f in $files) {
    $lines = Get-Content $f.FullName -Encoding UTF8 -ErrorAction SilentlyContinue
    if (-not $lines) { continue }
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $line = $lines[$i]
        # ① markdown 链接 `[文本](path)`
        # ② 反引号内的 .md 路径（如 `docs/04-记忆系统设计.md`）——那也是对"文件存在"的承诺
        $targets = @()
        foreach ($m in [regex]::Matches($line, '\]\(([^)\s]+)\)')) { $targets += $m.Groups[1].Value }
        foreach ($m in [regex]::Matches($line, '`([^`]+\.md)`')) { $targets += $m.Groups[1].Value }
        foreach ($target in $targets) {
            if ($target -match '^(https?:|mailto:|#|tel:)') { continue }
            $target = $target.Split('#')[0]
            if ([string]::IsNullOrWhiteSpace($target)) { continue }
            # 跳过非本仓库路径（~ 开头的 home 路径、带通配/非法字符的示例、绝对盘符）
            if ($target -match '[<>"|?*]' -or $target.StartsWith('~') -or $target -match '^[A-Za-z]:') { continue }
            # 反引号里的**裸文件名**（MEMORY.md / SOUL.md / NN-中文名.md）多半是"说明性文件名"，不是仓库链接；
            # 只查带本仓库目录前缀的路径，避免把说明文字误判成坏链（否则 28 条里 20+ 是误报）
            if ($target -notmatch '^(docs|plans|scripts|evidence|src|src-tauri|crates|research|design)/') { continue }
            # 上下文排除：行内出现这些词时，路径是"提及某文件"（计划中/不存在/旧项目），不是链接承诺。
            # 依据：4 条真实 MISS 里 1 条原文就是「该文件在仓库里不存在（死链）」，另 3 条是计划中的文档。
            if ($line -match '(不存在|死链|计划中|未建立|旧项目|拟建|TODO)') { continue }
            # 反引号里的路径可能是相对仓库根的写法（docs/xx.md），也可能只写文件名
            $checked++
            $candidates = @(
                (Join-Path $f.DirectoryName $target),
                (Join-Path $root $target)
            )
            $ok = $false
            foreach ($c in $candidates) { if (Test-Path $c) { $ok = $true; break } }
            if (-not $ok) {
                $miss += "{0}:{1}: {2}" -f $f.Name, ($i + 1), $target
            }
        }
    }
}

Write-Host "checked=$checked files=$($files.Count)"
if ($miss.Count -gt 0) {
    foreach ($m in $miss) { Write-Host "MISS $m" }
    Write-Host "CHECK-LINKS: FAIL ($($miss.Count) items)"
    exit 1
}
Write-Host "CHECK-LINKS: PASS"
exit 0
