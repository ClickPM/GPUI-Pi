# Fetch the pinned pi-subagents-lite subagent kernel into vendor\.
#
# 与 fetch-pi-source.ps1 / fetch-pi-web.ps1 同一套流程（下载 -> 校验 -> 解包 -> 写 marker
# -> 全量比对 -> 发布），但有两点不同：
#   - 这是**运行时依赖**，不是只读参考源码：pi 以 `-e <本目录>` 加载它；
#   - 它有唯一一条运行时依赖 @sinclair/typebox，这里直接下载并解包该包的 tarball，
#     **不调用 npm install**。npm 会按 caret 区间解析 typebox（版本会漂），且 npm 7+ 默认
#     连 peerDependencies 一起装（实测多装 90 个包 / 75MB @earendil-works/* 等，而那些
#     必须由 pi 二进制自身提供）。直接解包两个钉死 tarball 才是确定性的。
#
# 本机缓存：默认 D:\tmp\gpui-pi-cache，可用 GPUI_PI_CACHE 覆盖路径、设 OFF 禁用（CI 已禁用）。
# 命中缓存时只做本地校验 + 拷贝，不联网；缓存缺失/损坏才联网拉取并在缓存目录内覆盖更新。
$ErrorActionPreference = "Stop"

$PkgName       = "pi-subagents-lite"
$PkgVersion    = "1.13.0"
$PkgSha512     = "sha512-g6kyfhC762ju1Esr7/6O3WQKrSXPtEsACC+O9UK1jLQ2d5gFTZU1F39mB8AkpuyOMHgVemZVqr4rXbmycr1fIw=="
$PkgUrl        = "https://registry.npmjs.org/$PkgName/-/$PkgName-$PkgVersion.tgz"
$TypeboxVer    = "0.34.52"
$TypeboxSha512 = "sha512-XiMQh7qqVlxZzcVD+kkGMNGMzcTrDMLWI7S4x7z1MkCkbDPrekpZXEUK0eZqZFMuHQg2a2DZOcDIh9o5v3Gonw=="
$TypeboxUrl    = "https://registry.npmjs.org/@sinclair/typebox/-/typebox-$TypeboxVer.tgz"
$Root          = Split-Path -Parent $PSScriptRoot
$Dest          = Join-Path $Root "vendor\$PkgName-$PkgVersion"
$Check         = Join-Path $Root "scripts\check-pi-subagents-lite-pin.ps1"
. (Join-Path $Root "scripts\pi-cache-utils.ps1")

# npm 的 integrity 是 base64 编码的摘要，不是 Get-FileHash 的十六进制串，这里直接算 base64。
function Get-Sha512Integrity([string]$Path) {
    $sha = [System.Security.Cryptography.SHA512]::Create()
    $fs = [System.IO.File]::OpenRead($Path)
    try { $digest = $sha.ComputeHash($fs) } finally { $fs.Dispose() }
    return "sha512-" + [Convert]::ToBase64String($digest)
}

# Git Bash 会把自己的 GNU tar.exe 排到 PATH 前面，并把 D:\... 误判成 remote:file。
function Get-SystemTar {
    $Candidates = @(
        (Join-Path $env:SystemRoot "Sysnative\tar.exe")
        (Join-Path $env:SystemRoot "System32\tar.exe")
    )
    $Tar = $Candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if (-not $Tar) { throw "Windows system tar.exe not found" }
    return $Tar
}

# 下载一个 npm tarball、校验 integrity、解包，并返回其中 package\ 目录的路径。
function Expand-NpmTarball([string]$Url, [string]$ExpectedSha512, [string]$TmpRoot, [string]$Tag) {
    $Archive = Join-Path $TmpRoot "$Tag.tgz"
    $Extract = Join-Path $TmpRoot "$Tag-extract"
    New-Item -ItemType Directory -Path $Extract | Out-Null

    Write-Host "==> Download $Tag ($Url)"
    Invoke-WebRequest -Uri $Url -OutFile $Archive -UseBasicParsing

    Write-Host "==> Verify $Tag integrity"
    $Got = Get-Sha512Integrity $Archive
    if ($Got -ne $ExpectedSha512) {
        throw "$Tag integrity mismatch: expected $ExpectedSha512, got $Got"
    }

    $Tar = Get-SystemTar
    & $Tar -xzf $Archive -C $Extract
    if ($LASTEXITCODE -ne 0) { throw "Windows system tar.exe failed with exit $LASTEXITCODE" }

    $PackageRoot = Join-Path $Extract "package"
    if (-not (Test-Path -LiteralPath $PackageRoot -PathType Container)) {
        throw "Unexpected npm tarball layout for ${Tag}: expected a package\ root"
    }
    return $PackageRoot
}

