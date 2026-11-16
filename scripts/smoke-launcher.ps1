# smoke-launcher.ps1 —— T-M1-06 端到端验证
# 判据：3 进程 / 双端口返回角色 / 杀 core 后自动重启 / 退出后 0 残留 + 运行态已删
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$exe = Join-Path $root 'src-tauri\target\debug\orbital-app.exe'
if (-not (Test-Path $exe)) { Write-Output 'FAIL exe 不存在，先 cargo build'; exit 1 }

$state = Join-Path $env:LOCALAPPDATA 'OrbitalPro\runtime\launcher.ndjson'
$fails = 0
function Check($name, $cond, $detail) {
  if ($cond) { Write-Output ("PASS " + $name) }
  else { Write-Output ("FAIL " + $name + "  <- " + $detail); $script:fails++ }
}

# 清理上次残留
Get-Process orbital-app -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Remove-Item $state -ErrorAction SilentlyContinue

$gui = Start-Process -FilePath $exe -ArgumentList '--run-ms', '26000' -PassThru
Start-Sleep -Seconds 6

# T1: 三个进程
$procs = @(Get-Process orbital-app -ErrorAction SilentlyContinue)
Check 'T1 三进程（GUI+core+proxy）' ($procs.Count -eq 3) ("count=" + $procs.Count)

# T2: 双端口返回各自角色
function Get-Role($port) {
  try { (Invoke-WebRequest -Uri ("http://127.0.0.1:" + $port + "/") -TimeoutSec 3 -UseBasicParsing).Content | ConvertFrom-Json }
  catch { $null }
}
$c = Get-Role 8791
$p = Get-Role 8790
Check 'T2 8791 返回 core' ($null -ne $c -and $c.role -eq 'core') ($c | ConvertTo-Json -Compress)
Check 'T2 8790 返回 proxy' ($null -ne $p -and $p.role -eq 'proxy') ($p | ConvertTo-Json -Compress)

# T3: 运行态文件有两行（core/proxy）
$lines = @()
if (Test-Path $state) { $lines = @(Get-Content $state | Where-Object { $_.Trim() -ne '' }) }
Check 'T3 运行态含两个角色' ($lines.Count -eq 2) ("lines=" + $lines.Count)

# T4: 杀 core → 自动重启（按行为判据：端口恢复 + pid 变化）
$corePid = ($lines | Where-Object { $_ -match '"role":"core"' } | ConvertFrom-Json).pid
Stop-Process -Id $corePid -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 7
$c2 = Get-Role 8791
Check 'T4 杀 core 后自动重启（角色恢复）' ($null -ne $c2 -and $c2.role -eq 'core') ($c2 | ConvertTo-Json -Compress)
Check 'T4 重启后 pid 变化' ($null -ne $c2 -and $c2.pid -ne $corePid) ("old=" + $corePid + " new=" + $(if ($c2) { $c2.pid } else { '?' }))

# T5: 强杀 GUI → 留孤儿
Stop-Process -Id $gui.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2
$orphans = @(Get-Process orbital-app -ErrorAction SilentlyContinue)
Write-Output ("  (强杀 GUI 后残留 " + $orphans.Count + " 个孤儿)")

# T6: 重启 GUI → 应接管（不报 BIND_FAIL、进程回到 3）
$gui2 = Start-Process -FilePath $exe -ArgumentList '--run-ms', '12000' -PassThru
Start-Sleep -Seconds 6
$procs2 = @(Get-Process orbital-app -ErrorAction SilentlyContinue)
Check 'T6 重启后接管孤儿并回到 3 进程' ($procs2.Count -eq 3) ("count=" + $procs2.Count)

# T7: 等 GUI 自动退出 → 0 残留 + 运行态已删
Start-Sleep -Seconds 10
$left = @(Get-Process orbital-app -ErrorAction SilentlyContinue)
Check 'T7 退出后 0 残留' ($left.Count -eq 0) ("left=" + $left.Count)
Check 'T7 运行态文件已删除' (-not (Test-Path $state)) 'file still exists'

Write-Output ''
if ($fails -gt 0) { Write-Output ("SMOKE-LAUNCHER: FAIL (" + $fails + ")"); exit 1 }
Write-Output 'SMOKE-LAUNCHER: PASS'
exit 0
