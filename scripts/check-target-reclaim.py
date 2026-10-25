#!/usr/bin/env python3
"""Keep literal target writers in sync with the build-cache reclaim policy.

This is a conservative source guard, not a Rust/shell interpreter: path literals
are checked even when only read. It follows scoped target-root assignments and
literal joins (Rust join, Python / and os.path.join, shell variable expansion).
Paths proven to descend from temporary-directory fixtures are excluded.
Computed entry names and interprocedural aliases need review. Exceptions are
file/name pairs with reasons in target-reclaim-retained.json; they do NOT make
an entry reclaimable. Prefer target/.rsi-tmp/ for disposable new scratch.
"""

import argparse
import ast
from bisect import bisect_right
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import sys
import tomllib


POLICY = "crates/rsid-store/src/sandbox/target_reclaim.rs"
SHARED = "crates/rsi-common/src/sandbox_storage.rs"
RETAINED = "scripts/target-reclaim-retained.json"
NEXTEST = ".config/nextest.toml"
ROOTS = ("crates", "scripts", "desktop/scripts", ".config")
NAME = r"[A-Za-z0-9_.-]+"
BYTE = rf'b"({NAME})"'
ATOM = rf'(?:b"{NAME}"|[A-Z][A-Z0-9_]*)'


def compact(text):
    return re.sub(r"\s+", "", re.sub(r"//[^\n]*", "", text))


def function(source, name):
    match = re.search(rf"fn {name}\([^\n]*\) -> bool \{{(.*?)\n\}}", source, re.S)
    if not match:
        raise ValueError(f"cannot read {name} in {POLICY}")
    return compact(match[1])


@dataclass
class Policy:
    directories: set
    files: set
    fixture_suffix: str
    idle_prefix: str
    idle_suffix: str

    def allows(self, name):
        pid = name.removeprefix(self.idle_prefix).removesuffix(self.idle_suffix)
        return (name in self.directories | self.files
                or (name.endswith(self.fixture_suffix) and len(name) > len(self.fixture_suffix))
                or (name.startswith(self.idle_prefix) and name.endswith(self.idle_suffix)
                    and bool(re.fullmatch(r"[0-9]+", pid))))


def read_policy(root):
    """Parse the complete predicate, failing closed if its grammar changes.

    Only the *shape* is mirrored here; all names, constants and suffixes come
    from Rust. A new match arm is automatic; a new predicate requires teaching
    this reader its semantics rather than silently accepting a partial list.
    """
    source = (root / POLICY).read_text()
    body = function(source, "is_reclaimable_target_entry")
    pattern = (
        rf'constFIXTURE_SUFFIX:&\[u8\]={BYTE};'
        r'ifkind==SFlag::S_IFDIR\{matches!\(name,'
        rf'({ATOM}(?:\|{ATOM})*)\)'
        r'\|\|name==E2E_ARTIFACTS_DIR_NAME.as_bytes\(\)'
        r'\|\|\(name.len\(\)>FIXTURE_SUFFIX.len\(\)&&name.ends_with\(FIXTURE_SUFFIX\)\)\}'
        r'elseifkind==SFlag::S_IFREG\{is_idle_cost_scratch\(name\)'
        r'\|\|is_python_discover_scratch\(name\)\|\|matches!\(name,'
        rf'({ATOM}(?:\|{ATOM})*)\)\}}else\{{false\}}'
    )
    match = re.fullmatch(pattern, body)
    if not match:
        raise ValueError(f"unsupported reclaim predicate in {POLICY}; update read_policy in {__file__}")
    suffix, dirs, files = match.groups()
    constants_source = compact(source + "\n" + (root / SHARED).read_text())

    def names(arms):
        result = set()
        for arm in arms.split("|"):
            if arm.startswith('b"'):
                result.add(arm[2:-1])
            else:
                constant = re.search(rf'const{arm}:(?:&\[u8\]|&str)=b?"({NAME})";', constants_source)
                if not constant:
                    raise ValueError(f"unresolved reclaim constant {arm} in {POLICY}")
                result.add(constant[1])
        return result

    directories = names(dirs + "|E2E_ARTIFACTS_DIR_NAME")
    regular = names(files)
    discover = function(source, "is_python_discover_scratch")
    match = re.fullmatch(rf'matches!\(name,({ATOM}(?:\|{ATOM})*)\)', discover)
    if not match:
        raise ValueError(f"unsupported is_python_discover_scratch in {POLICY}")
    regular.update(names(match[1]))
    idle = function(source, "is_idle_cost_scratch")
    match = re.fullmatch(
        rf'letSome\(pid\)=name.strip_prefix\({BYTE}\)'
        rf'.and_then\(\|rest\|rest.strip_suffix\({BYTE}\)\)'
        r'else\{returnfalse;\};!pid.is_empty\(\)&&pid.iter\(\).all\(u8::is_ascii_digit\)', idle)
    if not match:
        raise ValueError(f"unsupported is_idle_cost_scratch in {POLICY}")
    return Policy(directories, regular, suffix, *match.groups())


