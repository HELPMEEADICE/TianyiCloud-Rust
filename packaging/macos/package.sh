#!/usr/bin/env bash
# 打包 macOS dmg：TianyiCloud.app（ad-hoc 签名）+ /Applications 快捷方式。
#
# 用法：packaging/macos/package.sh <version> <target-triple> <arch-name> [out-dir]
#   VERSION / BUILD_VERSION 也可由环境变量提供（build.yml 就是这么传的）
set -euo pipefail

VERSION="${1:-${VERSION:-}}"
TARGET="${2:?用法: package.sh <version> <target-triple> <arch-name> [out-dir]}"
ARCH_NAME="${3:-macos-arm64}"
OUT_DIR="${4:-dist}"
BUILD_VERSION="${BUILD_VERSION:-$VERSION}"

[ -n "$VERSION" ] || { echo '缺少版本号' >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP_NAME="TianyiCloud"
BIN="$ROOT/target/$TARGET/release/tianyi-app"
[ -x "$BIN" ] || { echo "找不到编译产物: $BIN" >&2; exit 1; }

PYTHON="$(command -v python3 || command -v python || true)"
[ -n "$PYTHON" ] || { echo '找不到 python' >&2; exit 1; }

WORK="$ROOT/target/package-macos/$ARCH_NAME"
rm -rf "$WORK"
mkdir -p "$WORK"

echo '[package] 生成图标'
"$PYTHON" "$ROOT/packaging/ci/make-icon.py" "$WORK/icons" >/dev/null

echo '[package] 组装 .app'
APP="$WORK/$APP_NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$BIN" "$APP/Contents/MacOS/tianyi-app"
install -m 644 "$WORK/icons/icon.icns" "$APP/Contents/Resources/AppIcon.icns"
sed -e "s/__VERSION__/${VERSION}/g" -e "s/__BUILD_VERSION__/${BUILD_VERSION}/g" \
    "$ROOT/packaging/macos/Info.plist" >"$APP/Contents/Info.plist"
printf 'APPL????' >"$APP/Contents/PkgInfo"

# arm64 上未签名的可执行文件会被系统直接杀掉，这里做 ad-hoc 签名（无需开发者证书）
if command -v codesign >/dev/null 2>&1; then
    echo '[package] ad-hoc 签名'
    codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo '[package] ad-hoc 签名失败（继续）' >&2
fi

echo '[package] 生成 dmg'
DMG_ROOT="$WORK/dmg"
mkdir -p "$DMG_ROOT"
cp -R "$APP" "$DMG_ROOT/"
ln -s /Applications "$DMG_ROOT/Applications"

mkdir -p "$ROOT/$OUT_DIR"
DMG="$ROOT/$OUT_DIR/${APP_NAME}-${VERSION}-${ARCH_NAME}.dmg"
rm -f "$DMG"

for attempt in 1 2 3; do
    if hdiutil create -volname "$APP_NAME $VERSION" -srcfolder "$DMG_ROOT" -fs HFS+ -format UDZO -ov "$DMG"; then
        break
    fi
    echo "[package] hdiutil 第 $attempt 次失败，重试" >&2
    sleep 3
done

[ -f "$DMG" ] || { echo 'DMG 生成失败' >&2; exit 1; }
ls -lh "$DMG"
echo "[package] 完成: $DMG"
