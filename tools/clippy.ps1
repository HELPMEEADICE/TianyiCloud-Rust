# 用与 rustc 版本匹配的 clippy 驱动跑 lint
#
# 本机（MSYS2 UCRT64）的 cargo-clippy.exe 版本比 rustc 新，直接 `cargo clippy`
# 会用错位的内置驱动，并在编译 build script 时按 MSVC host 解析而报
# `linker link.exe not found`。这里绕过 cargo-clippy，直接把匹配版本的
# clippy-driver 作为 `RUSTC_WORKSPACE_WRAPPER`，等价于
#   cargo clippy --workspace --all-targets -- -D warnings
# 只是对 workspace 成员生效，不影响依赖。
#
# 用法：pwsh -File tools/clippy.ps1
$ErrorActionPreference = 'Stop'

$driver = (Get-Command clippy-driver.exe -ErrorAction SilentlyContinue).Source
if (-not $driver) { throw '找不到 clippy-driver.exe（需要安装 rustup/clippy 组件）' }

$rustcVersion = (& rustc -V) -replace '^rustc ', ''
$driverVersion = (& $driver --version) -replace '^clippy ', ''
Write-Host "rustc $rustcVersion / clippy-driver $driverVersion"

$env:RUSTC_WORKSPACE_WRAPPER = $driver
$env:CLIPPY_ARGS = '-Dwarnings'
cargo check --workspace --all-targets
exit $LASTEXITCODE
