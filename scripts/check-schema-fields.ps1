# check-schema-fields.ps1
# 目的：找出「只定义、从不被写入」的字段（ADR-031 教训：memories.layer 活了整个 M1）
# 用法：pwsh -NoProfile -File scripts/check-schema-fields.ps1 [-Verbose]
# 判定：对 schema.rs 的每个 CREATE TABLE 列，在 crates/**/*.rs 里 grep
#       是否出现在 INSERT 列清单或 UPDATE SET 中；不在 => 报告为「只定义未写入」

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$schemaFile = Join-Path $root 'crates\orbital-core\src\schema.rs'
if (-not (Test-Path $schemaFile)) { Write-Output "FAIL 找不到 schema.rs"; exit 1 }

$schema = Get-Content $schemaFile -Raw -Encoding UTF8

# 收集所有 Rust 源码（排除 schema.rs 自身，避免把定义当引用）
$srcFiles = Get-ChildItem (Join-Path $root 'crates') -Recurse -Filter '*.rs' -File |
    Where-Object { $_.FullName -ne (Resolve-Path $schemaFile).Path }
$srcText = ($srcFiles | ForEach-Object { Get-Content $_.FullName -Raw -Encoding UTF8 }) -join "`n"

$results = @()

# 逐表解析列名
$tableRx = [regex]'(?s)CREATE TABLE IF NOT EXISTS\s+(\w+)\s*\((.*?)\n\s*\);'
foreach ($m in $tableRx.Matches($schema)) {
    $table = $m.Groups[1].Value
    $body = $m.Groups[2].Value
    if ($table -match '_fts' -or $table -match 'fts_') { continue }

    foreach ($line in ($body -split "`n")) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith('--')) { continue }
        if ($t -match '^(PRIMARY|UNIQUE|FOREIGN|CHECK|CONSTRAINT)\b') { continue }
        $col = ($t -split '\s+')[0].Trim(',').Trim('"')
        if ($col -match '^(id|rowid)$') { continue }
        if ($col -notmatch '^[a-z_][a-z0-9_]*$') { continue }

        # 是否在非 schema 源码里被「写入」：INSERT 列清单 或 UPDATE SET
        # 支持 `INSERT INTO` / `INSERT OR REPLACE INTO`，且列清单可跨行（(?s)）
        $insertRx = [regex]('(?s)INSERT\s+(?:OR\s+\w+\s+)?INTO\s+' + [regex]::Escape($table) + '\s*\([^)]*?\b' + [regex]::Escape($col) + '\b')
        $updateRx = [regex]('UPDATE\s+' + [regex]::Escape($table) + '\s+SET\s[^;]*?\b' + [regex]::Escape($col) + '\s*=')
        $written = $insertRx.IsMatch($srcText) -or $updateRx.IsMatch($srcText)

        $results += [pscustomobject]@{ Table = $table; Column = $col; Written = $written }
    }
}

# 已知例外（必须有理由；对应 ADR）
$knownExceptions = @{
    'memories.layer'              = 'ADR-031：已定案废弃，M3-3.0 删除'
    'memories.effective_importance' = 'ADR-013：M4 才实现；schema 注释已禁止 ORDER BY'
    'memories.confidence'         = '设计保留了可信度，但当前无写入路径（M3 冲突检测时启用）'
    'memories.last_accessed_at'   = 'M3 召回时更新（当前无访问计数）'
    'memories.access_count'       = '同 last_accessed_at（M4 EI 用）'
    # ✅ agents.mode / config_path / config_sha256 已于 2026-11-12 修复：
    #    attach_by_spec 成功后调 record_agent_state() 写这三列 + last_verified_at
    # ✅ memories.superseded_by 已于 2026-11-12 修复：update() 走版本链，此处**不再是例外**
}

$onlyDefined = $results | Where-Object { -not $_.Written }
$realIssues = @()
# 整表豁免（有明确里程碑归属的表）
$knownTableExceptions = @{
    'memory_facts'  = 'M3 启用（结构化属性通道）；MCP 的 facts 参数当前明确拒绝并提示 M3，不是静默丢弃'
    'conversations' = 'ADR-031 已建表；写入靠 M3 的采集层（hook/proxy 或读 agent 会话文件），采集未实现前恒空'
}
foreach ($r in $onlyDefined) {
    $key = "$($r.Table).$($r.Column)"
    if ($knownTableExceptions.ContainsKey($r.Table)) {
        Write-Output ("ok   整表待启用: {0}  （{1}）" -f $key, $knownTableExceptions[$r.Table])
        continue
    }
    if ($knownExceptions.ContainsKey($key)) {
        Write-Output ("ok   已知未写入: {0}  （{1}）" -f $key, $knownExceptions[$key])
    } else {
        $realIssues += $key
        Write-Output ("WARN 只定义未写入: {0}  —— 必须要么实现写入，要么写 ADR 说明为什么不删" -f $key)
    }
}

Write-Output ''
Write-Output ("扫描 {0} 个字段；只定义未写入 {1} 个（其中已知例外 {2} 个）" -f $results.Count, $onlyDefined.Count, ($onlyDefined.Count - $realIssues.Count))

if ($realIssues.Count -gt 0) {
    Write-Output ("CHECK-SCHEMA-FIELDS: FAIL ({0} 个未登记的死字段)" -f $realIssues.Count)
    exit 1
}
Write-Output 'CHECK-SCHEMA-FIELDS: PASS'
exit 0