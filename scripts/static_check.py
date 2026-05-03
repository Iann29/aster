#!/usr/bin/env python3
"""Small dependency-free static checks for this artifact."""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

PAIRS = {"(": ")", "[": "]", "{": "}"}
CLOSERS = {v: k for k, v in PAIRS.items()}


def check_balanced(path: Path) -> None:
    stack: list[tuple[str, int, int]] = []
    in_string = False
    escape = False
    for lineno, line in enumerate(path.read_text().splitlines(), 1):
        for col, ch in enumerate(line, 1):
            if in_string:
                if escape:
                    escape = False
                elif ch == "\\":
                    escape = True
                elif ch == '"':
                    in_string = False
                continue
            if ch == '"':
                in_string = True
            elif ch in PAIRS:
                stack.append((ch, lineno, col))
            elif ch in CLOSERS:
                if not stack or stack[-1][0] != CLOSERS[ch]:
                    raise SystemExit(f"{path}:{lineno}:{col}: unmatched {ch}")
                stack.pop()
    if stack:
        ch, lineno, col = stack[-1]
        raise SystemExit(f"{path}:{lineno}:{col}: unclosed {ch}")


def main() -> None:
    for suffix in ("*.rs", "*.proto", "*.toml"):
        for path in ROOT.rglob(suffix):
            check_balanced(path)
    proto = (ROOT / "proto" / "aster.proto").read_text()
    required = [
        'syntax = "proto3";',
        'package aster.runner.v1;',
        'service AsterExecution',
        'rpc Execute',
        'rpc Hydrate',
    ]
    for needle in required:
        if needle not in proto:
            raise SystemExit(f"proto missing {needle}")
    print("static_check: balanced delimiters and required proto declarations OK")


if __name__ == "__main__":
    main()