# A literal component must end at a path/token boundary. In particular do not
# mistake target/cloud-$sha for a literal entry named "cloud-".
COMPONENT = rf'({NAME})(?=/|[\s\x22\x27);,\\}}\]]|$)'
DIRECT = re.compile(rf'(?<![\w-])(?:target/|\$(?:CARGO_TARGET_DIR|\{{CARGO_TARGET_DIR\}})/){COMPONENT}')
ASSIGN = re.compile(r'\b(?:let\s+(?:mut\s+)?)?([A-Za-z_]\w*)(?:\s*:\s*[^=;\n]+)?\s*=(?!=)\s*([^;\n]*(?:\n\s*\.[^;\n]*)*)')
TARGET_ROOT = re.compile(r'''(?:\.join\(\s*["'](?:\.\./)*target["']\s*\)|(?:Path|PathBuf)(?:::\w+)?\(\s*["'](?:\.\./)*target["']\s*\)|/\s*["']target["']|["']CARGO_TARGET_DIR["']|(?:^|["/])target["']?\}?["']?$)''')

# Only known path-preserving operations establish fixture provenance. Unknown
# calls and paths escaping the temporary directory keep the conservative scan.
PATH_STEP = r'''(?:\s*\.(?:path|canonicalize|unwrap|to_path_buf|clone|as_path)\(\s*\)|\s*\.expect\(\s*"[^"\n]*"\s*\)|\s*\.name|\s*\.join\(\s*["'][^"'\n]*["']\s*\)|\s*/\s*["'][^"'\n]*["']|\?)'''
TEMP_ROOT = r'''(?:tempfile::tempdir\(\s*\)|(?:tempfile::)?TempDir::new\(\s*\)|(?:crate::test_support::)?disk_backed_tempdir\(\s*"[^"\n]*"\s*\)|tempfile\.TemporaryDirectory\(\s*\))'''
FIXTURE_PATH = re.compile(rf'(?:{TEMP_ROOT}|\b[A-Za-z_]\w*)(?:{PATH_STEP})*')


def lexical_scopes(code):
    """Return a scope identity at each offset, ignoring quoted braces.

    Python functions use indentation (dict/set braces are expressions). Rust
    and shell use braces. This remains lexical: no interprocedural inference.
    """
    try:
        tree = ast.parse(code)
    except SyntaxError:
        tree = None
    events, parameters = [], []
    if tree is not None:
        lines = [0]
        lines.extend(m.end() for m in re.finditer("\n", code))
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                start = lines[node.body[0].lineno - 1] + node.body[0].col_offset
                events.extend(((start, True),
                               (lines[node.end_lineno - 1] + node.end_col_offset, False)))
                if not isinstance(node, ast.ClassDef):
                    arguments = node.args
                    names = arguments.posonlyargs + arguments.args + arguments.kwonlyargs
                    names += [arg for arg in (arguments.vararg, arguments.kwarg) if arg]
                    parameters.extend((start, arg.arg) for arg in names)
        events.sort()
    else:
        tokens = r'''"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\\n])*'|//[^\n]*|/\*.*?\*/|\#[^\n]*|[{}]'''
        for token in re.finditer(tokens, code, re.S):
            if token[0] in ("{", "}"):
                events.append((token.end(), token[0] == "{"))
        for header in re.finditer(r'\bfn\s+\w+[^;{]*\{', code):
            parameters.extend((header.end(), name) for name in
                              re.findall(r'\b(\w+)\s*:\s*[^:]', header[0]))
    offsets, scopes, stack = [0], [()], []
    for identity, (offset, opening) in enumerate(events):
        if opening:
            stack.append(identity)
        elif stack:
            stack.pop()
        offsets.append(offset)
        scopes.append(tuple(stack))
    return (lambda offset: scopes[bisect_right(offsets, offset) - 1]), parameters


