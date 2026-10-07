#!/usr/bin/env python3
"""Require a scoped theme pin in tests that read or mutate active theme state.

This is a lexical guard, not a Rust parser: it checks direct theme::function
references and unqualified theme setters in #[test]/#[tokio::test] bodies.
Indirect helper calls still need review. Immutable theme catalog lookups are
exempt. Comments and literals are masked before balancing test-body braces.
"""
import re
import sys
from pathlib import Path


TRIVIA = re.compile(
    r'//[^\n]*|/\*.*?\*/|\br(\#*)".*?"\1|"(?:[^"\\]|\\.)*"'
    r"|'(?:[^'\\\n]|\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.))'",
    re.DOTALL,
)
TEST = re.compile(
    r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]\s*"
    r"(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+(\w+)[^{]*\{"
)
THEME = re.compile(r"\btheme\s*::\s*([a-z_]\w*)\b(?!\s*::)")
SETTER = re.compile(
    r"\b(?:set_theme_\w+|apply_theme_role_overrides|clear_theme_role_overrides|"
    r"set_border_color_override|apply_border_color_overrides|apply_startup_theme_state)\s*\("
)
EXEMPT = {
    "pin_theme_state", "with_theme_state", "test_render_guard", "test_rgb_channels",
    "theme_count", "theme_display_name", "theme_key", "theme_swatch",
}
PIN = re.compile(
    r"\blet\s+(?!_\s*=)\w+\s*=\s*(?:[\w]+::)*"
    r"(?:pin_theme_state|theme_test_guard)\s*\(\s*\)\s*;"
)
WRAPPER = re.compile(r"\bwith_theme_state\s*\(\s*\|\|\s*\{")


def mask(text):
    return TRIVIA.sub(lambda m: re.sub(r"[^\n]", " ", m[0]), text)


def closing_brace(code, start):
    depth = 1
    for pos in range(start, len(code)):
        depth += (code[pos] == "{") - (code[pos] == "}")
        if depth == 0:
            return pos
    raise ValueError("unbalanced test body")


def test_bodies(text):
    code = mask(text)
    for match in TEST.finditer(code):
        start = match.end()
        end = closing_brace(code, start)
        yield match[1], start, code[start:end]


def unpinned(body):
    reads = [m.start() for m in THEME.finditer(body) if m[1] not in EXEMPT]
    reads.extend(m.start() for m in SETTER.finditer(body))
    if not reads:
        return False
    # A pin must be retained in a named binding in the test's outer scope,
    # before any theme read, so it survives the render and later assertions.
    for pin in PIN.finditer(body):
        prefix = body[:pin.start()]
        if prefix.count("{") == prefix.count("}") and pin.end() <= min(reads):
            return False
    for wrapper in WRAPPER.finditer(body):
        end = closing_brace(body, wrapper.end())
        if all(wrapper.end() <= read < end for read in reads):
            return False
    return True


def scan(path):
    text = path.read_text(encoding="utf-8")
    return [(text.count("\n", 0, start) + 1, name)
            for name, start, body in test_bodies(text) if unpinned(body)]


def main(argv):
    roots = [Path(arg) for arg in argv] or [Path("crates/rsi/src")]
    findings = 0
    for root in roots:
        for path in [root] if root.is_file() else sorted(root.rglob("*.rs")):
            for line, name in scan(path):
                findings += 1
                print(f"{path}:{line}: {name}: theme state is not pinned")
    if findings:
        print("Hold `let _theme = crate::ui::theme::pin_theme_state();` from the")
        print("start of the test, or wrap its theme reads in `with_theme_state`.")
    return int(findings > 0)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
