<#
.SYNOPSIS
    从微软官方仓库 microsoft/fluentui-system-icons 拉取本项目实际使用的 24px SVG。

.DESCRIPTION
    只下载清单里列出的图标（不把整个图标仓库加入项目），并按官方原始文件名保存，
    便于回溯上游。每个图标都会尝试 Regular 与 Filled 两种风格；上游没有 Filled
    变体的图标在清单里标注 fallback（Filled 复用 Regular 文件）。

    生成物：
      - crates/tianyi-app/ui/assets/icons/*.svg
      - crates/tianyi-app/ui/assets/icons/SOURCES.md（来源/版本/映射表）
      - 控制台输出可直接贴进 icons.slint 的 @image-url 行

.EXAMPLE
    pwsh -File tools/fetch-fluent-icons.ps1
#>
[CmdletBinding()]
param(
    # 上游分支或 tag。默认 main；要固定版本可传 tag（如 v1.1.300）
    [string]$Ref = 'main',
    # 目标目录
    [string]$Dest = 'crates/tianyi-app/ui/assets/icons',
    # 只按磁盘上已有资源重算注册表，不联网（用于不改变资源版本的前提下改映射）
    [switch]$Offline
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root $Dest
New-Item -ItemType Directory -Force -Path $dest | Out-Null

# 清单：语义键 → 上游 snake 文件名（官方 assets/<Name>/SVG/ic_fluent_<snake>_<size>_<style>.svg）
$manifest = @(
    # 侧边栏
    @{ key = 'home';                snake = 'home' }
    @{ key = 'people';              snake = 'people' }
    @{ key = 'transfer';            snake = 'arrow_swap' }
    @{ key = 'link';                snake = 'link' }
    @{ key = 'settings';            snake = 'settings' }
    # 文件类型
    @{ key = 'folder';              snake = 'folder' }
    @{ key = 'file';                snake = 'document' }
    @{ key = 'image';               snake = 'image' }
    @{ key = 'video';               snake = 'video' }
    @{ key = 'audio';               snake = 'music_note_2' }
    @{ key = 'archive';             snake = 'folder_zip' }
    @{ key = 'pdf';                 snake = 'document_pdf' }
    @{ key = 'text';                snake = 'document_text' }
    # 文件操作
    @{ key = 'search';              snake = 'search' }
    @{ key = 'refresh';             snake = 'arrow_clockwise' }
    @{ key = 'upload';              snake = 'arrow_upload' }
    @{ key = 'download';            snake = 'arrow_download' }
    @{ key = 'folder-add';          snake = 'folder_add' }
    @{ key = 'delete';              snake = 'delete' }
    @{ key = 'rename';              snake = 'rename' }
    @{ key = 'copy';                snake = 'copy' }
    @{ key = 'move';                snake = 'arrow_move' }
    @{ key = 'folder-open';         snake = 'folder_open' }
    @{ key = 'select-all';          snake = 'select_all_on' }
    # 导航/方向
    @{ key = 'back';                snake = 'arrow_left' }
    @{ key = 'chevron-right';       snake = 'chevron_right' }
    @{ key = 'chevron-down';        snake = 'chevron_down' }
    @{ key = 'chevron-up';          snake = 'chevron_up' }
    # 通用操作
    @{ key = 'more';                snake = 'more_horizontal' }
    @{ key = 'close';               snake = 'dismiss' }
    @{ key = 'check';               snake = 'checkmark' }
    @{ key = 'add';                 snake = 'add' }
    @{ key = 'filter';              snake = 'filter' }
    @{ key = 'sort';                snake = 'arrow_sort' }
    @{ key = 'sort-asc';            snake = 'arrow_sort_up' }
    @{ key = 'sort-desc';           snake = 'arrow_sort_down' }
    # 传输
    @{ key = 'pause';               snake = 'pause' }
    @{ key = 'play';                snake = 'play' }
    @{ key = 'cancel';              snake = 'dismiss_circle' }
    @{ key = 'retry';               snake = 'arrow_counterclockwise' }
    @{ key = 'success';             snake = 'checkmark_circle' }
    @{ key = 'error';               snake = 'error_circle' }
    @{ key = 'warning';             snake = 'warning' }
    # 账号与状态
    @{ key = 'account';             snake = 'person' }
    @{ key = 'password-show';       snake = 'eye' }
    @{ key = 'password-hide';       snake = 'eye_off' }
    # 上游没有 Sign In；最准确的“进入/登录”语义是 Arrow Enter，
    # 但它只提供 16px 与 20px 资源（24px 逐文件实测 404），因此这里显式取官方 20px 版本。
    @{ key = 'login';               snake = 'arrow_enter'; size = 20 }
    @{ key = 'logout';              snake = 'sign_out' }
    @{ key = 'info';                snake = 'info' }
    @{ key = 'cloud';               snake = 'cloud' }
    @{ key = 'cloud-off';           snake = 'cloud_off' }
    @{ key = 'storage';             snake = 'storage' }
    @{ key = 'clock';               snake = 'clock' }
    @{ key = 'light';               snake = 'weather_sunny' }
    @{ key = 'dark';                snake = 'weather_moon' }
    @{ key = 'qr';                  snake = 'qr_code' }
)

# 上游把缩写整体大写（document_pdf → Document PDF、qr_code → QR Code），
# 这类词不能按 Title Case 猜，先查已知缩写表，再退化为首字母大写。
$acronyms = @('qr', 'pdf', 'api', 'id', 'url', 'ai', 'hd', 'usb', 'sip')

function Resolve-UpstreamDir {
    param([string]$Snake)
    # 官方目录名是 snake_case 的 Title Case（arrow_swap → Arrow Swap），
    # 少数含数字（music_note_2 → Music Note 2）。逐个试，取第一个 200 的。
    $words = $Snake -split '_'
    $acr = ($words | ForEach-Object {
            if ($_ -match '^\d+$') { $_ }
            elseif ($acronyms -contains $_) { $_.ToUpperInvariant() }
            else { (Get-Culture).TextInfo.ToTitleCase($_) }
        }) -join ' '
    $plain = ($words | ForEach-Object { if ($_ -match '^\d+$') { $_ } else { (Get-Culture).TextInfo.ToTitleCase($_) } }) -join ' '
    @($acr, $plain, $Snake) | Select-Object -Unique
}

$rows = @()
$usedUrls = @{}
foreach ($item in $manifest) {
    $snake = $item.snake
    $key = $item.key
    # 默认取 24px 画布；个别图标上游只提供别的尺寸时在清单里显式指定
    $canvas = if ($item.ContainsKey('size')) { [int]$item.size } else { 24 }
    $dirs = Resolve-UpstreamDir -Snake $snake
    $record = [ordered]@{ key = $key; snake = $snake; size = $canvas; regular = $null; filled = $null; filled_fallback = $false }

    foreach ($style in 'regular', 'filled') {
        $fetched = $false
        foreach ($d in $dirs) {
            $name = "ic_fluent_${snake}_${canvas}_${style}.svg"
            $url = "https://raw.githubusercontent.com/microsoft/fluentui-system-icons/$Ref/assets/$($d -replace ' ', '%20')/SVG/$name"
            $out = Join-Path $dest $name
            if ($usedUrls.ContainsKey($name)) { $record[$style] = $name; $fetched = $true; break }
            if ($Offline) {
                if (Test-Path $out) { $record[$style] = $name; $fetched = $true; break }
                continue
            }
            try {
                Invoke-WebRequest -Uri $url -UseBasicParsing -TimeoutSec 30 -OutFile $out
                $usedUrls[$name] = $url
                $record[$style] = $name
                $fetched = $true
                break
            } catch {
                if (Test-Path $out) { Remove-Item $out -Force }
            }
        }
        if (-not $fetched -and $style -eq 'filled') {
            # 上游部分图标没有 Filled 变体（方向箭头等），Filled 直接复用 Regular 文件
            $record.filled = $record.regular
            $record.filled_fallback = $true
        }
        if (-not $fetched -and $style -eq 'regular') {
            throw "无法从上游取得 $key ($snake) 的 24px regular SVG"
        }
    }
    $rows += [pscustomobject]$record
    $mark = if ($record.filled_fallback) { 'filled→regular' } else { 'filled ok' }
    Write-Host ("{0,-16} {1,-24} {2}" -f $key, $snake, $mark)
}

# ------------------------------------------------------------------ 来源与许可
$licensePath = Join-Path $dest 'LICENSE'
if (-not $Offline) {
    $licenseUrl = "https://raw.githubusercontent.com/microsoft/fluentui-system-icons/$Ref/LICENSE"
    Invoke-WebRequest -Uri $licenseUrl -UseBasicParsing -TimeoutSec 30 -OutFile $licensePath
}

$commit = if ($Offline) { '（离线模式未重新解析，沿用上一次记录的提交）' } else {
    try {
        (Invoke-RestMethod -Uri "https://api.github.com/repos/microsoft/fluentui-system-icons/commits/$Ref" -Headers @{ 'User-Agent' = 'ps' } -TimeoutSec 30).sha
    } catch { 'unknown' }
}

$sb = [System.Text.StringBuilder]::new()
[void]$sb.AppendLine('# 图标资源来源')
[void]$sb.AppendLine()
[void]$sb.AppendLine('本项目所有应用 UI 图标均来自微软官方开源图标库 **Microsoft Fluent UI System Icons**，')
[void]$sb.AppendLine('没有再手工编写任何 SVG 路径 / Slint Path / 字符或 Emoji 图标。')
[void]$sb.AppendLine()
[void]$sb.AppendLine('| 项 | 值 |')
[void]$sb.AppendLine('| --- | --- |')
[void]$sb.AppendLine('| 上游仓库 | https://github.com/microsoft/fluentui-system-icons |')
[void]$sb.AppendLine("| 引用 | ``$Ref`` |")
[void]$sb.AppendLine("| 解析到的提交 | ``$commit`` |")
[void]$sb.AppendLine('| 许可证 | MIT（见同目录 LICENSE） |')
[void]$sb.AppendLine(('| 规格 | 24px 画布（{0} 个图标因上游无 24px 版本改用其它尺寸，见下表画布列）；Regular 为默认态，Filled 为选中态 |' -f ($rows | Where-Object { $_.size -ne 24 }).Count))
[void]$sb.AppendLine("| 抓取时间 | $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') |")
[void]$sb.AppendLine()
[void]$sb.AppendLine('## 文件清单与语义映射')
[void]$sb.AppendLine()
[void]$sb.AppendLine('| Slint 图标名 | 上游文件名 | 画布 | Regular | Filled |')
[void]$sb.AppendLine('| --- | --- | --- | --- | --- |')
foreach ($r in $rows) {
    $f = if ($r.filled_fallback) { "$($r.filled)（上游无 Filled，复用 Regular）" } else { $r.filled }
    [void]$sb.AppendLine("| ``$($r.key)`` | ``$($r.snake)`` | $($r.size)px | ``$($r.regular)`` | ``$f`` |")
}
[void]$sb.AppendLine()
[void]$sb.AppendLine('## 关于 colorize')
[void]$sb.AppendLine()
[void]$sb.AppendLine('Slint 的 `Image.colorize` 把图像当作 **alpha 蒙版** 并以给定颜色绘制（见 i-slint-compiler')
[void]$sb.AppendLine('`builtins.slint` 中 `Image.colorize` 的文档），因此本目录的 SVG 保持微软原始字节不变，')
[void]$sb.AppendLine('主题色 / 选中态 / 禁用态着色全部在 Slint 侧完成，无需为每种颜色生成副本。')
if (-not $Offline) { Set-Content -Path (Join-Path $dest 'SOURCES.md') -Value $sb.ToString() -Encoding utf8 }

# ------------------------------------------------------------------ 回填 icons.slint
# icons.slint 里有两个 generated 区块（图标名 enum / 资源注册表 / 映射函数），
# 由本脚本按"磁盘上真实存在的 SVG"生成并回填，保证三者永不脱节。
$slintPath = Join-Path $root 'crates/tianyi-app/ui/theme/icons.slint'
$slint = Get-Content -Raw -Path $slintPath

$enumLines = [System.Text.StringBuilder]::new()
[void]$enumLines.AppendLine('export enum IconName {')
[void]$enumLines.AppendLine('    none,')
foreach ($r in $rows) { [void]$enumLines.AppendLine("    $($r.key),") }
[void]$enumLines.AppendLine('}')

$assetLines = [System.Text.StringBuilder]::new()
foreach ($r in $rows) {
    [void]$assetLines.AppendLine("    out property <image> $($r.key)-regular: @image-url(`"../assets/icons/$($r.regular)`");")
    [void]$assetLines.AppendLine("    out property <image> $($r.key)-filled: @image-url(`"../assets/icons/$($r.filled)`");")
}

# 名称 → 资源只能靠三元链表达：Slint 没有 match，且 @image-url 必须是编译期字面量。
# 这里生成的是 Icon 组件的 source 绑定（属性绑定可以读 global，pure 函数不行）。
$pickLines = [System.Text.StringBuilder]::new()
[void]$pickLines.AppendLine('    source: root.name == IconName.none ? IconAssets.placeholder')
foreach ($r in $rows) {
    [void]$pickLines.AppendLine("        : root.name == IconName.$($r.key) ? (root.filled ? IconAssets.$($r.key)-filled : IconAssets.$($r.key)-regular)")
}
[void]$pickLines.AppendLine('        : IconAssets.placeholder;')

function Splice-Block {
    param([string]$Text, [string]$Tag, [string]$Content)
    $pattern = "(?s)(// <generated:$Tag>).*?(// </generated:$Tag>)"
    if ($Text -notmatch $pattern) { throw "icons.slint 里找不到 generated:$Tag 标记" }
    $body = $Content.TrimEnd()
    return [regex]::Replace($Text, $pattern, { param($m) "$($m.Groups[1].Value)`n$body`n$($m.Groups[2].Value)" })
}

$slint = Splice-Block -Text $slint -Tag 'enum' -Content $enumLines.ToString()
$slint = Splice-Block -Text $slint -Tag 'assets' -Content $assetLines.ToString()
$slint = Splice-Block -Text $slint -Tag 'source' -Content $pickLines.ToString()
Set-Content -Path $slintPath -Value $slint -Encoding utf8

Write-Host ''
Write-Host "SVG 数量: $((Get-ChildItem $dest -Filter *.svg).Count)  目录: $dest"
Write-Host "已回填 $slintPath"


# ------------------------------------------------------------------ 图标总览（开发自检）
# 生成 ui/dev/icon-sheet.slint：把注册表里全部图标按 16/20/32 与 Regular/Filled
# 全部铺开，配合 examples/icon_sheet.rs 渲染成 PNG，用来逐个肉眼验证资源没坏。
#
# 注意：GridLayout 的单元格必须显式给 row / col，否则 Slint 会把所有子项排成同一行。
$sheetPath = Join-Path $root 'crates/tianyi-app/ui/dev/icon-sheet.slint'
New-Item -ItemType Directory -Force -Path (Split-Path $sheetPath) | Out-Null

$sheetLines = [System.Collections.Generic.List[string]]::new()
function Add-Lines { param([string[]]$Lines) foreach ($line in $Lines) { $script:sheetLines.Add($line) } }

function New-Cell {
    param([int]$Row, [int]$Col, [string]$Width, [string[]]$Body)
    $out = @(
        "            Rectangle {",
        "                row: $Row;",
        "                col: $Col;",
        "                width: $Width;",
        "                height: 36px;",
        "                background: transparent;"
    ) + ($Body | ForEach-Object { "                $_" }) + @("            }")
    return $out
}

$sheetCols = @(
    @{ Title = '16 / Reg'; Size = 16; Filled = 'false' },
    @{ Title = '16 / Fill'; Size = 16; Filled = 'true' },
    @{ Title = '20 / Reg'; Size = 20; Filled = 'false' },
    @{ Title = '20 / Fill'; Size = 20; Filled = 'true' },
    @{ Title = '32 / Reg'; Size = 32; Filled = 'false' },
    @{ Title = '32 / Fill'; Size = 32; Filled = 'true' }
)

Add-Lines @(
    '// 图标总览（由 tools/fetch-fluent-icons.ps1 生成，人工修改会在下次生成时被覆盖）',
    '//',
    '// 用途：一次性渲染注册表里的全部图标、全部风格与常用尺寸，',
    '// 配合 `cargo run -p tianyi-app --example icon_sheet` 产出 PNG 做自检。',
    '// 不参与主界面的构建与打包。',
    '',
    'import { Theme } from "../theme/tokens.slint";',
    'import { Icon, IconName } from "../theme/icons.slint";',
    '',
    'export component IconSheet inherits Window {',
    '    title: "Fluent 图标总览";',
    '    // 主题开关：单独编译的文件不会导出 Theme 全局，因此在 Slint 侧写入',
    '    in property <bool> dark: false;',
    '    init => { Theme.dark = root.dark; }',
    '    background: Theme.window-bg;',
    '    width: 620px;',
    ('    height: {0}px;' -f (150 + $rows.Count * 38)),
    '',
    '    VerticalLayout {',
    '        padding: Theme.space-4;',
    '        spacing: Theme.space-2;',
    '        alignment: start;',
    '        cross-axis-alignment: start;',
    '',
    '        Text {',
    ('            text: "Fluent UI System Icons · {0} 个图标（{1} 个 SVG） · 官方 24px 画布（{2} 个例外） · Regular 与 Filled 对照";' -f $rows.Count, ($rows.Count * 2), ($rows | Where-Object { $_.size -ne 24 }).Count),
    '            font-family: Theme.font-family;',
    '            font-size: Theme.font-body-large;',
    '            font-weight: Theme.weight-semibold;',
    '            color: Theme.text-primary;',
    '        }',
    '',
    '        GridLayout {',
    '            spacing: 2px;'
)

# 表头
Add-Lines (New-Cell -Row 0 -Col 0 -Width '160px' -Body @(
    'Text {',
    '    x: 0; y: (parent.height - self.height) / 2;',
    '    text: "名称";',
    '    font-family: Theme.font-family;',
    '    font-size: Theme.font-caption;',
    '    color: Theme.text-tertiary;',
    '}'
))
for ($c = 0; $c -lt $sheetCols.Count; $c++) {
    Add-Lines (New-Cell -Row 0 -Col ($c + 1) -Width '56px' -Body @(
        'Text {',
        '    x: 0; y: (parent.height - self.height) / 2;',
        ('    text: "{0}";' -f $sheetCols[$c].Title),
        '    font-family: Theme.font-family;',
        '    font-size: Theme.font-caption;',
        '    color: Theme.text-tertiary;',
        '}'
    ))
}

# 每个图标一行
for ($i = 0; $i -lt $rows.Count; $i++) {
    $r = $rows[$i]
    $row = $i + 1
    Add-Lines (New-Cell -Row $row -Col 0 -Width '160px' -Body @(
        'Text {',
        '    x: 0; y: (parent.height - self.height) / 2;',
        ('    text: "{0}";' -f $r.key),
        '    font-family: Theme.mono-font-family;',
        '    font-size: Theme.font-small;',
        '    color: Theme.text-secondary;',
        '}'
    ))
    for ($c = 0; $c -lt $sheetCols.Count; $c++) {
        Add-Lines (New-Cell -Row $row -Col ($c + 1) -Width '56px' -Body @(
            'Icon {',
            '    x: (parent.width - self.width) / 2;',
            '    y: (parent.height - self.height) / 2;',
            ('    name: IconName.{0};' -f $r.key),
            ('    filled: {0};' -f $sheetCols[$c].Filled),
            ('    size: {0}px;' -f $sheetCols[$c].Size),
            '    icon-color: Theme.text-primary;',
            '}'
        ))
    }
}

Add-Lines @(
    '        }',
    '    }',
    '}',
    ''
)
Set-Content -Path $sheetPath -Value ($sheetLines -join "`n") -Encoding utf8
Write-Host "已生成 $sheetPath"
# ------------------------------------------------------------------ 清理未引用的资源
# 清单是唯一事实来源：删掉目录里没被清单引用的 SVG，
# 避免改清单后遗留孤儿文件（它们不会被任何 @image-url 引用，只会混淆维护者）。
$keep = @{}
foreach ($r in $rows) { $keep[$r.regular] = $true; $keep[$r.filled] = $true }
$removed = @()
foreach ($f in Get-ChildItem $dest -Filter *.svg) {
    if (-not $keep.ContainsKey($f.Name)) { Remove-Item $f.FullName -Force; $removed += $f.Name }
}
if ($removed.Count -gt 0) {
    Write-Host ("已清理 {0} 个未引用资源: {1}" -f $removed.Count, ($removed -join ', '))
}
Write-Host ("最终 SVG 数量: {0}" -f (Get-ChildItem $dest -Filter *.svg).Count)
