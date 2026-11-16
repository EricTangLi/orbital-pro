# verify.ps1 · 一键校验（M1 骨架的 DoD）
# 用法: powershell -NoProfile -File scripts\verify.ps1
# 判据：全部步骤 exit 0 才输出 # 死字段检查（ADR-031 教训：memories.layer 活了整个 M1 才被发现）
Write-Output ''
Write-Output '--- 死字段检查（字段必须有写入路径）---'
& powershell -NoProfile -File (Join-Path $PSScriptRoot 'check-schema-fields.ps1')
if ($LASTEXITCODE -ne 0) {
    Write-Output 'VERIFY: FAIL (死字段检查未过)'
    exit 1
}

Write-Output ''
  Write-Output "VERIFY: PASS"

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$fail = 0
function Step($name, $block) {
  Write-Output ""
  Write-Output ("=== " + $name + " ===")
  & $block
  $code = $LASTEXITCODE
  if ($code -ne 0) {
    Write-Output ("FAIL " + $name + " (exit " + $code + ")")
    $script:fail++
  } else {
    Write-Output ("ok   " + $name)
  }
}

Step 'cargo check --workspace' { cargo check --workspace --quiet }
Step 'cargo test -p orbital-core' { cargo test -p orbital-core --quiet }
Step 'npm run typecheck' { npm run typecheck --silent }
Step 'npm run build' { npm run build --silent }
Step 'cargo check src-tauri' { cargo check --manifest-path src-tauri\Cargo.toml --quiet }
# T-M10-35：HANDOFF 新鲜度（2026-11-15 加，理由是真实事故：规则写了但没闸门，
# HANDOFF 停在 0.91.8 而实际已 0.93.4，连续 20+ 个任务无人发现）
Step 'check-handoff（HANDOFF 版本 = 实际版本）' { powershell -NoProfile -File (Join-Path $PSScriptRoot 'check-handoff.ps1') }

Write-Output ""
if ($fail -gt 0) {
  Write-Output ("VERIFY: FAIL (" + $fail + " steps)")
  exit 1
} else {
  Write-Output "VERIFY: PASS"
  exit 0
}
