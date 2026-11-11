<#
.SYNOPSIS
    逐通道比对两组界面基线 PNG，输出每张图的差异像素数、差异包围盒与判定。

.DESCRIPTION
    把「临时 python + numpy 片段」固化成可复跑的工具。逐通道（BGRA 四通道都比）
    统计差异像素数，并给出差异区域的包围盒；同一张图的尺寸不一致单独报
    `size-mismatch` 并计入 changed，不会因为"宽高不同就没法比"被静默跳过。

    判定分三档（`-Noise` 是噪声底，默认 60 像素）：

        identical      差异像素数 = 0
        noise          0 < 差异像素数 <= -Noise      —— 已知会抖的场景落在这一档
        changed        差异像素数 > -Noise           —— 需要人看一眼

    `-Noise 0` 表示关掉噪声底：任何非零差异都算 changed。这一档用于"我要逐像素相同"
    的场合（例如同一个二进制连渲两次的复现性检查）。

    退出码：存在任一 changed（含 size-mismatch / missing / error）→ 1，否则 0。
    因此 CI 里可以直接 `pwsh tools/visual-diff.ps1 -BeforeRev origin/main`。

    为什么不用 numpy / Pillow：
      · 它们不是本仓库的依赖，只是某些机器上恰好有；工具要能在只装了 Rust 工具链 +
        PowerShell 7 的机器上跑，否则"验证手段"会随机器漂移。
      · Pillow 12 起 `ImageChops.difference(...).getbbox()` 默认 `alpha_only=True`，
        RGBA 全不透明图恒返回 `None`——第三轮 `b181cd7` 的"22 张逐像素相同"就是
        这么来的假绿（见 `docs/ROUND3_REPORT.md` §5.2）。逐通道自己数，不留这个坑。
    实现只用 .NET 自带能力：`System.Drawing.Bitmap` + `LockBits` 取原始 BGRA 缓冲，
    比较循环放在 `Add-Type` 内联的 C# 里（PowerShell 里逐像素循环 200 万次太慢，
    29 张图要跑几分钟）。这两个程序集随 PowerShell 7 一起交付，不需要联网装包。

.PARAMETER Before
    基准目录（"改之前"那一组 PNG）。

.PARAMETER After
    对比目录（"改之后"那一组 PNG）。默认 `<仓库>/screenshots/after`。

.PARAMETER BeforeRev
    用 git 里的某个版本当基准：把该版本 `<RevPath>` 下的 PNG 解到临时目录再比对，
    便于"与上一个提交比"。与 `-Before` 互斥。

.PARAMETER RevPath
    `-BeforeRev` 模式下要取的仓库内路径，默认 `screenshots/after`。

.PARAMETER Only
    文件名过滤，glob 写法，默认 `*.png`。既匹配文件名也匹配不含扩展名的主名，
    所以 `-Only 06-*` 与 `-Only 06-*.png` 等价。

.PARAMETER Noise
    噪声底（差异像素数），默认 60。`0` = 关掉噪声底。

.PARAMETER KeepTemp
    `-BeforeRev` 模式下保留临时解包目录（默认比对结束即删）。

.EXAMPLE
    # 自证：同一份目录比对自己，必须全是 identical 且退出码 0
    pwsh tools/visual-diff.ps1 -Before screenshots/after -After screenshots/after

.EXAMPLE
    # 与上一个提交的基线比（当前工作树的图 vs HEAD 里的图）
    pwsh tools/visual-diff.ps1 -BeforeRev HEAD

.EXAMPLE
    # 只比某几张，且关掉噪声底
    pwsh tools/visual-diff.ps1 -Before screenshots/after -After $env:TEMP\new -Only '0*' -Noise 0
#>
[CmdletBinding(DefaultParameterSetName = 'Path')]
param(
    [Parameter(ParameterSetName = 'Path', Mandatory = $true, Position = 0)]
    [string]$Before,

    [Parameter(ParameterSetName = 'Rev', Mandatory = $true)]
    [string]$BeforeRev,

    [string]$After,

    [Parameter(ParameterSetName = 'Rev')]
    [string]$RevPath = 'screenshots/after',

    [string]$Only = '*.png',

    [int]$Noise = 60,

    [switch]$KeepTemp
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $After) { $After = Join-Path $repoRoot 'screenshots/after' }
if ($Noise -lt 0) { throw "-Noise 不能是负数（收到 $Noise）" }

