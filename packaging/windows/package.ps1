#Requires -Version 5.1
<#
.SYNOPSIS
    打包 Windows 产物：zip 便携包 + msi 安装包。

.DESCRIPTION
    由 .github/workflows/build.yml 调用；也可以在本机直接跑：

        pwsh packaging/windows/package.ps1 -Version 0.1.0 -Target x86_64-pc-windows-msvc

    产物写入 <OutDir>：
        TianyiCloud-<Version>-<ArchName>.zip   exe + LICENSE + README
        TianyiCloud-<Version>-<ArchName>.msi   WiX v5 生成的安装包

    MSI 需要的 WiX 通过 `dotnet tool install --global wix` 按需安装（GitHub 的
    windows-2022 自带 WiX 3，但 arm64 镜像没有，所以统一用 WiX v5）。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Target,
    [string]$MsiVersion = '',
    [ValidateSet('x64', 'arm64')][string]$WixArch = 'x64',
    [string]$ArchName = 'windows-x64',
    [string]$OutDir = 'dist'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot) # <repo>/packaging/windows -> <repo>
$Exe = Join-Path $Root "target/$Target/release/tianyi-app.exe"
if (-not (Test-Path $Exe)) { throw "找不到编译产物: $Exe" }
if (-not $MsiVersion) { $MsiVersion = $Version }

$OutPath = Join-Path $Root $OutDir
New-Item -ItemType Directory -Force -Path $OutPath | Out-Null

function Get-PythonCommand {
    foreach ($candidate in @('python', 'python3')) {
        $cmd = Get-Command $candidate -ErrorAction SilentlyContinue
        if ($cmd) { return $cmd.Source }
    }
    throw '找不到 python（生成图标需要）'
}

# ---------------------------------------------------------------- 图标（占位）
$IconDir = Join-Path $Root 'target/package-icons'
$Python = Get-PythonCommand
& $Python (Join-Path $Root 'packaging/ci/make-icon.py') $IconDir
if ($LASTEXITCODE -ne 0) { throw '图标生成失败' }
$Icon = Join-Path $IconDir 'icon.ico'

# ---------------------------------------------------------------- zip 便携包
$StageDir = Join-Path $Root "target/package-stage/$ArchName"
if (Test-Path $StageDir) { Remove-Item -Recurse -Force $StageDir }
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null
Copy-Item $Exe (Join-Path $StageDir 'tianyi-app.exe')
Copy-Item (Join-Path $Root 'LICENSE') $StageDir
Copy-Item (Join-Path $Root 'README.md') $StageDir

$ZipPath = Join-Path $OutPath "TianyiCloud-$Version-$ArchName.zip"
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
Compress-Archive -Path (Join-Path $StageDir '*') -DestinationPath $ZipPath -CompressionLevel Optimal
Write-Host "[package] zip -> $ZipPath"

# ---------------------------------------------------------------- msi 安装包
function Install-WixTool {
    param([string]$DotnetExe, [string]$ToolsDir)

    $wixExe = Join-Path $ToolsDir 'wix.exe'
    if (Test-Path $wixExe) { return $wixExe }

    # 清掉可能残留的半截 tool store：CI 里 target/ 会被 rust-cache 缓存，
    # 只恢复了一半的 .store 会让 dotnet 报 "Failed to read NuGet assets file"
    Remove-Item -Recurse -Force $ToolsDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null

    foreach ($ver in @('5.*', '')) {
        $installArgs = @('tool', 'install', '--tool-path', $ToolsDir, 'wix')
        if ($ver) { $installArgs += @('--version', $ver) }
        Write-Host "[package] dotnet tool install wix $ver"
        & $DotnetExe @installArgs | Out-Host
        if ((Test-Path $wixExe) -and $LASTEXITCODE -eq 0) { return $wixExe }
        Remove-Item -Recurse -Force $ToolsDir -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null
    }
    throw 'WiX 安装失败（需要 .NET SDK）'
}

function Resolve-WixCommand {
    $cmd = Get-Command wix -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    $dotnet = Get-Command dotnet -ErrorAction SilentlyContinue
    if (-not $dotnet) { throw '缺少 dotnet，无法安装 WiX（MSI 打包工具链）' }

    # 装到 runner 的临时目录，不要放 target/（CI 里 target 被 rust-cache 缓存，容易留残渣）
    $toolsDir = if ($env:RUNNER_TEMP) {
        Join-Path $env:RUNNER_TEMP 'dotnet-tools'
    } else {
        Join-Path $Root 'target/dotnet-tools'
    }
    return (Install-WixTool -DotnetExe $dotnet.Source -ToolsDir $toolsDir)
}

$Wix = Resolve-WixCommand
$MsiPath = Join-Path $OutPath "TianyiCloud-$Version-$ArchName.msi"
if (Test-Path $MsiPath) { Remove-Item -Force $MsiPath }

$WixArgs = @(
    'build',
    '-arch', $WixArch,
    '-d', "ProductVersion=$MsiVersion",
    '-d', "ExePath=$Exe",
    '-d', "LicensePath=$(Join-Path $Root 'LICENSE')",
    '-d', "IconPath=$Icon",
    '-o', $MsiPath,
    (Join-Path $Root 'packaging/windows/App.wxs')
)
& $Wix @WixArgs
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $MsiPath)) { throw 'MSI 打包失败' }

# wix 会在 MSI 旁边留下调试数据库，产物目录只保留分发文件
Get-ChildItem -Path $OutPath -Include '*.wixpdb', '*.wdb' -File -Recurse -ErrorAction SilentlyContinue |
    Remove-Item -Force -ErrorAction SilentlyContinue
Write-Host "[package] msi -> $MsiPath"

Write-Host '[package] 完成：'
Get-ChildItem -Path $OutPath -File | ForEach-Object { Write-Host ("  {0} ({1:N1} MB)" -f $_.Name, ($_.Length / 1MB)) }
