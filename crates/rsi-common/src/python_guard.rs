//! #654 (original guard #3697): refuse Python in agent shell commands.
//!
//! The operator directive is that Python is forbidden: not an
//! edit script, not a `python3 -c` one-liner, not even `python3 --version`.
//! Recall alone did not hold (a worker ran `python3 --version` inside a longer
//! command), so the Claude/Codex `PreToolUse` hook enforces it mechanically,
//! in the style of the `pkill` ([`crate::kill_guard`]) and `git stash`
//! refusals.
//!
//! Repository scripts and modules have no agent-shell exemption. The daemon's
//! transitional gate machinery runs only through `AgentSubmitJob` and automatic
//! git hooks. Those launch processes directly, outside the provider's tool
//! hook; this guard never scans their nested commands.
//!
//! Like the kill guard it is a guard rail for cooperative agents, not a
//! security boundary: it reads command text with the shared shell lexer and
//! does not follow scripts, aliases or variables.

use crate::kill_guard::{command_word, simple_commands};

/// What a refused command is told.
pub const PYTHON_REFUSAL: &str = "rsi refused this command (#654): AGENTS.md operator rule: \
Python is forbidden; use Rust or Go. Agent shell invocations of `python`, `python3`, \
versioned Python, `pypy`, `pip`, `pip3`, `pipx`, `uv` and `uvx` are refused with any arguments, \
including repository scripts, modules and version probes. The daemon's transitional Python \
gate machinery runs only via AgentSubmitJob and automatic git hooks, which bypass the agent \
shell hook. Nothing was run.";

/// Shell programs whose `-c` script is checked too.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish"];
/// Python package and environment managers: refused with any arguments.
const PACKAGE_TOOLS: &[&str] = &["pip", "pip3", "pipx", "uv", "uvx"];

/// `Some(reason)` when `command` runs a Python interpreter or a Python package
/// manager, regardless of its arguments; `None` otherwise.
#[must_use]
pub fn python_refusal(command: &str) -> Option<&'static str> {
    uses_python(command, 0).then_some(PYTHON_REFUSAL)
}

fn uses_python(script: &str, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    for words in simple_commands(script) {
        let Some((program, args)) = command_word(&words) else {
            continue;
        };
        if is_package_tool(program) || is_interpreter(program) {
            return true;
        }
        if SHELLS.contains(&program)
            && let Some(position) = args.iter().position(|arg| arg == "-c")
            && let Some(inner) = args.get(position + 1)
            && uses_python(inner, depth + 1)
        {
            return true;
        }
    }
    false
}

fn is_package_tool(program: &str) -> bool {
    if PACKAGE_TOOLS.contains(&program) {
        return true;
    }
    // `pip3.12`
    program.strip_prefix("pip").is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

/// `python`, `python3`, `python3.12`, `pythonw`, `pypy`, `pypy3`.
fn is_interpreter(program: &str) -> bool {
    if program == "pythonw" {
        return true;
    }
    ["python", "pypy"].iter().any(|stem| {
        program
            .strip_prefix(stem)
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(command: &str) -> bool {
        python_refusal(command).is_some()
    }

    #[test]
    fn every_interpreter_use_is_refused_even_a_version_probe() {
        for command in [
            "python",
            "python3 -",
            "python3 --version",
            "python --version",
            "python3 -V",
            "python3.12 --version",
            "/usr/bin/python3 --version",
            "python3",
            "python3 -c 'print(1)'",
            "python3 -I -c 'print(1)'",
            "python3 -Ic 'print(1)'",
            "python3 - <<'PY'\nprint(1)\nPY",
            "python3 <<'PY'\nprint(1)\nPY",
            "python3 /tmp/edit.py",
            "python3 edit.py",
            "python3 docs/edit.py",
            "python3 scripts/../edit.py",
            "python3 -m json.tool file.json",
            "python3 -m pip install x",
            "python3 -m venv .venv",
            "pypy3 -c 'x'",
            ".venv/bin/python -m pip list",
            "echo hi && python3 --version",
            "ls | python3 -c 'import sys'",
            "env FOO=1 python3 --version",
            "sudo -u worker python3 -c 'x'",
            "timeout 5 python3 --version",
            "bash -c 'python3 --version'",
            "sh -c \"echo ok; python --version\"",
            "echo \"$(python3 --version)\"",
            "x=$(python3 -V); echo $x",
            "cargo build && python3 -W error -c 'x'",
            "python3 --version > /tmp/v",
        ] {
            assert!(refused(command), "{command}");
        }
    }

    #[test]
    fn package_managers_are_refused() {
        for command in [
            "pip install requests",
            "pip3 install requests",
            "pip3.12 list",
            "pipx run black",
            "uv run script.py",
            "uv pip install x",
            "uvx ruff",
            "cd x && uv sync",
            "bash -c 'pip --version'",
        ] {
            assert!(refused(command), "{command}");
        }
    }

    #[test]
    fn repository_python_requires_daemon_jobs_or_automatic_hooks() {
        for command in [
            "python3 -m unittest -v scripts/tests/test_rsi_stash.py",
            "python3 -m unittest discover -s scripts/tests -p test_x.py",
            "python3 -I -m unittest -v tools/tests/test_x.py",
            "python3 -I scripts/rsid-sample-stacks --pid 12 --seconds 5",
            "python3 tools/check-released-migrations.py --refresh abc123",
            "python3 scripts/desktop-test-gate.py --run",
            "python3 ./scripts/x.py",
            "python3 -B -u scripts/x.py",
            "python3 -W error scripts/x.py",
            "env PYTHONDONTWRITEBYTECODE=1 python3 -I scripts/x.py 2>&1 | tee /tmp/x.log",
            "cd /home/me/.rsi/sandboxes/s && python3 -m unittest -v scripts/tests/test_x.py",
            "python3 /home/me/.rsi/sandboxes/s/scripts/x.py",
        ] {
            assert_eq!(python_refusal(command), Some(PYTHON_REFUSAL), "{command}");
        }
    }

    #[test]
    fn text_that_only_mentions_python_is_allowed() {
        for command in [
            "ls ~/python-notes",
            "echo python",
            "git log --grep python",
            "rg python3 docs/",
            "echo 'run python3 --version'",
            "git log --grep=\"pip install\"",
            "cat <<'EOF'\npython3 --version\nuv run x\nEOF",
            "ls python3-notes.md",
            "grep -n pythonic README.md",
            "# python3 --version\necho ok",
            "cargo test -p rsi-common python_guard",
            "which pipeline",
            "pipeline-tool run",
            "pythonista --help",
            "git commit -m 'drop python3 --version'",
        ] {
            assert!(!refused(command), "{command}");
        }
    }

    #[test]
    fn the_refusal_names_the_issue_and_the_allowed_alternatives() {
        let reason = python_refusal("python3 --version").unwrap();
        for required in [
            "#654",
            "AGENTS.md",
            "Python is forbidden",
            "Rust or Go",
            "AgentSubmitJob",
            "automatic git hooks",
            "bypass the agent shell hook",
        ] {
            assert!(reason.contains(required), "{reason}");
        }
    }
}