def local_paths(code):
    scope_at, parameters = lexical_scopes(code)
    bindings = {}

    def lookup(variable, offset):
        scope = scope_at(offset)
        for end, bound_scope, kind in reversed(bindings.get(variable, [])):
            if end <= offset and scope[:len(bound_scope)] == bound_scope:
                return kind
        return "target" if variable == "CARGO_TARGET_DIR" else None

    def fixture(expr, offset):
        expr = expr.strip().removeprefix("&").strip()
        # Path(...) is a path-preserving Python wrapper.
        if expr.startswith("Path(") and expr.endswith(")"):
            expr = expr[5:-1]
        match = FIXTURE_PATH.fullmatch(expr)
        if not match:
            return False
        if not re.match(TEMP_ROOT, expr):
            base = re.match(r"\w+", expr)[0]
            if lookup(base, offset) != "fixture":
                return False
        components = re.findall(r'''(?:\.join\(\s*|/\s*)["']([^"']*)["']''', expr)
        return all(not path.startswith(("/", "\\")) and ".." not in path.replace("\\", "/").split("/")
                   and not re.match(r"[A-Za-z]:", path) for path in components)

    events = [(m.start(), None, m) for m in ASSIGN.finditer(code)]
    events.extend((offset, name, None) for offset, name in parameters)
    for offset, parameter, assignment in sorted(events, key=lambda event: (event[0], event[2] is not None)):
        if assignment is None:
            # `target` and the Cargo environment name are conservative roots
            # without interprocedural provenance. Other parameters clear
            # fixtures, but cannot clear an already visible production target.
            kind = "target" if (parameter in ("target", "CARGO_TARGET_DIR")
                                or lookup(parameter, offset) == "target") else None
            bindings.setdefault(parameter, []).append((offset, scope_at(offset), kind))
            continue
        variable, expr = assignment.groups()
        kind = None
        if fixture(expr, assignment.start()):
            kind = "fixture"
        else:
            roots = list(TARGET_ROOT.finditer(expr))
            if roots:
                tail = root_tail(expr, roots[-1])
                if not re.search(r"\.join\(|/\s*[\"']", tail):
                    kind = "target"
            elif lookup(expr.strip(' \"\'{}$'), assignment.start()) == "target":
                kind = "target"
            if lookup(variable, offset) == "target":
                kind = "target"
            elif kind is None and not roots:
                # A wrapper or relative-root normalization of another alias
                # may still return a target root. Do not infer roots from
                # child joins, parent paths, or arbitrary mentions of aliases.
                reference = re.fullmatch(
                    r'\s*(?:[A-Za-z_]\w*\(\s*|Path\([^\n]*\)\s*/\s*)([A-Za-z_]\w*)\s*\)?\s*', expr)
                if reference and lookup(reference[1], offset) == "target":
                    kind = "target"
        # Only proven fixtures may clear a target; unknown bindings still
        # shadow fixture provenance, including in nested blocks.
        bindings.setdefault(variable, []).append(
            (assignment.end(), scope_at(assignment.start()), kind))

    # Hide only proven fixture path expressions, preserving all source offsets
    # and newlines used by diagnostics and by the lexical binding lookup.
    masked = list(code)
    for match in FIXTURE_PATH.finditer(code):
        if fixture(match[0], match.start()):
            masked[match.start():match.end()] = ["\n" if c == "\n" else " " for c in match[0]]
    return "".join(masked), lookup


def root_tail(expr, root):
    tail = expr[root.end():]
    if root[0] in ('"CARGO_TARGET_DIR"', "'CARGO_TARGET_DIR'"):
        # Consume the whole lookup, including nested default arguments. A
        # Path.home() / ".cargo/shared-target" fallback is itself a root.
        depth = 0
        for token in re.finditer(r'''"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[()[\]]''', tail):
            if token[0] in ("(", "["):
                depth += 1
            elif token[0] in (")", "]"):
                if depth == 0:
                    return tail[token.end():]
                depth -= 1
    return tail


