<#
.SYNOPSIS
    从 Slint 源码提取 @tr(...) 的 msgid，或据此生成 .po 语言包骨架。

.DESCRIPTION
    扫描 crates/tianyi-app/ui 下所有 .slint（跳过 ui/dev/：那是 examples/icon_sheet.rs
    的自检素材，不进产品界面），把 @tr("…") 里的 msgid 抽出来。

    build.rs 用 with_default_translation_context(None) 编译，全库共用一张 msgid 表、
    .po 里不写 msgctxt，所以 @tr 的第一个字符串字面量就是 msgid（不存在
    @tr("上下文" => "文案") 的写法）。语言目录名要与 slint::select_bundled_translation
    的入参字面一致，简中不建目录（它的原文就是翻译表索引 0 的回退语言）。

    输出一律 UTF-8 无 BOM，LF 换行。

.PARAMETER Count
    只输出一行「msgid 总数 / 去重数」，便于在 CI 里比对。

.PARAMETER Skeleton
    生成 crates/tianyi-app/lang/<lang>/LC_MESSAGES/tianyi-app.po 骨架：
    头部 + 每条 msgid + 空 msgstr（未翻译的条目 gettext 会回退到 msgid 原文）。

.EXAMPLE
    pwsh tools/i18n-extract.ps1

.EXAMPLE
    pwsh tools/i18n-extract.ps1 -Count

.EXAMPLE
    pwsh tools/i18n-extract.ps1 -Skeleton ja
#>
[CmdletBinding()]
param(
    [switch]$Count,
    [string]$Skeleton
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$appDir = Join-Path $repoRoot 'crates/tianyi-app'
$uiDir = Join-Path $appDir 'ui'

# Slint 字符串字面量里只有 \" 与 \\ 两种转义，这里手工解回来：
# 用 [^"]* 之类的朴素正则会在 \" 处提前截断，把一条文案拆成两半。
function ConvertFrom-SlintString([string]$s) {
    $sb = [System.Text.StringBuilder]::new()
    $i = 0
    while ($i -lt $s.Length) {
        if ($s[$i] -eq '\' -and ($i + 1) -lt $s.Length) {
            $next = $s[$i + 1]
            if ($next -eq '"' -or $next -eq '\') {
                [void]$sb.Append($next)
                $i += 2
                continue
            }
        }
        [void]$sb.Append($s[$i])
        $i++
    }
    return $sb.ToString()
}

# 反向：写回 .po 时必须先转义反斜杠，否则 \" 会被 gettext 读成「结束引号 + 孤立引号」
function ConvertTo-PoString([string]$s) {
    return $s.Replace('\', '\\').Replace('"', '\"')
}

function Get-TrMsgIds {
    if (-not (Test-Path -LiteralPath $uiDir)) {
        throw "找不到界面目录：$uiDir"
    }
    $files = Get-ChildItem -LiteralPath $uiDir -Recurse -Filter *.slint -File |
        Where-Object { $_.FullName -notmatch '[\\/]dev[\\/]' }
    $ids = [System.Collections.Generic.List[string]]::new()
    foreach ($file in $files) {
        $text = Get-Content -LiteralPath $file.FullName -Raw -Encoding utf8
        foreach ($m in [regex]::Matches($text, '@tr\(\s*"((?:[^"\\]|\\.)*)"')) {
            $ids.Add((ConvertFrom-SlintString $m.Groups[1].Value))
        }
    }
    return $ids
}

$all = @(Get-TrMsgIds)
$unique = @($all | Sort-Object -Unique)

if ($Count) {
    Write-Output "$($all.Count) / $($unique.Count)"
    return
}

if ($Skeleton) {
    $lang = $Skeleton.Trim()
    if ([string]::IsNullOrWhiteSpace($lang)) {
        throw '-Skeleton 需要一个语言目录名，例如 zh-Hant / en / ja'
    }
    $poDir = Join-Path $appDir (Join-Path 'lang' (Join-Path $lang 'LC_MESSAGES'))
    New-Item -ItemType Directory -Force -Path $poDir | Out-Null
    $poPath = Join-Path $poDir 'tianyi-app.po'

    # 数量形式跟着语言走：英文有单复数，中日没有
    $plural = if ($lang -eq 'en') { 'nplurals=2; plural=(n != 1);' } else { 'nplurals=1; plural=0;' }

    $blocks = [System.Collections.Generic.List[string]]::new()
    $blocks.Add(@(
            'msgid ""'
            'msgstr ""'
            '"Content-Type: text/plain; charset=UTF-8\n"'
            ('"Language: {0}\n"' -f $lang)
            ('"Plural-Forms: {0}\n"' -f $plural)
        ) -join "`n")
    foreach ($id in $unique) {
        $blocks.Add((('msgid "{0}"' -f (ConvertTo-PoString $id)) + "`n" + 'msgstr ""'))
    }

    $content = ($blocks -join "`n`n") + "`n"
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($poPath, $content, $utf8NoBom)
    Write-Output "已生成 $poPath（$($unique.Count) 条 msgid）"
    return
}

foreach ($id in $unique) {
    Write-Output $id
}
