# T1 静态验收（validate.sh 的 Windows 版）—— 每一轮 /loop 迭代结束都必须全绿。
#
#   .\scripts\validate.ps1          全量
#   .\scripts\validate.ps1 -Logic   只跑四个纯逻辑 crate
param([switch]$Logic)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Push-Location $Root

function Step($n, $name, $block) {
    Write-Host "### [$n/5] $name"
    # 每步前手动清零：PowerShell 只有在跑过「外部程序」之后才会写 $LASTEXITCODE，
    # 调 .ps1 且它正常结束时这个变量保持上一次的值、首次调用时干脆是空的 ——
    # 空值 -ne 0 为真，会把成功的一步判成失败（R0 的 CI 就是这么红的）。
    $global:LASTEXITCODE = 0
    & $block
    # Pop-Location 交给外层 finally，这里再 pop 一次会把位置栈弹空。
    if ($LASTEXITCODE -ne 0) { throw "$name 失败（exit $LASTEXITCODE）" }
}

try {
    if ($Logic) {
        $scope = @("-p", "pi-rpc", "-p", "pi-data", "-p", "pi-render", "-p", "pi-runtime")
        Write-Host "### 范围：仅纯逻辑 crate（pi-rpc / pi-data / pi-render / pi-runtime）"
    } else {
        $scope = @("--workspace")
        Write-Host "### 范围：全工作区（含 gpui / gpui-component 编译）"
    }

    Step 1 "上游钉版本" { & "$Root\scripts\check-pins.ps1" }
    Step 2 "cargo fmt"  { cargo fmt --all -- --check }
    Step 3 "cargo clippy" { cargo clippy @scope --all-targets -- -D warnings }
    Step 4 "cargo test"   { cargo test @scope }
    Step 5 "cargo build --release" { cargo build --release @scope }

    Write-Host ""
    Write-Host "VALIDATE OK"
}
finally { Pop-Location }
