# Verify the vendored pi-subagents-lite subagent kernel matches the checked-in manifest.
# Used by check-pins.ps1 and fetch-pi-subagents-lite.ps1.
# Usage: check-pi-subagents-lite-pin.ps1 [-Dir <path>]
#
# 与 pi / pi-web 两份上游参考源码不同，本目录是**运行时依赖**：pi 会以
# `-e <本目录>` 加载它，node 从 node_modules 解析 @sinclair/typebox。因此除了
# 全量 manifest 比对，还要额外确认 node_modules 里没有混进 peerDependencies ——
# 不加 --legacy-peer-deps 的 npm install 会把 @earendil-works/* 等 90 个包装进来
# （实测 75MB），那些包必须由 pi 二进制自身提供，vendor 里出现即视为污染。
param([string]$Dir)

$ErrorActionPreference = "Stop"

$PkgName       = "pi-subagents-lite"
$PkgVersion    = "1.13.0"
$PkgSha512     = "sha512-g6kyfhC762ju1Esr7/6O3WQKrSXPtEsACC+O9UK1jLQ2d5gFTZU1F39mB8AkpuyOMHgVemZVqr4rXbmycr1fIw=="
$PkgUrl        = "https://registry.npmjs.org/$PkgName/-/$PkgName-$PkgVersion.tgz"
$TypeboxVer    = "0.34.52"
$TypeboxSha512 = "sha512-XiMQh7qqVlxZzcVD+kkGMNGMzcTrDMLWI7S4x7z1MkCkbDPrekpZXEUK0eZqZFMuHQg2a2DZOcDIh9o5v3Gonw=="
$Root          = Split-Path -Parent $PSScriptRoot
if (-not $Dir) { $Dir = Join-Path $Root "vendor\$PkgName-$PkgVersion" }
$Marker        = Join-Path $Dir ".gpui-pi-subagents-lite-pin"
$Manifest      = Join-Path $Root "pins\$PkgName-$PkgVersion.manifest"

$fail = $false
function Fail([string]$Message) {
    Write-Error "FAIL $Message" -ErrorAction Continue
    $script:fail = $true
}

if (-not (Test-Path -LiteralPath $Manifest -PathType Leaf)) {
    throw "Manifest baseline is missing: $Manifest"
}

if (-not (Test-Path -LiteralPath $Dir -PathType Container)) {
    Fail "pi-subagents-lite is not prepared; run .\scripts\fetch-pi-subagents-lite.ps1"
    exit 1
}

if (Test-Path -LiteralPath (Join-Path $Dir ".git")) {
    Fail "vendored kernel contains .git and may drift: $Dir"
} else {
    Write-Host "OK   pi-subagents-lite directory has no .git"
}

# node_modules 只允许 @sinclair/typebox 这一棵子树；多出任何东西都说明装进了 peerDependencies。
$NodeModules = Join-Path $Dir "node_modules"
if (Test-Path -LiteralPath $NodeModules -PathType Container) {
    $TopLevel = @(Get-ChildItem -LiteralPath $NodeModules -Force | ForEach-Object { $_.Name } | Sort-Object)
    if ($TopLevel.Count -ne 1 -or $TopLevel[0] -ne "@sinclair") {
        Fail "node_modules must contain exactly @sinclair (got: $($TopLevel -join ', ')); peerDependencies must come from the pi binary, not vendor"
    } else {
        $Scoped = @(Get-ChildItem -LiteralPath (Join-Path $NodeModules "@sinclair") -Force | ForEach-Object { $_.Name } | Sort-Object)
        if ($Scoped.Count -ne 1 -or $Scoped[0] -ne "typebox") {
            Fail "node_modules\@sinclair must contain exactly typebox (got: $($Scoped -join ', '))"
        } else {
            Write-Host "OK   node_modules holds only @sinclair/typebox"
        }
    }
} else {
    Fail "node_modules\@sinclair\typebox is missing; the extension cannot resolve its only runtime dependency"
}

