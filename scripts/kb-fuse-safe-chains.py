#!/usr/bin/env python3
"""从 safe-chains（MIT OR Apache-2.0）融合命令风险标注到 clum 知识库。

用法:
    python3 scripts/kb-fuse-safe-chains.py <safe-chains 源码目录> [输出路径] [源版本]

源版本缺省时尝试从上游 git HEAD 读取，读不到则写 unknown。

safe-chains 的 commands/**/*.toml 为每条命令标注一个 level:
    Inert     无副作用   -> allow
    SafeRead  只读       -> allow
    SafeWrite 会写状态   -> ask
其余等级（更高权限）一律 -> ask（保守，绝不因为不认识就放行）。

同一条命令在多处出现时取**最严**等级。只导出简单命令名（跳过含 '/' 或 '@'
的包名/路径式条目，我们的匹配按 basename，这类条目只会造成噪声）。

产物是生成文件，**请勿手工编辑**——重跑本脚本即可。
"""

from __future__ import annotations

import pathlib
import sys

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore

LEVEL_TO_EFFECT = {"Inert": "allow", "SafeRead": "allow", "SafeWrite": "ask"}
EFFECT_RANK = {"allow": 0, "ask": 1, "deny": 2}

# 上游语义是「编码助手沙箱内是否无副作用」，不是「生产执行闸门能否放行」。
# 以下命令即使被标为 Inert/SafeRead，也一律收紧到 ask（与 data/commands.toml 的人
# 工层保持一致；两处改动必须同步）。见 commands.toml 的同名注释。
FORCE_ASK = {
    # 解释器 / 任意代码执行
    "python3", "perl", "ruby", "node", "nodejs", "php", "lua", "tclsh", "expect",
    "R", "Rscript", "awk", "gawk", "mawk", "nawk", "zsh",
    "ipython", "nu", "deno", "julia", "erl", "groovy", "scala", "sbcl", "swipl",
    "csi", "ts-node", "nodemon", "pry", "java", "mix", "phpunit", "luarocks",
    "dc",
    # 交互式 shell escape
    "vim", "nvim", "emacs", "nano", "less", "more", "man", "screen", "tmux", "watch",
    # 网络外联 / 反弹 shell
    "nc", "ncat", "netcat", "socat", "ssh", "tcpdump", "mosh", "autossh",
    # 数据库客户端（可执行 shell / 破坏数据）
    "sqlite3", "mysql", "psql", "redis-cli", "mongo", "mongosh",
    # 包管理器 / 构建（可执行任意代码）
    "pip", "pip3", "npm", "npx", "make",
    # 主机管理
    "kill", "pkill", "killall", "crontab", "sysctl", "mount",
}


def source_version(src: pathlib.Path, explicit: str | None) -> str:
    """返回上游版本串。显式参数优先；否则尝试 git HEAD；都没有则标注 unknown。"""
    if explicit:
        return explicit
    try:
        import subprocess

        out = subprocess.run(
            ["git", "-C", str(src), "rev-parse", "--short", "HEAD"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if out.returncode == 0 and out.stdout.strip():
            return out.stdout.strip()
    except Exception:  # noqa: BLE001 - 版本锚定是尽力而为，不能阻断生成
        pass
    return "unknown"


def collect_levels(node) -> list[str]:
    """递归收集一个 command 条目里出现的所有 level 值（可能分布在子表中）。"""
    found: list[str] = []
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "level" and isinstance(value, str):
                found.append(value)
            else:
                found.extend(collect_levels(value))
    elif isinstance(node, list):
        for item in node:
            found.extend(collect_levels(item))
    return found


def strictest_effect(levels: list[str]) -> str:
    if not levels:
        return "ask"
    effects = [LEVEL_TO_EFFECT.get(level, "ask") for level in levels]
    return max(effects, key=lambda e: EFFECT_RANK[e])


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 1

    src = pathlib.Path(sys.argv[1])
    out = pathlib.Path(
        sys.argv[2]
        if len(sys.argv) > 2
        else "crates/clum-command-kb/data/generated.toml"
    )

    commands_dir = src / "commands"
    if not commands_dir.is_dir():
        print(
            f"error: {commands_dir} not found (point this at the safe-chains checkout)"
        )
        return 1

    entries: dict[str, str] = {}
    files = 0
    for path in sorted(commands_dir.rglob("*.toml")):
        try:
            data = tomllib.loads(path.read_text(encoding="utf-8"))
        except Exception as exc:  # noqa: BLE001 - 单个文件坏了不该中断整批
            print(f"skip {path}: {exc}", file=sys.stderr)
            continue
        files += 1
        for cmd in data.get("command", []):
            name = cmd.get("name")
            if not isinstance(name, str) or not name:
                continue
            if "/" in name or "@" in name:
                continue
            effect = strictest_effect(collect_levels(cmd))
            if name in FORCE_ASK:
                effect = "ask"
            prev = entries.get(name)
            if prev is None or EFFECT_RANK[effect] > EFFECT_RANK[prev]:
                entries[name] = effect

    version = source_version(src, sys.argv[3] if len(sys.argv) > 3 else None)

    lines = [
        "# 本文件由 scripts/kb-fuse-safe-chains.py 生成，请勿手工编辑。",
        "# 数据来源: safe-chains (MIT OR Apache-2.0)",
        f"# source-version: {version}",
        "# 映射: Inert / SafeRead -> allow（只读）; SafeWrite -> ask（会写）; 其余 -> ask",
        "# FORCE_ASK 覆盖: 解释器 / 交互式 shell escape / 网络外联 / 数据库 / 主机管理 -> ask",
        "# 同命令多处标注时取最严。被 data/commands.toml 与 data/overlay.toml 的同名条目覆盖。",
        "",
        "schema_version = 1",
        "",
    ]
    for name in sorted(entries):
        lines += [
            "[[command]]",
            f'name = "{name}"',
            f'effect = "{entries[name]}"',
            'source = "safe-chains(mit-or-apache-2.0)"',
            "",
        ]

    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text("\n".join(lines), encoding="utf-8")

    allow = sum(1 for v in entries.values() if v == "allow")
    ask = sum(1 for v in entries.values() if v == "ask")
    print(
        f"parsed {files} files -> {out}: {len(entries)} commands ({allow} allow, {ask} ask)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