# ---------------- 内联 C#：只做一件事——逐通道比对两块 BGRA 缓冲 ----------------
if (-not ('TianyiVisualDiff' -as [type])) {
    Add-Type -AssemblyName System.Drawing
    $refs = @(
        [System.Drawing.Bitmap].Assembly.Location
        [System.Drawing.Point].Assembly.Location
        (Join-Path $PSHOME 'System.Private.Windows.GdiPlus.dll')
        (Join-Path $PSHOME 'System.Private.Windows.Core.dll')
    ) | Where-Object { Test-Path -LiteralPath $_ }

    $code = @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class TianyiVisualDiff
{
    // 返回 long[5]：
    //   尺寸/格式可读且宽高一致 → { 差异像素数, x0, y0, x1, y1 }（包围盒含端点，无差异时后四项为 0）
    //   宽高不一致             → { -1, wBefore, hBefore, wAfter, hAfter }
    // 抛异常（文件读不出来）由调用方捕获并记成 error，不在这里吞掉。
    public static long[] Compare(string beforePath, string afterPath)
    {
        using (var a = Load32bpp(beforePath))
        using (var b = Load32bpp(afterPath))
        {
            int wa = a.Width, ha = a.Height, wb = b.Width, hb = b.Height;
            if (wa != wb || ha != hb)
            {
                return new long[] { -1, wa, ha, wb, hb };
            }

            byte[] da = ReadPixels(a, out int strideA);
            byte[] db = ReadPixels(b, out int strideB);

            long diff = 0;
            int x0 = int.MaxValue, y0 = int.MaxValue, x1 = -1, y1 = -1;
            for (int y = 0; y < ha; y++)
            {
                int rowA = y * strideA, rowB = y * strideB;
                for (int x = 0; x < wa; x++)
                {
                    int ia = rowA + x * 4, ib = rowB + x * 4;
                    // BGRA 四通道全比：只看 RGB 会漏掉"只有不透明度变了"的差异，
                    // 而基线的 PNG 都是 32 位带 alpha 的。
                    if (da[ia] != db[ib] || da[ia + 1] != db[ib + 1] ||
                        da[ia + 2] != db[ib + 2] || da[ia + 3] != db[ib + 3])
                    {
                        diff++;
                        if (x < x0) x0 = x;
                        if (x > x1) x1 = x;
                        if (y < y0) y0 = y;
                        if (y > y1) y1 = y;
                    }
                }
            }

            if (diff == 0) return new long[] { 0, 0, 0, 0, 0 };
            return new long[] { diff, x0, y0, x1, y1 };
        }
    }

    // 统一成 32bppArgb 再取缓冲：源图若是 24bpp/索引色，LockBits 会拒绝，
    // 这里先画一份同尺寸的 32bpp 副本（像素值原样搬运，不缩放）。
    private static Bitmap Load32bpp(string path)
    {
        var src = new Bitmap(path);
        if (src.PixelFormat == PixelFormat.Format32bppArgb) return src;
        try
        {
            var dst = new Bitmap(src.Width, src.Height, PixelFormat.Format32bppArgb);
            using (var g = Graphics.FromImage(dst))
            {
                g.DrawImageUnscaled(src, 0, 0);
            }
            return dst;
        }
        finally
        {
            src.Dispose();
        }
    }

    private static byte[] ReadPixels(Bitmap bmp, out int stride)
    {
        var rect = new Rectangle(0, 0, bmp.Width, bmp.Height);
        BitmapData bd = bmp.LockBits(rect, ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        try
        {
            stride = bd.Stride;
            var data = new byte[bd.Stride * bmp.Height];
            Marshal.Copy(bd.Scan0, data, 0, data.Length);
            return data;
        }
        finally
        {
            bmp.UnlockBits(bd);
        }
    }
}
'@
    Add-Type -TypeDefinition $code -ReferencedAssemblies $refs -Language CSharp
}

# ---------------- 收集两侧的图 ----------------
function Get-PngMap([string]$dir, [string]$filter) {
    $map = @{}
    if (-not (Test-Path -LiteralPath $dir)) { return $map }
    $ext = [System.IO.Path]::GetExtension($filter)
    if (-not $ext) { $filter = "$filter.png"; $ext = '.png' }
    $base = [System.IO.Path]::GetFileNameWithoutExtension($filter)
    foreach ($f in Get-ChildItem -LiteralPath $dir -File | Where-Object { $_.Extension -ieq $ext }) {
        if ($f.Name -like $filter -or $f.BaseName -like $base) {
            $map[$f.BaseName] = $f.FullName
        }
    }
    return $map
}

$tempRoot = $null
$beforeDir = $Before
$sourceLabel = ""

if ($PSCmdlet.ParameterSetName -eq 'Rev') {
    $tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('tianyi-visual-diff-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null
    $beforeDir = Join-Path $tempRoot 'png'
    New-Item -ItemType Directory -Path $beforeDir -Force | Out-Null

    Push-Location $repoRoot
    try {
        $paths = @(& git ls-tree -r --name-only $BeforeRev -- $RevPath 2>&1)
        if ($LASTEXITCODE -ne 0) { throw "git ls-tree $BeforeRev -- $RevPath 失败：`n$($paths -join "`n")" }
    }
    finally {
        Pop-Location
    }
    $pngPaths = $paths | Where-Object { $_ -like '*.png' }
    if (-not $pngPaths) { throw "版本 $BeforeRev 里 $RevPath 没有 PNG（确认路径与版本号）" }

    foreach ($p in $pngPaths) {
        $dest = Join-Path $beforeDir (Split-Path -Leaf $p)
        # Start-Process 的重定向是 OS 级文件句柄，二进制安全；PowerShell 的 `>`
        # 会把子进程 stdout 按文本解码，PNG 会被写坏。
        $proc = Start-Process -FilePath 'git' -ArgumentList @('cat-file', 'blob', "$BeforeRev`:$p") `
            -RedirectStandardOutput $dest -NoNewWindow -Wait -PassThru
        if ($proc.ExitCode -ne 0) { throw "git cat-file blob $BeforeRev`:$p 失败（退出码 $($proc.ExitCode)）" }
    }
    $sourceLabel = "$BeforeRev`:$RevPath（已解到 $beforeDir）"
}
else {
    $sourceLabel = (Resolve-Path -LiteralPath $Before).Path
}

$afterResolved = (Resolve-Path -LiteralPath $After).Path
$beforeMap = Get-PngMap $beforeDir $Only
$afterMap = Get-PngMap $afterResolved $Only

$names = @($beforeMap.Keys + $afterMap.Keys | Sort-Object -Unique)
if (-not $names) {
    if ($tempRoot -and -not $KeepTemp) { Remove-Item -LiteralPath $tempRoot -Recurse -Force }
    throw "没有匹配 $Only 的图片（before=$beforeDir after=$afterResolved）"
}

Write-Host "视觉差异比对"
Write-Host "  before : $sourceLabel"
Write-Host "  after  : $afterResolved"
if ($Noise -eq 0) {
    Write-Host "  噪声底 : 0（任何非零差异都算 changed）"
}
else {
    Write-Host "  噪声底 : $Noise 像素（<= 该值判 noise）"
}
Write-Host ""

$counts = [ordered]@{ identical = 0; noise = 0; changed = 0 }
$lines = @()

try {
    foreach ($name in $names) {
        $bp = $beforeMap[$name]
        $ap = $afterMap[$name]
        $diff = 0
        $bbox = 'bbox=none'
        $verdict = 'identical'

        if (-not $bp -or -not $ap) {
            $verdict = 'missing'
            $bbox = if (-not $ap) { 'bbox=none（after 缺这张）' } else { 'bbox=none（before 缺这张）' }
        }
        else {
            try {
                $r = [TianyiVisualDiff]::Compare($bp, $ap)
            }
            catch {
                $verdict = 'error'
                $bbox = "bbox=none（$($_.Exception.Message)）"
                $r = @(0, 0, 0, 0, 0)
            }
            if ($verdict -ne 'error') {
                if ($r[0] -lt 0) {
                    $verdict = 'size-mismatch'
                    $diff = [Math]::Abs($r[1] * $r[2] - $r[3] * $r[4])
                    $bbox = "bbox=none（before $($r[1])x$($r[2]) vs after $($r[3])x$($r[4])）"
                }
                else {
                    $diff = [int]$r[0]
                    if ($diff -eq 0) {
                        $verdict = 'identical'
                    }
                    elseif ($Noise -gt 0 -and $diff -le $Noise) {
                        $verdict = 'noise'
                    }
                    else {
                        $verdict = 'changed'
                    }
                    if ($diff -gt 0) { $bbox = "bbox=($($r[1]),$($r[2]))-($($r[3]),$($r[4]))" }
                }
            }
        }

        if ($verdict -in @('identical', 'noise')) { $counts[$verdict]++ } else { $counts['changed']++ }
        $lines += [pscustomobject]@{ Name = $name; Diff = $diff; BBox = $bbox; Verdict = $verdict }
    }
}
finally {
    if ($tempRoot -and -not $KeepTemp) { Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue }
}

foreach ($l in $lines) {
    # missing / error 没有可比的像素，差异数写成 `-`：写 0 会被读成"逐像素相同"。
    $diffText = if ($l.Verdict -in @('missing', 'error')) { '-' } else { $l.Diff }
    '{0,-30} {1,7}  {2,-34} {3}' -f $l.Name, $diffText, $l.BBox, $l.Verdict
}

$total = $names.Count
$changed = $counts['changed']
Write-Host ""
Write-Host ("合计 {0} 张：identical {1} / noise {2} / changed {3}" -f $total, $counts['identical'], $counts['noise'], $changed)
if ($changed -gt 0) {
    $bad = ($lines | Where-Object { $_.Verdict -notin @('identical', 'noise') } | ForEach-Object { $_.Name }) -join ', '
    Write-Host "changed 列表：$bad"
    exit 1
}
exit 0
