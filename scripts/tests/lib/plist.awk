# Validate an XML property list and flatten it to `path<TAB>type<TAB>value` lines (#472).
#
# The legacy Python suites used plistlib, which rejects malformed XML and gives every value its
# type. A text grep does neither: `<string>21600</string>` for StartInterval, a mismatched
# closing tag, or a `<string>` value outside the `<key>` it was meant for still contained the
# expected text. This reader is the same contract without an interpreter:
#   * the document is well formed (every tag closed by its own name, one `<plist>` root holding
#     exactly one value, text only inside leaf elements);
#   * a `<dict>` alternates `<key>` and a value, no key without a value, no repeated key;
#   * the output is UNFORGEABLE (#479): a record is `path<TAB>type<TAB>value<LF>`, so text may
#     never carry a tab or line break (a string value or key that does is refused, `<data>` has
#     its base64 whitespace removed) and a key may not contain `.` (it would collide with a
#     nested path): no document can make one value look like another key;
#   * only the plist form `<plist version="1.0">` (or a bare `<plist>`) and attribute-less elements
#     are accepted, and CDATA, numeric/unknown entities, a DOCTYPE with an internal subset and
#     elements inside a leaf are REFUSED, not misread: over-refusal is the safe direction;
#   * `<integer>` text is an integer, `<real>` text is a number, `<true/>`/`<false/>` are booleans;
#   * paths are dotted: `Label`, `EnvironmentVariables.HOME`, `ProgramArguments.0`.
# Leaf types are string, integer, real, true, false, data, date; containers print as dict/array
# with an empty value. Exit 1 with a `plist: ...` message on stderr for any malformed input.
function fail(msg) { print "plist: " msg > "/dev/stderr"; failed = 1; exit 1 }
function trim(s) { gsub(/^[ \t\r\n]+|[ \t\r\n]+$/, "", s); return s }
function unesc(s,   t) {
    t = s; gsub(/&(lt|gt|quot|apos|amp);/, "", t)
    if (t ~ /&/) fail("unsupported entity (only &lt; &gt; &quot; &apos; &amp;) in [" s "]")
    gsub(/&lt;/, "<", s); gsub(/&gt;/, ">", s); gsub(/&quot;/, "\"", s)
    gsub(/&apos;/, "'", s); gsub(/&amp;/, "\\&", s)
    return s
}
# The path the next value in container c takes; consumes the pending key or array slot.
function next_path(c,   p) {
    if (ctype[c] == "plist") {
        if (roots > 0) fail("more than one value under <plist>")
        roots++
        return ""
    }
    if (ctype[c] == "dict") {
        if (ckey[c] == "") fail("a dict value without a <key>")
        p = (cpath[c] == "") ? ckey[c] : cpath[c] "." ckey[c]
        ckey[c] = ""
        return p
    }
    p = (cpath[c] == "") ? cidx[c] : cpath[c] "." cidx[c]
    cidx[c]++
    return p
}
function emit(path, type, value) {
    if ((path type value) ~ /[\t\r\n]/) fail("a tab or line break in a record [" path "] cannot be represented")
    print path "\t" type "\t" value
}
function push_container(name,   p) {
    p = (cdepth == 0) ? "" : next_path(cdepth)
    cdepth++
    ctype[cdepth] = name; cpath[cdepth] = p; ckey[cdepth] = ""; cidx[cdepth] = 0
    if (name != "plist") emit(p, name, "")
}
function leaf_ok(type, value) {
    if (type == "integer" && value !~ /^-?[0-9]+$/) fail("<integer> holds [" value "]")
    if (type == "real" && value !~ /^-?[0-9]+(\.[0-9]*)?([eE][-+]?[0-9]+)?$/) fail("<real> holds [" value "]")
}
function is_container(name) { return name == "dict" || name == "array" || name == "plist" }
function open_tag(name, selfclose) {
    if (depth > 0 && !is_container(stack[depth])) fail("<" name "> inside <" stack[depth] ">")
    if (name == "dict" || name == "array" || name == "plist") {
        if (name == "plist" && (depth > 0 || seen_root)) fail("<plist> must be the single root")
        if (name == "plist") seen_root = 1
        if (name != "plist" && cdepth == 0) fail("<" name "> outside <plist>")
        push_container(name)
        if (selfclose) close_container(name)
        else { depth++; stack[depth] = name }
        return
    }
    if (cdepth == 0) fail("<" name "> outside <plist>")
    if (name == "true" || name == "false") {
        if (!selfclose) fail("<" name "> must be self-closing")
        if (ctype[cdepth] == "dict" && ckey[cdepth] == "") fail("a dict value without a <key>")
        emit(next_path(cdepth), name, name)
        return
    }
    if (name == "key" || name == "string" || name == "integer" || name == "real" || name == "data" || name == "date") {
        if (name == "key" && ctype[cdepth] != "dict") fail("<key> outside a <dict>")
        if (name != "key" && ctype[cdepth] == "dict" && ckey[cdepth] == "") fail("a dict value without a <key>")
        if (selfclose) {
            if (name == "string") { emit(next_path(cdepth), "string", ""); return }
            fail("<" name "/> cannot be empty")
        }
        depth++; stack[depth] = name; text = ""
        return
    }
    fail("unknown element <" name ">")
}
function close_container(name) {
    if (name == "dict" && ckey[cdepth] != "") fail("<key>" ckey[cdepth] "</key> has no value")
    cdepth--
}
function close_tag(name) {
    if (depth == 0 || stack[depth] != name) fail("</" name "> does not close <" stack[depth] ">")
    depth--
    if (name == "dict" || name == "array" || name == "plist") {
        close_container(name)
        return
    }
    text = unesc(text)
    if (name == "data") gsub(/[ \t\r\n]+/, "", text)
    else if (text ~ /[\t\r\n]/) fail("a tab or line break inside <" name "> text is unsupported")
    if (name == "key") {
        if (text ~ /[.]/) fail("<key>" text "</key> contains a dot, which collides with a nested path")
        k = (cpath[cdepth] == "") ? text : cpath[cdepth] "." text
        if (ckey[cdepth] != "") fail("two <key> elements in a row")
        if (k in keys) fail("repeated key " text)
        keys[k] = 1
        ckey[cdepth] = text
        if (text == "") fail("an empty <key>")
        return
    }
    leaf_ok(name, text)
    emit(next_path(cdepth), name, text)
}
{ doc = doc $0 "\n" }
END {
    if (failed) exit 1
    n = length(doc); i = 1; depth = 0; cdepth = 0; roots = 0; seen_root = 0; text = ""
    while (i <= n) {
        rest = substr(doc, i)
        lt = index(rest, "<")
        if (lt == 0) { chunk = rest; i = n + 1 } else { chunk = substr(rest, 1, lt - 1); i += lt - 1 }
        # Text inside a leaf keeps its whitespace (plistlib does); anywhere else only blanks are legal.
        if (depth > 0 && !is_container(stack[depth])) text = text chunk
        else if (chunk !~ /^[ \t\r\n]*$/) fail("stray text [" trim(chunk) "]")
        if (i > n) break
        rest = substr(doc, i)
        if (substr(rest, 1, 4) == "<!--") {
            e = index(rest, "-->")
            if (e == 0) fail("unterminated comment")
            i += e + 2
            continue
        }
        gt = index(rest, ">")
        if (gt == 0) fail("unterminated tag")
        tag = substr(rest, 2, gt - 2); i += gt
        if (tag ~ /^\?/) {
            if (seen_root || depth > 0 || tag !~ /^\?xml[ \t].*\?$/) fail("unsupported processing instruction <" tag ">")
            continue
        }
        if (tag ~ /^!/) {
            if (seen_root || depth > 0 || tag !~ /^!DOCTYPE plist[ \t]/ || index(tag, "[") > 0) fail("unsupported declaration <" tag "> (no CDATA, entities or internal subsets)")
            continue
        }
        if (tag ~ /^\//) { close_tag(trim(substr(tag, 2))); continue }
        selfclose = 0
        if (tag ~ /\/$/) { selfclose = 1; tag = substr(tag, 1, length(tag) - 1) }
        tag = trim(tag)
        name = tag; sub(/[ \t\r\n].*$/, "", name)
        # No attributes anywhere except the exact `<plist version="1.0">`: an unquoted or odd
        # attribute is malformed XML and must not be dropped silently (#479).
        if (name !~ /^[a-z]+$/) fail("not a valid element name: <" tag ">")
        if (name == "plist") { if (tag != "plist" && tag != "plist version=\"1.0\"") fail("<plist> attributes must be exactly version=\"1.0\": <" tag ">") }
        else if (tag != name) fail("<" name "> takes no attributes: <" tag ">")
        open_tag(name, selfclose)
    }
    if (failed) exit 1
    if (depth != 0) fail("unclosed <" stack[depth] ">")
    if (!seen_root || roots != 1) fail("no <plist> holding exactly one value")
}
