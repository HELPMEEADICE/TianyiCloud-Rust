#Requires -Version 5.1
<#
.SYNOPSIS
    校验版本号一致性：Cargo.toml 的 [workspace.package].version 是唯一真源。

.DESCRIPTION
    比对三处版本来源：

      1. Cargo.toml                      [workspace.package].version —— 真源，必须是 x.y.z
      2. packaging/windows/App.wxs       构建期由 `wix build -d ProductVersion=...` 注入，
                                         模板里必须保持 $(var.ProductVersion)，不得硬编码版本号
      3. packaging/macos/Info.plist      构建期由 package.sh 替换 __VERSION__ / __BUILD_VERSION__，
                                         模板里必须保持占位符，不得硬编码版本号

    判定规则（任一不满足即非零退出，并把三处实际值打印出来）：

      * 三处都必须是有效的 x.y.z；
      * 打包模板里出现的硬编码三段式版本号（XML 注释不算，注释只是示例）必须与真源一致；
      * 打包模板的注入点（$(var.ProductVersion) / __VERSION__）必须仍然存在，
        否则说明版本在打包链路上已经传不过去了。

    退出码：0 = 一致；1 = 不一致或注入点/真源缺失。

.EXAMPLE
    pwsh tools/check-versions.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $PSScriptRoot # <repo>/tools -> <repo>
$CargoToml = Join-Path $RepoRoot 'Cargo.toml'
$WxsPath = Join-Path $RepoRoot 'packaging/windows/App.wxs'
$PlistPath = Join-Path $RepoRoot 'packaging/macos/Info.plist'

$VersionPattern = '\d+\.\d+\.\d+'
$Problems = @()

function Get-XmlWithoutComments {
    param([string]$Text)
    # 去掉 <!-- ... --> 注释块：里面的版本号只是命令行示例，不参与比对
    return [regex]::Replace($Text, '(?s)<!--.*?-->', '')
}

function Get-HardcodedVersions {
    param([string]$Text)
    return @([regex]::Matches($Text, $VersionPattern) | ForEach-Object { $_.Value } | Select-Object -Unique)
}

# ------------------------------------------------------------------ 真源
if (-not (Test-Path $CargoToml)) { throw "找不到 $CargoToml" }

$CargoText = Get-Content -LiteralPath $CargoToml -Raw
$WorkspaceVersion = $null
$inWorkspacePackage = $false
foreach ($line in ($CargoText -split "`r?`n")) {
    if ($line -match '^\s*\[([^\]]+)\]') {
        $inWorkspacePackage = ($Matches[1].Trim() -eq 'workspace.package')
        continue
    }
    if ($inWorkspacePackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
        $WorkspaceVersion = $Matches[1].Trim()
        break
    }
}

if (-not $WorkspaceVersion) {
    $Problems += 'Cargo.toml 中找不到 [workspace.package].version'
} elseif ($WorkspaceVersion -notmatch "^$VersionPattern$") {
    $Problems += "Cargo.toml 的 [workspace.package].version 不是 x.y.z 形式：'$WorkspaceVersion'"
}

# ------------------------------------------------------------------ App.wxs
$WxsVersion = '(缺失)'
if (-not (Test-Path $WxsPath)) {
    $Problems += "找不到 $WxsPath"
} else {
    $WxsText = Get-XmlWithoutComments -Text (Get-Content -LiteralPath $WxsPath -Raw)
    $injected = $WxsText.Contains('$(var.ProductVersion)')
    $hardcoded = Get-HardcodedVersions -Text $WxsText

    if (-not $injected) {
        $Problems += 'App.wxs 里已经没有 $(var.ProductVersion) 注入点（版本号传不进 MSI）'
    }
    foreach ($v in $hardcoded) {
        if ($v -ne $WorkspaceVersion) {
            $Problems += "App.wxs 硬编码版本号 $v 与 Cargo.toml 的 $WorkspaceVersion 不一致"
        }
    }
    $WxsVersion = if ($injected) { '$(var.ProductVersion)（构建时 -d 注入）' } else { '(缺失注入点)' }
}

# ------------------------------------------------------------------ Info.plist
$PlistVersion = '(缺失)'
if (-not (Test-Path $PlistPath)) {
    $Problems += "找不到 $PlistPath"
} else {
    $PlistText = Get-XmlWithoutComments -Text (Get-Content -LiteralPath $PlistPath -Raw)
    $hasVersion = $PlistText.Contains('__VERSION__')
    $hasBuild = $PlistText.Contains('__BUILD_VERSION__')
    $hardcoded = Get-HardcodedVersions -Text $PlistText

    if (-not $hasVersion) {
        $Problems += 'Info.plist 里已经没有 __VERSION__ 占位符（版本号传不进 .app）'
    }
    if (-not $hasBuild) {
        $Problems += 'Info.plist 里已经没有 __BUILD_VERSION__ 占位符（CFBundleVersion 会缺失）'
    }
    foreach ($v in $hardcoded) {
        if ($v -ne $WorkspaceVersion) {
            $Problems += "Info.plist 硬编码版本号 $v 与 Cargo.toml 的 $WorkspaceVersion 不一致"
        }
    }
    $PlistVersion = if ($hasVersion) { '__VERSION__（构建时由 package.sh 替换）' } else { '(缺失占位符)' }
}

# ------------------------------------------------------------------ 报告
Write-Host '[check-versions] 三处实际值：'
Write-Host "  Cargo.toml   [workspace.package].version : $WorkspaceVersion"
Write-Host "  App.wxs      ProductVersion              : $WxsVersion"
Write-Host "  Info.plist   CFBundleShortVersionString  : $PlistVersion"

if ($Problems.Count -gt 0) {
    Write-Host '[check-versions] 不一致：' -ForegroundColor Red
    foreach ($p in $Problems) { Write-Host "  - $p" -ForegroundColor Red }
    exit 1
}

Write-Host '[check-versions] 版本号一致。'
exit 0
