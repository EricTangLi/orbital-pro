# check-plan.ps1 · 开工门禁（计划与文档纪律）
# 用法: powershell -NoProfile -File scripts/check-plan.ps1 [-TaskId T-M1-00] [-SkipDocs]
# 判据（任一不满足 → exit 1）:
#   1) PLAN.md 存在，且含「当前里程碑 / 当前任务 / 最近更新」
#   2) 当前任务（或 -TaskId）必须在 plans/ 下有任务卡
#   3) 该任务**所在段落**必须含 7 个必填节：需求/必读文档/技术方案/功能点/验收/风险/待确认问题
#   4) 任务卡文件整体必须含「执行记录」节（收工时追加）
#   5) 任务段内的「待确认问题」不得留有 `待确认`（不许带着猜测开工）
#   6) PLAN.md 的「最近更新」>7 天 → WARN（不阻断）
#   7) 可选：串跑 check-docs.ps1

param(
  [string]$TaskId = "",
  [switch]$SkipDocs
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$fail = 0
$planPath = Join-Path $root 'PLAN.md'

Write-Output '=== 1) PLAN.md 存在性与必备项 ==='
if (-not (Test-Path $planPath)) {
  Write-Output 'FAIL PLAN.md 不存在'
  exit 1
}
$plan = Get-Content $planPath -Raw -Encoding UTF8

foreach ($need in @('当前里程碑', '当前任务', '最近更新')) {
  if ($plan -notmatch [regex]::Escape($need)) {
    Write-Output "FAIL PLAN.md 缺少「$need」"
    $fail++
  } else {
    Write-Output "ok   PLAN.md 含「$need」"
  }
}

Write-Output ''
Write-Output '=== 2) 定位当前任务 ==='
if (-not $TaskId) {
    # ⚠️ 2026-11-12 修（真实漏洞）：原来抓全文第一个 T-M\d+-\d{2}，
    # 而 PLAN 的「命令速查」里有 `check-plan.ps1 -TaskId T-M1-00`
    # -> 永远解析成 T-M1-00，门禁一直在校验错误的任务。
    # 改为：只抓「当前任务」那一行里的任务 id。
    $m = [regex]::Match($plan, '(?m)^\|[^\r\n]*当前任务[^\r\n]*?(T-M\d+-\d{2})')
    if ($m.Success) {
        $TaskId = $m.Groups[1].Value
    } else {
        $m2 = [regex]::Match($plan, '当前任务[^\r\n]{0,60}?(T-M\d+-\d{2})')
        if ($m2.Success) { $TaskId = $m2.Groups[1].Value }
    }
}
if (-not $TaskId) {
  # T-M9-01 补：允许"全部里程碑完成、无进行中任务"的合法状态——
  # 但**必须**同时满足：PLAN 的当前任务写成「—」，且 docs/05 已经 0 条未勾。
  # 加这个双条件是为了防止有人用「—」把门禁绕过去（那样就等于没有门禁）。
  $planDone = $plan -match '(?m)^\|[^\r\n]*当前任务[^\r\n]*—'
  $doc5 = Join-Path $root 'docs\05-研发路线图与验收标准.md'
  $docTodo = 0
  if (Test-Path $doc5) {
    $docTodo = (Select-String -Path $doc5 -Pattern '^\s*- \[ \]' -ErrorAction SilentlyContinue | Measure-Object).Count
  }
  if ($planDone -and $docTodo -eq 0) {
    Write-Output 'ok   当前无进行中任务（PLAN 标「—」，且 docs/05 未勾 = 0）—— 全部里程碑完成'
    Write-Output ''
    Write-Output 'CHECK-PLAN: PASS —— 无进行中任务（可开工前先立项）'
    exit 0
  }
  Write-Output 'FAIL 无法从 PLAN.md 解析出当前任务 id（形如 T-M1-00）'
  Write-Output ("     若确实「全部完成」：当前任务写「—」且 docs/05 未勾必须为 0（当前 {0}）" -f $docTodo)
  $fail++
} else {
  Write-Output "当前任务 = $TaskId"
}

Write-Output ''
Write-Output '=== 3) 任务卡与任务段必填节 ==='
$required = @('需求', '必读文档', '技术方案', '功能点', '验收', '风险', '待确认问题')

$cardPath = $null
if ($TaskId) {
  $hits = Select-String -Path (Join-Path $root 'plans\*.md') -Pattern ("task_id:\s*" + [regex]::Escape($TaskId)) -ErrorAction SilentlyContinue
  if ($hits) { $cardPath = $hits[0].Path }
}

if (-not $cardPath) {
  Write-Output "FAIL 在 plans\ 下找不到 task_id = $TaskId 的任务卡"
  $fail++
} else {
  $cardName = [System.IO.Path]::GetFileName($cardPath)
  Write-Output "ok   任务卡: $cardName"
  $card = Get-Content $cardPath -Raw -Encoding UTF8

  # 截取该任务所在段落：从 `## <taskid>` 到下一个 `## T-M`
  $segRx = '(?s)##\s*' + [regex]::Escape($TaskId) + '.*?(?=##\s*T-M|\z)'
  $segM = [regex]::Match($card, $segRx)
  if (-not $segM.Success) {
    Write-Output "FAIL 任务卡内找不到 `## $TaskId` 段落标题"
    $fail++
    $taskSeg = $card
  } else {
    $taskSeg = $segM.Value
  }

  foreach ($kw in $required) {
    if ($taskSeg -notmatch [regex]::Escape($kw)) {
      Write-Output "FAIL 任务段缺少「$kw」"
      $fail++
    } else {
      Write-Output "ok   含「$kw」"
    }
  }

  if ($card -notmatch [regex]::Escape('执行记录')) {
    Write-Output 'FAIL 任务卡文件缺少「执行记录」节（收工要追加）'
    $fail++
  } else {
    Write-Output 'ok   文件含「执行记录」节'
  }

  Write-Output ''
  Write-Output '=== 4) 待确认问题必须清空 ==='
  $seg2 = [regex]::Match($taskSeg, '(?s)待确认问题(.*?)(执行记录|\z)')
  if ($seg2.Success) {
    $body = $seg2.Groups[1].Value
    $pending = @()
    foreach ($line in ($body -split "`n")) {
      # ⚠️ 2026-11-12 修（假阳性）：`**待确认问题**：无（...）` 这种"已清空"的表述
        # 原来只要不含"已确认"等词就被算成未决。补上 "：无 / ：空 / 无（" 也算清空。
        if ($line -match '待确认' -and $line -notmatch '已确认|已调研|无需确认|状态|：\s*无|：\s*空|:\s*无') { $pending += $line.Trim() }
    }
    if ($pending.Count -gt 0) {
      Write-Output "FAIL 仍有 $($pending.Count) 条未决问题（不许带着猜测开工）："
      $pending | ForEach-Object { Write-Output "     $_" }
      $fail++
    } else {
      Write-Output 'ok   无未决问题'
    }
  } else {
    Write-Output 'FAIL 无法解析「待确认问题」段'
    $fail++
  }
}

Write-Output ''
Write-Output '=== 5) 进度更新时效（>7 天 WARN，不阻断） ==='
$dm = [regex]::Match($plan, '最近更新\s*\|\s*(\d{4}-\d{2}-\d{2})')
if ($dm.Success) {
  $last = [datetime]::ParseExact($dm.Groups[1].Value, 'yyyy-MM-dd', $null)
  $days = ([datetime]::Today - $last).Days
  if ($days -gt 7) { Write-Output "WARN PLAN.md 已 $days 天未更新（请更新「当前进度」）" }
  else { Write-Output "ok   最近更新距今 $days 天" }
} else {
  Write-Output 'WARN 未能在 PLAN.md 找到「最近更新 YYYY-MM-DD」'
}

Write-Output ''
if (-not $SkipDocs) {
  Write-Output '=== 6) 串跑文档闸门 ==='
  $docsScript = Join-Path $root 'scripts\check-docs.ps1'
  if (Test-Path $docsScript) {
    & $docsScript | Select-Object -Last 2
    if ($LASTEXITCODE -ne 0) { Write-Output 'FAIL check-docs 未通过'; $fail++ }
    else { Write-Output 'ok   check-docs 通过' }
  } else {
    Write-Output 'WARN 未找到 check-docs.ps1'
  }
  Write-Output ''
}


# === 2.5) 当前任务不应是已完成状态（2026-11-12 补：门禁漏洞） ===
# 真实拦截案例：PLAN 的「当前任务」曾长期停在 T-M1-06（早已 done），
# 而 check-plan 只校验"任务卡存在"，不校验状态 -> 门禁 PASS 但计划是错的。
if ($TaskId) {
    $cardPath = Get-ChildItem (Join-Path $root 'plans') -Filter '*.md' -File |
        Where-Object { (Get-Content $_.FullName -Raw -Encoding UTF8) -match [regex]::Escape($TaskId) } |
        Select-Object -First 1
    if ($cardPath) {
        $cardText = Get-Content $cardPath.FullName -Raw -Encoding UTF8
        $m = [regex]::Match($cardText, 'task_id:\s*' + [regex]::Escape($TaskId) + '\s*\r?\nstatus:\s*(\w+)')
        if ($m.Success) {
            $st = $m.Groups[1].Value
            if ($st -eq 'done') {
                Write-Output "FAIL 当前任务 $TaskId 的状态是 done —— 已完成的任务不应作为「当前任务」，请更新 PLAN.md"
                $fail++
            } else {
                Write-Output "ok   当前任务 $TaskId 状态=$st"
            }
        }
    }
}

if ($fail -gt 0) {
  Write-Output "CHECK-PLAN: FAIL ($fail 项) —— 不许开工"
  exit 1
} else {
  Write-Output 'CHECK-PLAN: PASS —— 可以开工'
  exit 0
}
