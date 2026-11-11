<#
.SYNOPSIS
    采集界面基线截图（`examples/ui_preview` 的薄包装）。

.DESCRIPTION
    等价于

        cargo run -p tianyi-app --example ui_preview --locked -- <outdir>

    只多三件事：把默认输出目录钉到 `screenshots/after`（相对路径按仓库根解析，
    不受当前工作目录影响）、跑完统计张数与耗时、结束时把「下一步怎么比对」的
    命令原样打出来。

    **不删旧图**：`ui_preview` 自己在采集过程中重写同名 PNG，而不是先清空目录
    （清空会让"某张图突然采不出来"这件事变成静默缺图）。因此目录里可能残留上一轮
    的图；那是刻意保留的证据，不是垃圾——要判断哪张过期了，用 `visual-diff`
    的 `missing` / `changed` 判定，而不是靠删目录。

    不自动比对：采集是为了"给人看 + 给下一次提交当基线"，比对是另一个动作。
    需要一步到位时把两条命令串起来即可（见 .EXAMPLE）。

.PARAMETER OutDir
    输出目录，默认 `<仓库>/screenshots/after`。不存在会被创建。

.PARAMETER Lang
    采集非简中语种的界面：`en` / `zh-Hant` / `ja`。不传 = 简中原文。
    对应 `ui_preview` 读的 `TIANYI_PREVIEW_LANG` 环境变量。

.PARAMETER Release
    用 release profile 采集。默认 debug——基线的视觉内容与 profile 无关，
    debug 编译快得多。

.EXAMPLE
    pwsh tools/ui-preview.ps1

.EXAMPLE
    # 采到临时目录再比，不动仓库里的基线
    pwsh tools/ui-preview.ps1 -OutDir $env:TEMP\ui-new
    pwsh tools/visual-diff.ps1 -Before screenshots/after -After $env:TEMP\ui-new

.EXAMPLE
    # 采英文界面
    pwsh tools/ui-preview.ps1 -Lang en -OutDir $env:TEMP\ui-en
#>
[CmdletBinding()]
param(
    [string]$OutDir,
    [ValidateSet('zh-Hans', 'zh-Hant', 'en', 'ja')]
    [string]$Lang,
    [switch]$Release
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $repoRoot 'screenshots/after' }
elseif (-not [System.IO.Path]::IsPathRooted($OutDir)) { $OutDir = Join-Path $repoRoot $OutDir }
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

$cargoArgs = @('run', '-p', 'tianyi-app', '--example', 'ui_preview', '--locked')
if ($Release) { $cargoArgs += '--release' }
$cargoArgs += @('--', $OutDir)

if ($Lang) {
    # 简中 = 索引 0（空串），其余语种走 ui_preview 里的 TIANYI_PREVIEW_LANG 分支。
    $env:TIANYI_PREVIEW_LANG = if ($Lang -eq 'zh-Hans') { '' } else { $Lang }
    Write-Host "语种：$Lang"
}
else {
    Remove-Item Env:TIANYI_PREVIEW_LANG -ErrorAction SilentlyContinue
    Write-Host '语种：简体中文（默认）'
}

Write-Host "输出：$OutDir"
Write-Host ('执行：cargo ' + ($cargoArgs -join ' '))
Write-Host ''

$before = @(Get-ChildItem -LiteralPath $OutDir -File -Filter *.png)
$sw = [System.Diagnostics.Stopwatch]::StartNew()
Push-Location $repoRoot
try {
    & cargo @cargoArgs
    $code = $LASTEXITCODE
}
finally {
    Pop-Location
    $sw.Stop()
}

if ($code -ne 0) {
    Write-Host ""
    Write-Host "采集失败：cargo 退出码 $code（耗时 $([Math]::Round($sw.Elapsed.TotalSeconds, 1))s）"
    exit $code
}

$after = @(Get-ChildItem -LiteralPath $OutDir -File -Filter *.png)
Write-Host ""
Write-Host ("采集完成：{0} 张 PNG（运行前 {1} 张），耗时 {2}s" -f $after.Count, $before.Count, [Math]::Round($sw.Elapsed.TotalSeconds, 1))

$relOut = $OutDir
if ($OutDir.StartsWith($repoRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    $relOut = $OutDir.Substring($repoRoot.Length).TrimStart('\', '/') -replace '\\', '/'
}

Write-Host ""
Write-Host '下一步（在本仓库根目录执行）：'
Write-Host "  # 与上一个提交的基线比（噪声底默认 60 像素，06/21 两张 spinner 会抖）"
Write-Host "  pwsh tools/visual-diff.ps1 -BeforeRev HEAD"
Write-Host "  # 与工作区里现有的旧图比"
Write-Host "  pwsh tools/visual-diff.ps1 -Before $relOut -After $relOut"
exit 0
