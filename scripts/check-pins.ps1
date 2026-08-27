# 校验 Cargo.lock 里的上游 sha 与立项文档 § 二 钉死的一致（check-pins.sh 的 Windows 版）。
$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
$Lock = Join-Path $Root "Cargo.lock"

$ZedSha   = "cc053a4a6fa2fd0e8793201ed9099466af1be0b1"
$GpuicSha = "000114aad412b1a1b26cb65cd0c8ae9467fd396a"

if (-not (Test-Path $Lock)) { throw "没有 Cargo.lock（先跑一次 cargo generate-lockfile）" }
$content = Get-Content $Lock -Raw

$fail = $false
function Check($label, $needle) {
    if ($script:content.Contains($needle)) { Write-Host "OK   $label" }
    else { Write-Error "FAIL $label —— Cargo.lock 里找不到 $needle" -ErrorAction Continue; $script:fail = $true }
}

# 只检查"钉的 sha 在场"是不够的：cargo update 会把同一个 git 源里的一部分包
# 挪到新 sha、留一部分在旧 sha，形成半新半旧的混合锁 —— R0 实测踩过。
function CheckNoStray($label, $url, $want) {
    $found = [regex]::Matches($script:content, "git\+$([regex]::Escape($url))#([0-9a-f]{40})") |
             ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    $stray = $found | Where-Object { $_ -ne $want }
    if (-not $stray) { Write-Host "OK   $label 无杂散 sha" }
    else {
        Write-Error "FAIL $label 出现了别的 sha：$($stray -join ', ')" -ErrorAction Continue
        $script:fail = $true
    }
}

Check "zed (gpui / gpui_platform)" "git+https://github.com/zed-industries/zed#$ZedSha"
Check "gpui-component"             "git+https://github.com/longbridge/gpui-component#$GpuicSha"
CheckNoStray "zed"            "https://github.com/zed-industries/zed"        $ZedSha
CheckNoStray "gpui-component" "https://github.com/longbridge/gpui-component" $GpuicSha

$global:LASTEXITCODE = 0
& "$Root\scripts\check-pi-source-pin.ps1"
if ($LASTEXITCODE -ne 0) { $fail = $true }

$global:LASTEXITCODE = 0
& "$Root\scripts\check-pi-web-pin.ps1"
if ($LASTEXITCODE -ne 0) { $fail = $true }

# R26 起 vendor 里多了一份**运行时**依赖（子代理执行内核），同样按 manifest 全量判红。
$global:LASTEXITCODE = 0
& "$Root\scripts\check-pi-subagents-lite-pin.ps1"
if ($LASTEXITCODE -ne 0) { $fail = $true }

# 成功也要显式 exit 0：调用方（validate.ps1）靠 $LASTEXITCODE 判断，
# 而 .ps1 正常落地时并不会写这个变量。
if ($fail) { exit 1 } else { exit 0 }
