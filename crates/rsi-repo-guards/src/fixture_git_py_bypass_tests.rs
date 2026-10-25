//! Executable Git the retired `ast`-based fixture guard rejected and the first tokenizer missed
//! (#478, review of #466). Each input is valid Python whose runtime behaviour is a bare Git call,
//! so a scanner that does not decode escapes, record every chained-assignment target or read
//! f-string replacement fields as code lets it through. They live in their own file so a red-before
//! run can swap only `fixture_git_py.rs` for its earlier text.
use crate::fixture_git_py::python_violations;

fn count(source: &str) -> usize {
    python_violations(source).len()
}

#[test]
fn a_hex_escaped_git_word_is_still_git() {
    assert_eq!(
        count(
            r"import subprocess
subprocess.run(['\x67it', 'commit'], cwd=root)
"
        ),
        1
    );
}

#[test]
fn octal_and_unicode_escaped_git_words_are_still_git() {
    for word in [
        r"\147it",
        r"\u0067it",
        r"\U00000067it",
        r"g\x69t",
        r"\x67\x69\x74",
    ] {
        let source = format!("import subprocess\nsubprocess.run(['{word}', 'commit'])\n");
        assert_eq!(count(&source), 1, "{word}");
    }
}

#[test]
fn a_raw_string_is_not_decoded_so_a_backslash_word_is_not_git() {
    let source = "import subprocess\nsubprocess.run([r'\\x67it', 'commit'])\n";
    assert_eq!(count(source), 0);
}

#[test]
fn a_named_unicode_escape_is_unsupported_source_not_a_pass() {
    for text in [
        "import subprocess\nsubprocess.run(['\\N{LATIN SMALL LETTER G}it', 'commit'])\n",
        "x = '\\N{DIGIT ONE}'\n",
        "x = '\\xZZ'\n",
        "x = f'\\N{DIGIT ONE}{y}'\n",
    ] {
        let found = python_violations(text);
        assert_eq!(found.len(), 1, "{text}: {found:?}");
        assert!(found[0].1.contains("cannot decode"), "{found:?}");
    }
    // A plain backslash escape the scanner understands is fine.
    assert_eq!(count("x = 'a\\tb\\\\c\\x41\\101\\u0041'\n"), 0);
}

#[test]
fn every_name_target_of_a_chained_assignment_is_recorded() {
    let source = "import subprocess\ncommand = alias = ['git', 'commit']\nsubprocess.run(command, cwd=root)\n";
    assert_eq!(count(source), 1);
    let through_second =
        "import subprocess\nfirst = second = third = ['git', 'commit']\nsubprocess.run(third)\n";
    assert_eq!(count(through_second), 1);
    let through_first =
        "import subprocess\nfirst = second = ['git', 'commit']\nsubprocess.run(first)\n";
    assert_eq!(count(through_first), 1);
    // The last assignment still wins for every target.
    let rebound =
        "import subprocess\na = b = ['git', 'commit']\na = b = ['echo', 'x']\nsubprocess.run(b)\n";
    assert_eq!(count(rebound), 0);
    // A chained assignment of a safe vector stays safe.
    let safe = "import subprocess\na = b = ['git', '-C', str(r), 'status']\nsubprocess.run(b)\n";
    assert_eq!(count(safe), 0);
}

#[test]
fn an_f_string_replacement_field_is_code() {
    let source =
        "import subprocess\nx = f\"{subprocess.run(['git', 'reset', '--hard'], cwd=root)}\"\n";
    let found = python_violations(source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 2, "the violation names the f-string's line");
}

#[test]
fn nested_and_formatted_replacement_fields_are_scanned() {
    for source in [
        // A format spec and a conversion around the call.
        "import subprocess\nx = f\"{subprocess.run(['git', 'commit'])!r:>10}\"\n",
        // An f-string inside a replacement field inside an f-string.
        "import subprocess\nx = f\"\"\"a{f'{subprocess.run([\"git\", \"gc\"])}'}b\"\"\"\n",
        // A lambda and a subscript wrapping the call.
        "import subprocess\nx = f\"{(lambda: subprocess.run(['git', 'commit']))()}\"\n",
        "import subprocess\nx = f\"{[subprocess.run(['git', 'commit'])][0]}\"\n",
        // Resolved through an assignment made outside the field.
        "import subprocess\ncmd = ['git', 'commit']\nx = f\"{subprocess.run(cmd)}\"\n",
        // A shell string.
        "import os\nx = f\"{os.system('git commit -qm x')}\"\n",
    ] {
        assert_eq!(count(source), 1, "{source}");
    }
}

#[test]
fn harmless_f_strings_stay_clean() {
    for source in [
        "x = f\"{root}/{name!r}:{width}\"\n",
        "import subprocess\nsubprocess.run(['echo', f\"{a}\"])\n",
        "x = f\"{{git commit}} {a['k']} {b.c} {len(d)}\"\n",
        "import subprocess\nx = f\"{subprocess.run(['git', '-C', str(r), 'status'])}\"\n",
    ] {
        assert_eq!(count(source), 0, "{source}");
    }
}
