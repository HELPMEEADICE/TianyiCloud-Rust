#!/usr/bin/env python3
"""把「GitHub Actions 专用」的激进 release profile 注入 workspace Cargo.toml。

设计约束
--------
* 仓库里提交的 Cargo.toml 保持原样（本地 `cargo build --release` 不受影响）；
* 只有 CI 构建会调用本脚本，运行完即用的是一次性的工作区副本；
* 幂等：已存在的 `[profile.release]` 表会被整段替换，重复执行不会出现重复的表。

用法
----
    python packaging/ci/apply-ci-profile.py [Cargo.toml 路径]
"""

from __future__ import annotations

import pathlib
import sys

# Windows 上 stdout 默认是 cp1252，直接打印中文会 UnicodeEncodeError
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

HEADER = "[profile.release]"

PROFILE = """[profile.release]
# 仅 GitHub Actions 构建使用：体积优先 + 全量 LTO
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "debuginfo"
debug = false
incremental = false
overflow-checks = false
"""

REQUIRED_KEYS = (
    'opt-level = "z"',
    "lto = \"fat\"",
    "codegen-units = 1",
    'panic = "abort"',
    'strip = "debuginfo"',
    "debug = false",
    "incremental = false",
    "overflow-checks = false",
)


def strip_release_table(lines: list[str]) -> list[str]:
    """移除所有 `[profile.release]` 表（表头直到下一个表头之前）。"""
    kept: list[str] = []
    inside = False
    for line in lines:
        stripped = line.strip()
        looks_like_header = stripped.startswith("[") and stripped.endswith("]")
        if looks_like_header:
            inside = stripped == HEADER
            if inside:
                continue
        if not inside:
            kept.append(line)
    return kept


def main(argv: list[str]) -> int:
    manifest = pathlib.Path(argv[1] if len(argv) > 1 else "Cargo.toml")
    if not manifest.is_file():
        print(f"[ci-profile] 找不到 {manifest}", file=sys.stderr)
        return 1

    original = manifest.read_text(encoding="utf-8")
    body = strip_release_table(original.splitlines())

    # 去掉尾部空行，保证 profile 表是目前文件的最后一张表
    while body and not body[-1].strip():
        body.pop()

    manifest.write_text("\n".join(body) + "\n\n" + PROFILE, encoding="utf-8")

    written = manifest.read_text(encoding="utf-8")
    if written.count(HEADER) != 1:
        print("[ci-profile] 注入后 profile.release 表数量异常", file=sys.stderr)
        return 1
    missing = [key for key in REQUIRED_KEYS if key not in written]
    if missing:
        print(f"[ci-profile] 注入不完整，缺少: {missing}", file=sys.stderr)
        return 1

    print(f"[ci-profile] 已注入 GitHub Actions 专用 release profile -> {manifest}")
    for line in PROFILE.splitlines():
        print(f"  {line}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
