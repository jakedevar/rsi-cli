# Scan one shell fixture for unsafe Git use (#2574, #225). Prints "<line>: <message>" per
# violation. This is the shell half of the Python `fixture_git_guard` scanner, rule for rule:
#   * `cd <dir> && git ...` (any separator after a `cd`) runs Git wherever the cd leaves the shell;
#   * a command-position `git` whose first argument is not `-C ...` runs on an implicit directory;
#   * a one-line function that wraps `git` without `-C`;
#   * in a file that runs Git, `mktemp` with no directory lands under TMPDIR, which a sandbox
#     points inside itself.
# Heredoc bodies and comment-only lines are inert text. A line, or a comment-only line directly
# above it, ending in `# fixture-git: allow <reason>` is exempt (the reason is mandatory).
function trim_left(s) { sub(/^[ \t]+/, "", s); return s }
function allowed_text(s) { return s ~ /#[ \t]*fixture-git:[ \t]*allow[ \t]+[^ \t]/ }
function allowed(n,   above) {
    if (allowed_text(lines[n])) return 1
    if (n >= 2) { above = lines[n - 1]; if (trim_left(above) ~ /^#/ && allowed_text(above)) return 1 }
    return 0
}
# The first problem on one line of code, or "".
function problem_of(code,   p, rest, tail, s, parts, n) {
    # `cd` as a word, then later a separator followed by git (the first `cd` suffices: a later
    # one has a shorter tail).
    if (match(code, /(^|[^A-Za-z0-9_])cd([^A-Za-z0-9_]|$)/)) {
        rest = substr(code, RSTART + RLENGTH - 1)
        if (match(rest, /(&&|;|\|\|)[ \t]*([A-Za-z0-9_]+=[^ \t]+[ \t]+)*git([^A-Za-z0-9_]|$)/))
            return "`cd <dir> && git ...` runs Git wherever the cd leaves the shell (use `git -C <dir>`)"
    }
    tail = code
    while (match(tail, /(^|[;&|(`]|\$\()[ \t]*([A-Za-z0-9_]+=[^ \t]+[ \t]+)*git[ \t]+[^ \t]+/)) {
        s = substr(tail, RSTART, RLENGTH)
        n = split(s, parts, /[ \t]+/)
        if (substr(parts[n], 1, 2) != "-C")
            return "shell Git without `-C` runs on an implicit working directory"
        tail = substr(tail, RSTART + RLENGTH)
    }
    return ""
}
{ lines[NR] = $0 }
END {
    uses_git = 0
    for (i = 1; i <= NR; i++) {
        if (lines[i] ~ /^[ \t]*([A-Za-z0-9_]+=[^ \t]+[ \t]+)*(git|GIT)([^A-Za-z0-9_]|$)/ || lines[i] ~ /(^|[^A-Za-z0-9_])git[ \t]+-C([^A-Za-z0-9_]|$)/) uses_git = 1
    }
    heredoc = ""
    for (i = 1; i <= NR; i++) {
        raw = lines[i]
        if (heredoc != "") {
            t = raw; gsub(/^[ \t]+|[ \t]+$/, "", t)
            if (t == heredoc) heredoc = ""
            continue
        }
        t = raw; gsub(/^[ \t]+|[ \t]+$/, "", t)
        if (t == "" || substr(t, 1, 1) == "#") continue
        code = raw
        if (match(code, /<<-?[ \t]*['"]?[A-Za-z0-9_]+['"]?/)) {
            marker = substr(code, RSTART, RLENGTH)
            code = substr(code, 1, RSTART - 1)
            sub(/^<<-?[ \t]*['"]?/, "", marker); sub(/['"]?$/, "", marker)
            heredoc = marker
        }
        problem = problem_of(code)
        if (match(code, /^[ \t]*(function[ \t]+)?[A-Za-z0-9_]+[ \t]*\(\)[ \t]*\{[ \t]*git[ \t]+[^ \t]+/)) {
            s = substr(code, RSTART, RLENGTH); n = split(s, parts, /[ \t]+/)
            if (substr(parts[n], 1, 2) != "-C") problem = "shell function wraps Git without `-C`"
        }
        if (problem == "" && uses_git && match(code, /(^|[^A-Za-z0-9_])mktemp([^A-Za-z0-9_]|$)/)) {
            after = substr(code, RSTART + RLENGTH)
            if (index(after, "/") == 0) problem = "mktemp without a directory lands under TMPDIR, which a sandbox points inside itself"
        }
        if (problem != "" && !allowed(i)) printf "%d: %s\n", i, problem
    }
}
