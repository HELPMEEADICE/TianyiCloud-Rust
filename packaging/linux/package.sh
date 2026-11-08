#!/usr/bin/env bash
# 打包 Linux AppImage：AppDir + linuxdeploy（收集依赖）+ appimagetool。
#
# 用法：packaging/linux/package.sh <version> <target-triple> <arch-name> <appimage-arch> [out-dir]
# 例：  packaging/linux/package.sh 0.1.0 x86_64-unknown-linux-gnu linux-x64 x86_64 dist
#
# 说明：CI 里没有 FUSE，所以对 linuxdeploy / appimagetool 这两个 AppImage 工具都用
#      APPIMAGE_EXTRACT_AND_RUN=1 直接解包运行。
set -euo pipefail

VERSION="${1:-${VERSION:-}}"
TARGET="${2:?用法: package.sh <version> <target-triple> <arch-name> <appimage-arch> [out-dir]}"
ARCH_NAME="${3:-linux-x64}"
APPIMAGE_ARCH="${4:-x86_64}"
OUT_DIR="${5:-dist}"

[ -n "$VERSION" ] || { echo '缺少版本号' >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
APP_NAME="TianyiCloud"
BIN="$ROOT/target/$TARGET/release/tianyi-app"
[ -x "$BIN" ] || { echo "找不到编译产物: $BIN" >&2; exit 1; }

PYTHON="$(command -v python3 || command -v python || true)"
[ -n "$PYTHON" ] || { echo '找不到 python' >&2; exit 1; }

WORK="$ROOT/target/package-linux/$ARCH_NAME"
APPDIR="$WORK/AppDir"
rm -rf "$WORK"
mkdir -p "$APPDIR/usr/bin" \
    "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons/hicolor/512x512/apps"

echo '[package] 生成图标'
"$PYTHON" "$ROOT/packaging/ci/make-icon.py" "$WORK/icons" >/dev/null

echo '[package] 组装 AppDir'
install -m 755 "$BIN" "$APPDIR/usr/bin/tianyi-app"
install -m 644 "$ROOT/packaging/linux/tianyi-cloud.desktop" \
    "$APPDIR/usr/share/applications/tianyi-cloud.desktop"
install -m 644 "$WORK/icons/icon.png" \
    "$APPDIR/usr/share/icons/hicolor/512x512/apps/tianyi-cloud.png"
install -m 644 "$WORK/icons/icon.png" "$APPDIR/tianyi-cloud.png"
ln -sf tianyi-cloud.png "$APPDIR/.DirIcon"

cat >"$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib/$(uname -m)-linux-gnu:${LD_LIBRARY_PATH:-}"
exec "$HERE/usr/bin/tianyi-app" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

# ---------------------------------------------------------------- 下载打包工具
TOOLS="$ROOT/target/package-tools"
mkdir -p "$TOOLS"

fetch() { # fetch <dest> <url>...
    local dest="$1"
    shift
    if [ -x "$dest" ]; then
        return 0
    fi
    local url
    for url in "$@"; do
        echo "[package] 下载 $url"
        if curl -fsSL --retry 3 --retry-delay 5 --connect-timeout 20 -o "$dest.part" "$url"; then
            mv "$dest.part" "$dest"
            chmod +x "$dest"
            return 0
        fi
        rm -f "$dest.part"
    done
    echo "[package] 下载失败: $*" >&2
    return 1
}

export ARCH="$APPIMAGE_ARCH"
export APPIMAGE_EXTRACT_AND_RUN=1
export NO_APPSTREAM=1

LINUXDEPLOY="$TOOLS/linuxdeploy-$APPIMAGE_ARCH.AppImage"
fetch "$LINUXDEPLOY" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$APPIMAGE_ARCH.AppImage"

APPIMAGETOOL="$TOOLS/appimagetool-$APPIMAGE_ARCH.AppImage"
fetch "$APPIMAGETOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$APPIMAGE_ARCH.AppImage" \
    "https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-$APPIMAGE_ARCH.AppImage"

# ---------------------------------------------------------------- 收集依赖
echo '[package] linuxdeploy 收集运行时依赖'
"$LINUXDEPLOY" --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/tianyi-app" \
    --desktop-file "$APPDIR/usr/share/applications/tianyi-cloud.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/512x512/apps/tianyi-cloud.png"

# ---------------------------------------------------------------- 生成 AppImage
echo '[package] appimagetool 生成 AppImage'
mkdir -p "$ROOT/$OUT_DIR"
OUT="$ROOT/$OUT_DIR/${APP_NAME}-${VERSION}-${ARCH_NAME}.AppImage"
rm -f "$OUT"
"$APPIMAGETOOL" "$APPDIR" "$OUT"

[ -f "$OUT" ] || { echo 'AppImage 生成失败' >&2; exit 1; }
chmod +x "$OUT"
ls -lh "$OUT"
echo "[package] 完成: $OUT"