# 下载 + 校验 + 解包两个 tarball + 写 marker + 全量比对，全部就绪才返回内容根目录。
$Download = {
    param([string]$TmpRoot)

    $PkgRoot = Expand-NpmTarball $PkgUrl $PkgSha512 $TmpRoot "$PkgName-$PkgVersion"
    $TbRoot  = Expand-NpmTarball $TypeboxUrl $TypeboxSha512 $TmpRoot "typebox-$TypeboxVer"

    Write-Host "==> Place @sinclair/typebox as the only vendored node_modules entry"
    $TbDest = Join-Path $PkgRoot "node_modules\@sinclair\typebox"
    New-Item -ItemType Directory -Path (Split-Path -Parent $TbDest) -Force | Out-Null
    Move-Item -LiteralPath $TbRoot -Destination $TbDest

    Write-Host "==> Write pin marker"
    @(
        "version=$PkgVersion"
        "package_sha512=$PkgSha512"
        "typebox_version=$TypeboxVer"
        "typebox_sha512=$TypeboxSha512"
        "source=$PkgUrl"
    ) | Set-Content -LiteralPath (Join-Path $PkgRoot ".gpui-pi-subagents-lite-pin") -Encoding ascii

    Write-Host "==> Full verification against baseline manifest"
    & $Check -Dir $PkgRoot
    if ($LASTEXITCODE -ne 0) { throw "Pinned kernel verification failed" }

    return $PkgRoot
}

# 1) vendor 快路径：已存在且与基线逐字节一致，直接收工（不联网、不碰缓存）。
if (Test-Path -LiteralPath $Dest -PathType Container) {
    & $Check -Dir $Dest
    if ($LASTEXITCODE -eq 0) {
        Write-Host "OK  vendor\$PkgName-$PkgVersion already exists and matches baseline"
        exit 0
    }
    Write-Warning "vendor\$PkgName-$PkgVersion failed verification; will re-publish from cache or network"
}

# 把已校验的目录拷贝成 vendor 目录并复验。vendor 内是真实拷贝，不建任何链接（红线 6）。
function Publish-KernelToVendor([string]$Source) {
    Write-Host "==> Publish to vendor\$PkgName-$PkgVersion"
    if (Test-Path -LiteralPath $Dest) { Remove-Item -LiteralPath $Dest -Recurse -Force }
    New-Item -ItemType Directory -Path (Split-Path -Parent $Dest) -Force | Out-Null
    Copy-Item -LiteralPath $Source -Destination $Dest -Recurse
    & $Check -Dir $Dest
    if ($LASTEXITCODE -ne 0) { throw "published vendor tree failed verification: $Dest" }
}

# 2) 缓存路径：命中则本机拷贝；缺失/损坏则联网刷新缓存（在缓存目录内覆盖）。
$CacheRoot = Get-PiCacheRoot
if ($CacheRoot) {
    $CacheItem = Ensure-PiCacheItem -CacheRoot $CacheRoot -Name "$PkgName-$PkgVersion" `
        -CheckScript $Check -Download $Download
    Write-Host "==> cache hit; publishing from $CacheItem"
    Publish-KernelToVendor -Source $CacheItem
    Write-Host "OK  vendor\$PkgName-$PkgVersion ($PkgVersion + typebox $TypeboxVer)"
    exit 0
}

# 3) 兜底（缓存不可用）：临时目录建在目标父目录下、同卷，发布走同一个「拷贝 + 复验」函数。
$VendorRoot = Split-Path -Parent $Dest
New-Item -ItemType Directory -Path $VendorRoot -Force | Out-Null
$TmpRoot = Join-Path $VendorRoot (".fetch-tmp-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $TmpRoot | Out-Null
try {
    $Verified = & $Download $TmpRoot
    Publish-KernelToVendor -Source $Verified
    Write-Host "OK  vendor\$PkgName-$PkgVersion ($PkgVersion + typebox $TypeboxVer)"
}
finally {
    Remove-Item -LiteralPath $TmpRoot -Recurse -Force -ErrorAction SilentlyContinue
}