$ExpectedPairs = @(
    "version=$PkgVersion"
    "package_sha512=$PkgSha512"
    "typebox_version=$TypeboxVer"
    "typebox_sha512=$TypeboxSha512"
    "source=$PkgUrl"
)
if (Test-Path -LiteralPath $Marker -PathType Leaf) {
    $MarkerLines = Get-Content -LiteralPath $Marker | ForEach-Object { $_.TrimEnd("`r") } |
                   Where-Object { $_ -ne "" }
    if ($MarkerLines.Count -ne 5) {
        Fail "marker line count is not 5 (got $($MarkerLines.Count)): $Marker"
    }
    foreach ($Expected in $ExpectedPairs) {
        if ($MarkerLines -notcontains $Expected) {
            Fail "marker is missing or mismatched: $Expected"
        }
    }
    Write-Host "OK   pi-subagents-lite marker (version/package_sha512/typebox_version/typebox_sha512/source)"
} else {
    Fail "vendored kernel is missing its pin marker: $Marker"
}

# vendor 树里最长的相对路径来自 typebox 的 build\esm\type\constructor-parameters\（97 字符）。
# 加上仓库前缀后逼近 MAX_PATH，而 Windows PowerShell 5.1 跑在 .NET Framework 上：
# Directory.EnumerateFiles 直接拒收 \?\ 扩展长度路径（"Illegal characters in path"），
# 所以只能提前把超限情况判出来，而不是靠扩展路径绕过。
$MaxPathLimit = 259

function Get-VendorManifest([string]$SourceDir) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $items = New-Object System.Collections.Generic.List[object]
    $FullRoot = [System.IO.Path]::GetFullPath($SourceDir)
    foreach ($file in [System.IO.Directory]::EnumerateFiles($FullRoot, '*', 'AllDirectories')) {
        if ([System.IO.Path]::GetFileName($file) -eq '.gpui-pi-subagents-lite-pin') { continue }
        $rel = $file.Substring($FullRoot.Length + 1).Replace('\', '/')
        if ($file.Length -gt $MaxPathLimit) {
            throw "vendored path exceeds MAX_PATH ($($file.Length) > $MaxPathLimit): $rel -- move the checkout closer to the drive root"
        }
        # 读不动就必须炸，不能跳过：静默跳过会生成一份"文件更少但每行都对"的假清单，
        # 逐行比对只报 count mismatch，看不出到底是采集失败还是内容真的少了。
        # （这条护栏是 R26 实测踩出来的 —— 深路径下第一版 manifest 被静默截断成 62 个文件。）
        try {
            $fs = [System.IO.File]::OpenRead($file)
        } catch {
            throw "cannot read vendored file (manifest would be silently truncated): $rel -- $($_.Exception.Message)"
        }
        try {
            $hash = [BitConverter]::ToString($sha.ComputeHash($fs)).Replace('-', '').ToLower()
            $len = $fs.Length
        } finally { $fs.Dispose() }
        $items.Add([pscustomobject]@{ Path = $rel; Line = "$hash  $len  $rel" })
    }
    if ($items.Count -eq 0) {
        throw "vendored kernel enumerated zero files: $SourceDir"
    }
    # Baseline is sorted by path in byte order (LC_ALL=C); Ordinal matches that for ASCII paths.
    $items.Sort([System.Comparison[object]]{ param($a, $b) [System.StringComparer]::Ordinal.Compare($a.Path, $b.Path) })
    return ,($items | ForEach-Object { $_.Line })
}

$Current = Get-VendorManifest $Dir
$Baseline = Get-Content -LiteralPath $Manifest | ForEach-Object { $_.TrimEnd("`r") }
if ($Current.Count -ne $Baseline.Count) {
    Fail "file count mismatch: current $($Current.Count) vs baseline $($Baseline.Count)"
} else {
    $mismatch = 0
    for ($i = 0; $i -lt $Current.Count; $i++) {
        if ($Current[$i] -ne $Baseline[$i]) {
            if ($mismatch -lt 20) {
                Write-Error "      baseline: $($Baseline[$i])" -ErrorAction Continue
                Write-Error "      current : $($Current[$i])" -ErrorAction Continue
            }
            $mismatch++
        }
    }
    if ($mismatch -eq 0) {
        Write-Host "OK   pi-subagents-lite content matches baseline manifest ($($Current.Count) files)"
    } else {
        Fail "pi-subagents-lite content differs from baseline manifest ($mismatch files)"
    }
}

if ($fail) { exit 1 } else { exit 0 }