def entries(text):
    if "target" not in text and "CARGO_TARGET_DIR" not in text:
        return []
    # Preserve newlines for file:line diagnostics. Inline comments are left
    # alone because shell and generated source often contain quoted '#'.
    code = "\n".join("" if line.lstrip().startswith(("#", "//", "*")) else line
                     for line in text.splitlines())
    code, lookup = local_paths(code)
    hits = {(m.start(), m[1]) for m in DIRECT.finditer(code)}
    # Chained joins also occur directly inside create_dir_all/write calls.
    for root in TARGET_ROOT.finditer(code):
        child = re.match(rf'\s*\.join\(\s*["\x27]{COMPONENT}', code[root.end():])
        if child:
            hits.add((root.end() + child.start(), child[1]))
    direct_python = rf'''os\.path\.join\(\s*(?:os\.environ|env)\[\s*["']CARGO_TARGET_DIR["']\s*\]\s*,\s*["']{COMPONENT}'''
    hits.update((m.start(), m[1]) for m in re.finditer(direct_python, code))
    # Collect lexical aliases of a target root. Child paths are deliberately
    # not aliases: target.join("debug").join("deps") only contributes debug.
    assignments = list(ASSIGN.finditer(code))
    for assignment in assignments:
        _, expr = assignment.groups()
        roots = list(TARGET_ROOT.finditer(expr))
        for root in roots[-1:]:
            tail = root_tail(expr, root)
            join = re.search(rf'\.join\(\s*["\x27]{COMPONENT}|/\s*["\x27]{COMPONENT}', tail)
            if join:
                name = next(group for group in join.groups() if group is not None)
                if name not in ("..", "target"):
                    hits.add((assignment.start(2) + root.start(), name))
    alias = r'(?P<alias>[A-Za-z_]\w*)'
    for pattern in (
        rf'\b{alias}\s*\.join\(\s*["\x27]{COMPONENT}',
        rf'\b{alias}\s*/\s*["\x27]{COMPONENT}',
        rf'\b{alias}\s*\+\s*["\x27]/{COMPONENT}',
        rf'os\.path\.join\(\s*{alias}\s*,\s*["\x27]{COMPONENT}',
        rf'\$\{{?{alias}\}}?/{COMPONENT}',
    ):
        hits.update((m.start(), m[2]) for m in re.finditer(pattern, code)
                    if lookup(m["alias"], m.start()) == "target")
    return sorted({(code.count("\n", 0, offset) + 1, name) for offset, name in hits
                   if name not in (".", "..")})


def nextest_entries(text):
    # Nextest's default store is target/nextest; profile junit paths (such as
    # test-timings/junit.xml) are relative to that store's profile directory.
    # https://nexte.st/docs/configuration/reference/#storedir
    config = tomllib.loads(text)
    directory = config.get("store", {}).get("dir", "target/nextest")
    if not isinstance(directory, str):
        raise ValueError(f"nextest store directory in {NEXTEST} must be a string")
    return entries(directory)


def check(root):
    policy = read_policy(root)
    retained = json.loads((root / RETAINED).read_text())
    for path, entry in retained.items():
        if (not isinstance(entry, dict) or not isinstance(entry.get("reason"), str)
                or not entry["reason"].strip() or not isinstance(entry.get("entries"), list)
                or not all(isinstance(name, str) and re.fullmatch(NAME, name) for name in entry["entries"])):
            raise ValueError(f"{RETAINED}: {path} needs explicit entry names and a retention reason")
    findings = []
    for directory in ROOTS:
        for parent, directories, files in os.walk(root / directory):
            directories[:] = sorted(set(directories) - {"target", "node_modules", ".git", "__pycache__"})
            for filename in sorted(files):
                path = Path(parent) / filename
                relative = path.relative_to(root).as_posix()
                if path.is_symlink() or (relative != NEXTEST and path.suffix not in ("", ".rs", ".sh", ".py", ".mjs", ".js", ".cjs", ".ts")):
                    continue
                writer_entries = nextest_entries(path.read_text()) if relative == NEXTEST else entries(path.read_text())
                for line, name in writer_entries:
                    if not policy.allows(name) and name not in retained.get(relative, {}).get("entries", []):
                        findings.append(f"{relative}:{line}: target/{name} is not reclaimable; "
                                        f"edit is_reclaimable_target_entry in {POLICY}, "
                                        f"or document retention in {RETAINED} (pins the whole target tree)")
    return findings


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    try:
        findings = check(args.root.resolve())
    except (OSError, ValueError) as error:
        print(f"target-reclaim guard: {error}", file=sys.stderr)
        return 2
    for finding in findings:
        print(finding, file=sys.stderr)
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
