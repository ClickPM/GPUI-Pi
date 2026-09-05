# 打 Windows 绿色包：解压即用，免安装。
#
#   .\scripts\package.ps1
#   .\scripts\package.ps1 -SkipBuild   # 已有 target\release\gpui-pi.exe 时只组装
#
# 产物（gitignored 的 dist/）：
#   dist\gpui-pi-<ver>-windows-x64\gpui-pi.exe
#   dist\gpui-pi-<ver>-windows-x64\vendor\pi\
#   dist\gpui-pi-<ver>-windows-x64\vendor\pi-subagents-lite-1.13.0\
#   dist\gpui-pi-<ver>-windows-x64.zip
#
# 不收录：vendor\upstream（只读对照源码）、runtime_fake_child.exe、pdb。
param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$MaxMb = 220
$PinnedPi = "0.84.2"
$PinnedKernel = "pi-subagents-lite-1.13.0"

function Read-WorkspaceVersion([string]$CargoToml) {
    $text = Get-Content -LiteralPath $CargoToml -Raw
    $match = [regex]::Match($text, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { throw "无法从 Cargo.toml 读取 version" }
    return $match.Groups[1].Value
}

function Invoke-Step([string]$Name, [scriptblock]$Block) {
    Write-Host "==> $Name"
    $global:LASTEXITCODE = 0
    & $Block
    if ($LASTEXITCODE -ne 0) { throw "$Name 失败（exit $LASTEXITCODE）" }
}

function Copy-Tree([string]$From, [string]$To) {
    if (-not (Test-Path -LiteralPath $From)) { throw "缺少 $From" }
    New-Item -ItemType Directory -Path (Split-Path -Parent $To) -Force | Out-Null
    if (Test-Path -LiteralPath $To) { Remove-Item -LiteralPath $To -Recurse -Force }
    Copy-Item -LiteralPath $From -Destination $To -Recurse -Force
}

function Get-TreeMb([string]$Path) {
    $sum = (Get-ChildItem -LiteralPath $Path -Recurse -File -ErrorAction SilentlyContinue |
            Measure-Object Length -Sum).Sum
    if (-not $sum) { return 0 }
    return [math]::Round($sum / 1MB, 1)
}

function Write-PortableReadme([string]$Path, [string]$Version) {
    # 终端用户说明，不进仓库源码树。
    $body = @"
GPUI-Pi $Version  （Windows x64 绿色包）

免安装：解压本目录到任意位置，双击 gpui-pi.exe。
不写注册表、不装系统服务、不依赖 WebView2 / Node / Python。

需要：
  - 64 位 Windows 10/11
  - 模型 API Key（在应用内「模型与认证设置」里配置；与终端 pi 共用 %USERPROFILE%\.pi）

不要：
  - 只拷贝 gpui-pi.exe。必须保留旁边的 vendor\pi 与 vendor\$PinnedKernel
  - 把本目录链到别的 worktree / 共享 vendor（目录链接在 git worktree remove 时有误删风险）

本应用不做自动更新。新版本到 GitHub Releases 下载新的绿色包，覆盖本目录即可。
"@
    Set-Content -LiteralPath $Path -Value $body -Encoding utf8
}

Push-Location $Root
try {
    $Version = Read-WorkspaceVersion (Join-Path $Root "Cargo.toml")
    $ArchName = "windows-x64"
    $FolderName = "gpui-pi-$Version-$ArchName"
    $OutDir = Join-Path $Root "dist\$FolderName"
    $ZipPath = Join-Path $Root "dist\$FolderName.zip"
    $TargetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }
    $ExeSrc = Join-Path $TargetRoot "release\gpui-pi.exe"
    $PiSrc = Join-Path $Root "vendor\pi"
    $KernelSrc = Join-Path $Root "vendor\$PinnedKernel"
    $FakeChild = Join-Path $TargetRoot "release\runtime_fake_child.exe"

    if (-not (Test-Path -LiteralPath (Join-Path $PiSrc "pi.exe"))) {
        throw "缺少 vendor\pi\pi.exe，先跑 .\scripts\fetch-pi.ps1"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $KernelSrc "package.json"))) {
        throw "缺少 vendor\$PinnedKernel，先跑 .\scripts\fetch-pi-subagents-lite.ps1"
    }

    if (-not $SkipBuild) {
        Invoke-Step "cargo build --release -p gpui-pi" {
            cargo build --release -p gpui-pi
        }
    }
    if (-not (Test-Path -LiteralPath $ExeSrc)) {
        throw "没有 $ExeSrc（去掉 -SkipBuild 或先 cargo build --release -p gpui-pi）"
    }

    Write-Host "==> Assemble $OutDir"
    if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force }
    New-Item -ItemType Directory -Path $OutDir | Out-Null
    Copy-Item -LiteralPath $ExeSrc -Destination (Join-Path $OutDir "gpui-pi.exe")
    Copy-Tree $PiSrc (Join-Path $OutDir "vendor\pi")
    Copy-Tree $KernelSrc (Join-Path $OutDir "vendor\$PinnedKernel")
    Write-PortableReadme (Join-Path $OutDir "使用说明.txt") $Version

    if (Test-Path -LiteralPath (Join-Path $OutDir "runtime_fake_child.exe")) {
        throw "绿色包误收录了 runtime_fake_child.exe"
    }
    if (Test-Path -LiteralPath (Join-Path $OutDir "vendor\upstream")) {
        throw "绿色包误收录了 vendor\upstream"
    }
    Get-ChildItem -LiteralPath $OutDir -Filter *.pdb -Recurse -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force }

    Write-Host "==> Self check"
    $bundledPi = Join-Path $OutDir "vendor\pi\pi.exe"
    $gotVer = (& $bundledPi --version).Trim()
    if ($gotVer -ne $PinnedPi) { throw "pi 版本不符：expected $PinnedPi, got $gotVer" }
    if (-not (Test-Path -LiteralPath (Join-Path $OutDir "vendor\$PinnedKernel\package.json"))) {
        throw "子代理内核 package.json 未拷到绿色包"
    }
    if (-not (Test-Path -LiteralPath (Join-Path $OutDir "gpui-pi.exe"))) {
        throw "gpui-pi.exe 未拷到绿色包"
    }
    if (Test-Path -LiteralPath $FakeChild) {
        Write-Host "note  构建树里有 runtime_fake_child.exe，已确认未打进绿色包"
    }

    $folderMb = Get-TreeMb $OutDir
    Write-Host "OK   folder $folderMb MB (limit $MaxMb MB)"
    if ($folderMb -gt $MaxMb) { throw "绿色包目录 $folderMb MB 超过立项上限 ${MaxMb}MB" }

    Write-Host "==> Zip"
    New-Item -ItemType Directory -Path (Join-Path $Root "dist") -Force | Out-Null
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    Compress-Archive -Path $OutDir -DestinationPath $ZipPath -CompressionLevel Optimal
    $zipMb = [math]::Round((Get-Item -LiteralPath $ZipPath).Length / 1MB, 1)
    Write-Host "OK   zip    $zipMb MB  $ZipPath"
    if ($zipMb -gt $MaxMb) { throw "zip $zipMb MB 超过立项上限 ${MaxMb}MB" }

    Write-Host ""
    Write-Host "PACKAGE OK  $OutDir"
    Write-Host "免安装：把该目录拷到目标机器，双击 gpui-pi.exe"
}
finally {
    Pop-Location
}

exit 0
