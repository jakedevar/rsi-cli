//! Custody guard for the Harness `shell`, `exec_command` and `git` tools
//! (#2256, slice S6 of #2234).
//!
//! A direct-API model works in an RSI sandbox that is a linked worktree of a
//! repository shared with every other agent. AGENTS.md hard rules 1, 2 and 9
//! forbid the git commands that break that sharing: they move the sandbox
//! branch (`checkout`, `switch`, `reset --hard`), rewrite remote history
//! (`push --force`, `--force-with-lease`, `+refspec`), clobber a branch another
//! agent may own (`branch -f`/`-D`/`-M`) or use the stash, which is shared
//! across worktrees (`stash`, every form; `scripts/rsi-stash` is the supported
//! path). Commands that throw away uncommitted work (`checkout -- <path>`,
//! `restore`, `clean -f`) are allowed while there is nothing to lose and
//! refused when `git status` shows changes under the paths they touch.
//!
//! The command text is read with the same lexer as the #1227 kill guard
//! (`rsi_common::kill_guard`): it splits at `;`, `&`, `|`, newlines,
//! parentheses, braces, backticks and `$(` (also inside double quotes, #2288),
//! removes quotes and follows `bash -c '...'`, `eval` and the usual wrappers
//! (`env`, `sudo`, `time`, `xargs`, `/usr/bin/git`, `VAR=1 git`). A refused
//! command is refused whole: nothing in it runs.
//!
//! Aliases are refused, not resolved (#2287): `git -c alias.*=...`,
//! `--config-env` for an alias key (and `-c include.*`, which can load aliases
//! from a file), `GIT_CONFIG_KEY_n=alias.*`, `git config alias.* ...`, and any
//! subcommand that is not a built-in git command while the configuration
//! defines an alias of that name (a `!` alias runs arbitrary shell, a plain one
//! can spell `checkout`). Built-in commands cannot be shadowed by an alias.
//! The alias check reads the configuration the probe sees, so a line that
//! points git at other configuration (`GIT_CONFIG_GLOBAL=`, `GIT_CONFIG_SYSTEM=`,
//! `GIT_CONFIG=`, `GIT_CONFIG_NOSYSTEM=`, `GIT_COMMON_DIR=`, `HOME=`,
//! `XDG_CONFIG_HOME=`, also after `export` or `env`) is refused when its git
//! subcommand is not a built-in (#2319).
//!
//! The scan follows at most `MAX_DEPTH` nested `bash -c`/`eval` levels; a script
//! nested deeper is refused whole, never skipped (#2319).
//!
//! The dirty-path probe runs before the shell starts, so in `shell` and
//! `exec_command` a discard is allowed only when it is the only simple command
//! in the line, with no `;`, `&&`, pipe, subshell or substitution around it
//! (#2290): nothing earlier in the line can then change a path between the
//! probe and the discard. An `env -C DIR`/`--chdir` wrapper relocates the
//! command like `cd` and `git -C` do, so a discard under it is refused (#2289).
//! The direct `git` tool has no shell, but the same short gap remains between
//! its probe's exit and git's start. There is no locking: an edit by another
//! process inside that window is not caught.
//!
//! Where it applies (#3165): the same location predicate as the CLI provider
//! hook (`rsi_common::boundary_mail_hook::command_in_custody_scope`). The scan
//! runs only for a command that starts inside the sandbox base or names a path
//! under it; a scratch repository or a benchmark task's own repository
//! elsewhere is not ours to guard. The predicate is what the CLI providers
//! already use, so one rule decides it for every provider and no per-session
//! switch has to be recorded. A host with no resolvable sandbox base fails
//! closed (the guard applies everywhere). A container target skips the guard
//! at the tool (#3176).
//!
//! It is a guard rail for cooperative agents, not a security boundary. It does
//! not follow scripts, functions, variables, brace expansion, `env -S`, an
//! alias that a script writes into a config file or here-document bodies fed
//! to a shell, and `write_stdin` into an interactive shell is not scanned.

use super::git::scoped_git_command;
use crate::process_control::{CaptureLimits, CapturedOutput, capture_bounded};
use crate::session::harness::types::ToolResult;
use rsi_common::custody_scan::{
    Discard, EXEC_PATH_ARGS, Scan, alias_in_use, alias_names_from, alias_precheck,
    alias_probe_args, alias_unverifiable, branch_change_verdict, conflict_in_progress_from,
    conflict_state_args, dirty, dirty_during_merge, external_identity_args,
    external_repository_verdict, external_worktrees_args, not_shipped, paths_at_risk_from_status,
    resolves_conflict_side, scan_git, scan_script, ships_in_exec_path, status_args, unmerged_args,
    unverifiable, worktree_identity_args,
};
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Stable `error_code` of every custody refusal.
pub(super) const ERROR_CODE: &str = "custody_git_refused";

/// How long the read-only `git status` probe may run before the discard is
/// refused as unverifiable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// A refused command: what the model is told. Nothing was executed.
#[derive(Debug)]
pub(super) struct Refusal(String);

impl Refusal {
    pub(super) fn message(&self) -> &str {
        &self.0
    }
}

/// The result `shell` and `git` return for a refusal; the code leads the
/// message so it stays greppable. `exec_command` uses its JSON error instead.
pub(super) fn refused_result(refusal: Refusal) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some(format!("{ERROR_CODE}: {}", refusal.0)),
    }
}

/// Where and as whom the read-only `git status` probe runs.
pub(super) struct Probe<'a> {
    pub(super) session_id: Option<uuid::Uuid>,
    pub(super) invocation_id: Option<uuid::Uuid>,
    pub(super) working_dir: &'a Path,
    pub(super) cancel: &'a CancellationToken,
    /// The RSI sandbox base ([`rsi_common::boundary_mail_hook::sandbox_base`]);
    /// a command outside it is not guarded (#3165).
    pub(super) sandbox_base: &'a Path,
}

impl Probe<'_> {
    /// Whether `command` is inside RSI's git custody. An unresolvable (empty)
    /// base fails closed: the guard applies.
    fn guards(&self, command: &str) -> bool {
        self.sandbox_base.as_os_str().is_empty()
            || rsi_common::boundary_mail_hook::command_in_custody_scope(
                command,
                self.working_dir,
                self.sandbox_base,
            )
    }
}

/// `Some(refusal)` when a `shell`/`exec_command` script breaks a custody rule.
pub(super) async fn check_shell(command: &str, probe: &Probe<'_>) -> Option<Refusal> {
    if !probe.guards(command) {
        return None;
    }
    let mut scan = Scan::default();
    scan_script(command, 0, true, &mut scan);
    settle(scan, probe).await
}

/// `Some(refusal)` when the argv the `git` tool would pass to git breaks a
/// custody rule. The `git` tool runs git directly, with no shell.
pub(super) async fn check_git_args<S: AsRef<str>>(
    args: &[S],
    probe: &Probe<'_>,
) -> Option<Refusal> {
    let words: Vec<String> = args.iter().map(|word| word.as_ref().to_owned()).collect();
    if !probe.guards(&words.join(" ")) {
        return None;
    }
    let mut scan = Scan::default();
    scan_git(&words, &mut scan);
    settle(scan, probe).await
}

async fn settle(scan: Scan, probe: &Probe<'_>) -> Option<Refusal> {
    if let Some(message) = scan.refusal {
        return Some(Refusal(message));
    }
    if let Some(refusal) = settle_branch_changes(&scan, probe).await {
        return Some(refusal);
    }
    if let Some(refusal) = settle_aliases(&scan, probe).await {
        return Some(refusal);
    }
    let first = scan.discards.first()?;
    if scan.relocated {
        return Some(Refusal(unverifiable(
            first.command(),
            "it changes directory or repository first (cd, env -C, git -C, --git-dir, --work-tree or GIT_DIR)",
        )));
    }
    if scan.crowded {
        return Some(Refusal(unverifiable(
            first.command(),
            "it shares its line with other commands or a grouping, and the check runs before the shell starts, so an earlier command could change the paths first",
        )));
    }
    for discard in &scan.discards {
        match probe.paths_at_risk(discard).await {
            Ok(paths) if paths.is_empty() => {}
            Ok(paths) => {
                if !probe.conflict_in_progress(discard).await {
                    return Some(Refusal(dirty(discard.command(), &paths)));
                }
                if !probe.resolves_conflict_side(discard).await {
                    return Some(Refusal(dirty_during_merge(discard.command(), &paths)));
                }
            }
            Err(error) => {
                return Some(Refusal(unverifiable(
                    discard.command(),
                    &format!("`git status` failed: {error}"),
                )));
            }
        }
    }
    None
}

/// Refuse a `checkout`/`switch` whose `git -C` resolves to this sandbox; one
/// that names another worktree (a detached deploy worktree) passes (#2573).
async fn settle_branch_changes(scan: &Scan, probe: &Probe<'_>) -> Option<Refusal> {
    if scan.pending_branch_changes.is_empty() {
        return None;
    }
    let own = match probe.capture(&worktree_identity_args(&[])).await {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => None,
    };
    let base = rsi_common::boundary_mail_hook::sandbox_base();
    for pending in &scan.pending_branch_changes {
        let target = match probe
            .capture(&worktree_identity_args(&pending.directory))
            .await
        {
            Ok(output) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Ok(output) => Err(failure(&output, output.status.code())),
            Err(error) => Err(error),
        };
        if let Some(message) = branch_change_verdict(
            pending,
            target.as_deref().map_err(String::as_str),
            own.as_deref(),
            &base,
        ) {
            return Some(Refusal(message));
        }
    }
    None
}

/// Refuse a subcommand that the configuration defines as an alias (#2287).
/// Aliases are not followed: a `!` alias runs arbitrary shell and a plain one
/// can spell any git command, so any alias use is refused.
async fn settle_aliases(scan: &Scan, probe: &Probe<'_>) -> Option<Refusal> {
    let first = scan.alias_candidates.first()?;
    if let Some(message) = alias_precheck(scan) {
        return Some(Refusal(message));
    }
    let mut probed: Vec<Option<&Vec<String>>> = Vec::new();
    let mut exec_path: Option<Option<String>> = None;
    for candidate in &scan.alias_candidates {
        if !probed.contains(&candidate.directory.as_ref()) {
            probed.push(candidate.directory.as_ref());
            match probe.alias_names(candidate.directory.as_deref()).await {
                Ok(aliases) => {
                    if let Some(name) = scan
                        .alias_candidates
                        .iter()
                        .filter(|other| other.directory == candidate.directory)
                        .map(|other| &other.name)
                        .find(|name| aliases.contains(&name.to_ascii_lowercase()))
                    {
                        return Some(Refusal(alias_in_use(name)));
                    }
                }
                Err(error) => {
                    return Some(Refusal(alias_unverifiable(
                        &first.name,
                        &format!("`git config` failed: {error}"),
                    )));
                }
            }
        }
        if !candidate.read_only {
            let directory = candidate.directory.as_deref().unwrap_or_default();
            let target = probe
                .stdout_if_success(&external_identity_args(directory))
                .await;
            let own = probe.stdout_if_success(&external_identity_args(&[])).await;
            let worktrees = probe
                .stdout_if_success(&external_worktrees_args(directory))
                .await;
            if let Some(message) = external_repository_verdict(
                candidate,
                target.as_deref(),
                own.as_deref(),
                worktrees.as_deref(),
                probe.working_dir,
                probe.sandbox_base,
            ) {
                return Some(Refusal(message));
            }
        }
        if exec_path.is_none() {
            exec_path = Some(
                probe
                    .stdout_if_success(&EXEC_PATH_ARGS.map(str::to_owned))
                    .await,
            );
        }
        if !exec_path
            .as_ref()
            .and_then(Option::as_deref)
            .is_some_and(|path| ships_in_exec_path(path, &candidate.name))
        {
            return Some(Refusal(not_shipped(&candidate.name)));
        }
    }
    None
}

impl Probe<'_> {
    /// Run a read-only `git` probe in the working directory.
    async fn capture(&self, args: &[String]) -> Result<CapturedOutput, String> {
        let command =
            scoped_git_command(args, self.working_dir, self.session_id, self.invocation_id)?;
        let mut limits = CaptureLimits::session_tool();
        limits.execution_timeout = PROBE_TIMEOUT;
        capture_bounded(command, limits, self.cancel)
            .await
            .map_err(|error| error.to_string())
    }

    /// Lowercase names of every alias the configuration defines here.
    async fn alias_names(&self, directory: Option<&[String]>) -> Result<Vec<String>, String> {
        let args = alias_probe_args(directory);
        let output = self.capture(&args).await?;
        match output.status.code() {
            // Exit 1 is "no such key": no alias is defined.
            Some(0 | 1) => Ok(alias_names_from(&String::from_utf8_lossy(&output.stdout))),
            code => Err(failure(&output, code)),
        }
    }

    /// Stdout of a read-only probe that exited 0; `None` otherwise.
    async fn stdout_if_success(&self, args: &[String]) -> Option<String> {
        match self.capture(args).await {
            Ok(output) if output.status.success() => {
                Some(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            _ => None,
        }
    }

    /// A merge, cherry-pick or revert is in progress in the working directory
    /// and `discard` rewrites tracked files (#156). A failed probe reads false.
    async fn conflict_in_progress(&self, discard: &Discard) -> bool {
        if !matches!(discard, Discard::Tracked { .. }) {
            return false;
        }
        self.stdout_if_success(&conflict_state_args())
            .await
            .is_some_and(|stdout| conflict_in_progress_from(&stdout, self.working_dir))
    }

    /// `discard` only takes `--ours`/`--theirs` of paths that are all unmerged
    /// index entries (#156); the caller checked that a merge is in progress.
    async fn resolves_conflict_side(&self, discard: &Discard) -> bool {
        let Some(args) = unmerged_args(discard) else {
            return false;
        };
        self.stdout_if_success(&args)
            .await
            .is_some_and(|stdout| resolves_conflict_side(discard, true, &stdout))
    }

    /// Paths a discard would destroy, from `git status --porcelain` limited to
    /// the discard's pathspecs. Empty means nothing would be lost.
    async fn paths_at_risk(&self, discard: &Discard) -> Result<Vec<String>, String> {
        let output = self.capture(&status_args(discard)).await?;
        if !output.status.success() {
            return Err(failure(&output, output.status.code()));
        }
        Ok(paths_at_risk_from_status(
            discard,
            &String::from_utf8_lossy(&output.stdout),
        ))
    }
}

/// Why a probe's git process failed: its stderr, or its exit code.
fn failure(output: &CapturedOutput, code: Option<i32>) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("exit code {}", code.unwrap_or(-1))
    } else {
        stderr.to_owned()
    }
}

/// A throwaway repository for tests that run real git, shared with the tool
/// tests in `shell.rs`, `exec.rs` and `git.rs`.
#[cfg(test)]
pub(super) mod test_support {
    use std::path::Path;

    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A repository with committed `a.txt` and `b.txt` on branch `main`.
    pub(crate) fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "custody-test"]);
        git(
            dir.path(),
            &["config", "user.email", "custody@test.invalid"],
        );
        // No background `gc --auto` after a commit (it needs setsid).
        git(dir.path(), &["config", "gc.auto", "0"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        git(dir.path(), &["add", "a.txt", "b.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::custody_scan::MAX_DEPTH;

    /// `allow`, `refuse` or `gated` (allowed only while the touched paths are clean).
    fn classify(command: &str) -> &'static str {
        let mut scan = Scan::default();
        scan_script(command, 0, true, &mut scan);
        if scan.refusal.is_some() {
            "refuse"
        } else if !scan.discards.is_empty() {
            "gated"
        } else {
            "allow"
        }
    }

    fn refusal(command: &str) -> String {
        let mut scan = Scan::default();
        scan_script(command, 0, true, &mut scan);
        scan.refusal.unwrap_or_default()
    }

    fn git_words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn every_git_stash_form_is_refused_however_it_is_spelled() {
        for command in [
            "git stash",
            "git stash pop",
            "git stash push -m wip",
            "git stash -u",
            "git stash list",
            "git stash drop",
            "git stash apply",
            "git stash show -p",
            "git stash clear",
            "git stash save wip",
            "git stash create",
            "git stash store abc",
            "git stash branch topic",
            "git -C . stash",
            "git -c user.name=x stash",
            "git --no-pager stash",
            "git --git-dir=.git stash",
            "/usr/bin/git stash",
            "command git stash",
            "env GIT_X=1 git stash",
            "FOO=1 git stash",
            "sudo git stash",
            "time git stash",
            "nohup git stash",
            "xargs git stash",
            "git-stash",
            "echo a && git stash",
            "git status; git stash pop",
            "git diff | cat; git stash",
            "(git stash)",
            "{ git stash; }",
            "true || git stash",
            "echo x\ngit stash",
            "echo $(git stash)",
            "echo `git stash`",
            "\"git\" stash",
            "git \"stash\"",
            "git 'stash'",
            "git st\\ash",
            "g\\it stash",
            "bash -c 'git stash'",
            "sh -c \"git stash pop\"",
            "bash -lc 'git stash'",
            "zsh -c 'echo hi && git stash'",
            "eval \"git stash\"",
            "eval git stash",
            "bash -c \"sh -c 'git stash'\"",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
            assert!(
                refusal(command).contains("hard rule 9"),
                "{command:?} must name hard rule 9: {}",
                refusal(command)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn rsi_stash_and_stash_as_plain_text_stay_allowed() {
        for command in [
            "scripts/rsi-stash save wip",
            "./scripts/rsi-stash list",
            "bash scripts/rsi-stash restore abc123",
            "git commit -m \"git stash is banned\"",
            "git commit -m 'git checkout main'",
            "git commit -m stash",
            "echo git stash",
            "echo \"git checkout main && git stash\"",
            "grep -rn 'git stash' docs",
            "git log --grep=stash",
            "git log stash@{0}",
            "git diff stash",
            "cargo test stash",
        ] {
            assert_eq!(classify(command), "allow", "{command:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn branch_changing_commands_are_refused_naming_rule_2() {
        for command in [
            "git checkout main",
            "git checkout -b feature",
            "git checkout -B feature",
            "git checkout --detach HEAD",
            "git checkout --orphan fresh",
            "git checkout -t origin/topic",
            "git checkout origin/rolling",
            "git checkout HEAD~1",
            "git checkout -",
            "git checkout file.txt",
            "git checkout main --",
            "git switch main",
            "git switch -c topic",
            "git switch -",
            "git switch --detach",
            "git -C other checkout main",
            "cd sub && git checkout main",
            "bash -c 'git checkout main'",
            "git reset --hard",
            "git reset --hard HEAD~1",
            "git reset --hard origin/rolling",
            "git reset HEAD~1 --hard",
            "git reset --har",
            "sh -c 'git reset --hard'",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
            assert!(
                refusal(command).contains("hard rule 2"),
                "{command:?} must name hard rule 2: {}",
                refusal(command)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn forced_pushes_and_forced_branch_edits_are_refused() {
        for command in [
            "git push --force",
            "git push -f origin topic",
            "git push origin +main",
            "git push origin +HEAD:main",
            "git push --force-with-lease",
            "git push --force-with-lease=main:abc",
            "git push --force-if-includes",
            "git push -fu origin topic",
            "git push origin main -f",
            "git push origin topic --force",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
            assert!(refusal(command).contains("hard rule 1"), "{command:?}");
        }
        for command in [
            "git branch -f topic HEAD",
            "git branch --force topic",
            "git branch -D topic",
            "git branch -M renamed",
            "git branch -fd topic",
            "git branch -C a b",
            "git branch -m -f a b",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
            assert!(
                refusal(command).contains("hard rules 1 and 2"),
                "{command:?}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn ordinary_git_commands_stay_allowed() {
        for command in [
            "git",
            "git status",
            "git status --short",
            "git diff --cached",
            "git log --oneline -10",
            "git show HEAD:file",
            "git add -A",
            "git add file && git commit -m wip",
            "git fetch origin",
            "git merge origin/rolling",
            "git merge --no-ff topic",
            "git reset",
            "git reset HEAD -- file",
            "git reset --soft HEAD",
            "git reset --mixed HEAD",
            "git restore --staged file",
            "git restore -S file",
            "git checkout",
            "git clean",
            "git clean -n",
            "git clean -nfd",
            "git clean -fd --dry-run",
            "git branch",
            "git branch -a",
            "git branch -vv",
            "git branch topic origin/topic",
            "git branch -d topic",
            "git branch -m a b",
            "git branch --list",
            "git branch --show-current",
            "git branch --delete topic",
            "git push origin HEAD",
            "git push -u origin topic",
            "git push --set-upstream origin topic",
            "git push --dry-run",
            "git push origin HEAD:refs/heads/topic",
            "git push -o ci.skip origin topic",
            "git -C sub status",
            "git -c core.pager=cat log -1",
        ] {
            assert_eq!(classify(command), "allow", "{command:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn discards_of_uncommitted_work_are_gated_on_a_clean_path() {
        for command in [
            "git checkout -- file",
            "git checkout -- .",
            "git checkout .",
            "git checkout ./",
            "git checkout :/",
            "git checkout HEAD -- file",
            "git checkout HEAD .",
            "git checkout -f",
            "git checkout -f .",
            "git restore file",
            "git restore .",
            "git restore --worktree file",
            "git restore -SW file",
            "git restore --source=HEAD~1 file",
            "git restore -s HEAD~1 .",
            "git clean -f",
            "git clean -fd",
            "git clean -fdx",
            "git clean -ffd",
            "git clean -fX",
            "git clean --force -d",
            "bash -c 'git restore .'",
            "env FOO=1 git restore .",
            "env -u FOO git clean -fd",
            "time git checkout -- file",
            "git restore 'a;b.txt'",
        ] {
            assert_eq!(classify(command), "gated", "{command:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn the_git_tool_argv_is_classified_without_a_shell() {
        let classify_args = |line: &str| {
            let mut scan = Scan::default();
            scan_git(&git_words(line), &mut scan);
            (scan.refusal.is_some(), scan.discards.len())
        };
        for line in [
            "stash",
            "stash pop",
            "stash push -m wip",
            "checkout main",
            "checkout -b topic",
            "switch main",
            "reset --hard",
            "reset --hard HEAD~1",
            "push --force",
            "push origin +main",
            "branch -D topic",
            "-C . stash",
            "-c user.name=x stash",
        ] {
            assert_eq!(classify_args(line), (true, 0), "{line:?}");
        }
        for line in [
            "status",
            "diff --cached",
            "log --oneline -10",
            "add -A",
            "reset --soft HEAD",
        ] {
            assert_eq!(classify_args(line), (false, 0), "{line:?}");
        }
        for line in ["checkout -- file", "restore .", "clean -fd"] {
            assert_eq!(classify_args(line), (false, 1), "{line:?}");
        }
    }

    fn alias_candidates(line: &str) -> Vec<String> {
        let mut scan = Scan::default();
        scan_git(&git_words(line), &mut scan);
        scan.alias_candidates
            .into_iter()
            .map(|candidate| candidate.name)
            .collect()
    }

    /// #2287: aliases are refused, never resolved.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn git_aliases_are_refused_however_they_are_supplied() {
        for command in [
            "git -c 'alias.x=!git stash' x",
            "git -c alias.x=stash x",
            "git -c ALIAS.X=!true x",
            "git -c Alias.co=checkout co main",
            "git -c user.name=a -c alias.x=!true x",
            "git --config-env=alias.x=ALIAS_ENV x",
            "git --config-env alias.x=ALIAS_ENV x",
            "git -c include.path=/tmp/evil status",
            "git -c includeIf.gitdir:/.path=/tmp/evil status",
            "env -i git -c alias.x=!true x",
            "bash -c \"git -c 'alias.x=!git stash' x\"",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.x GIT_CONFIG_VALUE_0='!git stash' git x",
            "env GIT_CONFIG_KEY_0=ALIAS.x git x",
            "export GIT_CONFIG_KEY_3=include.path",
            "GIT_CONFIG_PARAMETERS=\"'alias.x=!git stash'\" git x",
            "git config alias.x '!git stash' && git x",
            "git config --global alias.x '!git stash'",
            "git config --add alias.x stash",
            "git config set alias.x stash",
            "git config --file /tmp/f alias.x '!true'",
            "git config ALIAS.X '!git stash'",
            "git -C . config alias.x stash",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
            assert!(
                refusal(command).contains("hard rules 1, 2 and 9"),
                "{command:?}: {}",
                refusal(command)
            );
        }
        // The git tool passes the argv straight to git, so `-c` is read there too.
        for line in [
            "-c alias.x=!true x",
            "--config-env=alias.x=E x",
            "config alias.x stash",
        ] {
            let mut scan = Scan::default();
            scan_git(&git_words(line), &mut scan);
            assert!(scan.refusal.is_some(), "{line:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn config_reads_and_non_alias_settings_stay_allowed() {
        for command in [
            "git config user.name",
            "git config core.editor vim",
            "git config --global user.email a@b.invalid",
            "git config --get alias.co",
            "git config --get-regexp '^alias\\.'",
            "git config --list",
            "git config get alias.co",
            "git config --unset alias.x",
            "git -c core.pager=cat -c color.ui=never diff",
            "git -c commit.gpgsign=false commit -q -m 'alias.x is only text'",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0=cat git log",
        ] {
            assert_eq!(classify(command), "allow", "{command:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn only_non_builtin_subcommands_are_alias_candidates() {
        for line in [
            "status",
            "stash",
            "checkout main",
            "-c core.pager=cat log",
            "-C sub diff",
            "submodule update",
            "worktree list",
            "",
        ] {
            assert_eq!(alias_candidates(line), Vec::<String>::new(), "{line:?}");
        }
        assert_eq!(alias_candidates("x"), ["x"]);
        assert_eq!(alias_candidates("-c core.pager=cat lfs ls-files"), ["lfs"]);
        assert_eq!(alias_candidates("--no-pager Co main"), ["Co"]);
    }

    /// #2288: command substitution inside double quotes is one word to a naive
    /// lexer, so the scan of the shared lexer must descend into it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn substitutions_inside_double_quotes_are_scanned() {
        for command in [
            "printf '%s' \"$(git stash)\"",
            "echo \"$(git stash pop)\"",
            "echo \"`git stash`\"",
            "echo \"a $(echo \"b $(git stash)\") c\"",
            "echo \"$(echo '\"')\"; git stash",
            "echo \"$(git checkout main)\"",
            "echo \"$(git reset --hard)\"",
            "echo \"$(git push --force)\"",
            "echo \"$(git -c 'alias.x=!true' x)\"",
            "FOO=\"$(git stash)\" true",
            "bash -c 'echo \"$(git stash)\"'",
            "git commit -m \"wip `git stash` done\"",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
        }
        for command in [
            "echo \"git stash\"",
            "echo \"\\$(git stash)\"",
            "echo '$(git stash)'",
            "echo \"$(date +%F)\" \"`hostname`\"",
            "git commit -m \"fix $(date +%F)\"",
        ] {
            assert_eq!(classify(command), "allow", "{command:?}");
        }
    }

    fn crowded(command: &str) -> bool {
        let mut scan = Scan::default();
        scan_script(command, 0, true, &mut scan);
        scan.crowded
    }

    /// #2290: the dirty-path probe runs before the shell starts, so only a
    /// discard that is the whole line can rely on it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn a_discard_is_crowded_unless_it_is_the_only_command_in_the_line() {
        for command in [
            "sleep 10; git checkout -- a.txt",
            "sleep 10 && git restore .",
            "true || git clean -fd",
            "git restore . ; echo done",
            "git status && git checkout -- a.txt",
            "echo x | git restore .",
            "git restore .\ngit status",
            "(git restore .)",
            "{ git restore .; }",
            "git restore . &",
            "git restore . 2>&1",
            "git restore $(sleep 10; echo a.txt)",
            "git restore \"$(sleep 10; echo a.txt)\"",
            "git restore `sleep 10; echo a.txt`",
            "git restore \"`sleep 10; echo a.txt`\"",
            "bash -c 'sleep 10; git checkout -- a.txt'",
            "bash -c 'git checkout -- a.txt; true'",
            "sleep 10; bash -c 'git restore .'",
            "eval 'sleep 1; git restore .'",
            "echo $(git restore .)",
        ] {
            assert_eq!(classify(command), "gated", "{command:?}");
            assert!(crowded(command), "{command:?}");
        }
        for command in [
            "git checkout -- a.txt",
            "git restore .",
            "git clean -fd",
            "  git restore a.txt\n",
            "git restore a.txt # why: undo the edit; then (retry)",
            "env FOO=1 git restore .",
            "time git restore .",
            "bash -c 'git restore .'",
            "sh -c \"git clean -fd\"",
            "eval git restore .",
            "git restore 'a;b.txt'",
            "git restore \"a b.txt\"",
            "git restore a.txt\\\n b.txt",
        ] {
            assert_eq!(classify(command), "gated", "{command:?}");
            assert!(!crowded(command), "{command:?}");
        }
        // Commands that discard nothing never crowd a line.
        assert!(!crowded("sleep 10; git status && git add -A"));
    }

    /// #2289: `env -C DIR` starts the command in another directory.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn env_chdir_relocates_like_cd() {
        let relocated = |command: &str| {
            let mut scan = Scan::default();
            scan_script(command, 0, true, &mut scan);
            scan.relocated
        };
        for command in [
            "env -C sub git checkout -- a.txt",
            "env -C sub git restore .",
            "env --chdir=sub git restore .",
            "env --chdir sub git clean -fd",
            "env -Csub git restore .",
            "env -iC sub git restore .",
            "env FOO=1 -C sub git restore .",
            "FOO=1 env -C sub git restore .",
            "sudo env -C sub git restore .",
            "bash -c 'env -C sub git restore .'",
            "cd sub",
            "GIT_DIR=.git git status",
        ] {
            assert!(relocated(command), "{command:?}");
        }
        for command in [
            "env git restore .",
            "env FOO=1 git restore .",
            "env -u FOO git restore .",
            "env -i git restore .",
            "git commit -C HEAD",
            "git restore -s HEAD .",
            "nice -n 5 git restore .",
            "echo -C sub",
        ] {
            assert!(!relocated(command), "{command:?}");
        }
    }

    /// `script` wrapped in `levels` nested `bash -c '...'` layers.
    fn nest_bash(script: &str, levels: usize) -> String {
        let mut nested = script.to_owned();
        for _ in 0..levels {
            nested = format!("bash -c '{}'", nested.replace('\'', "'\\''"));
        }
        nested
    }

    /// #2319: a script nested past `MAX_DEPTH` was skipped, so five literal
    /// `bash -c` layers around `git checkout main` reached the branch change.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn scripts_nested_past_the_depth_limit_are_refused_not_skipped() {
        // Within the limit the inner command is read and judged on its merits.
        let within = nest_bash("git checkout main", MAX_DEPTH);
        assert_eq!(classify(&within), "refuse", "{within}");
        assert!(refusal(&within).contains("hard rule 2"), "{within}");
        let within = nest_bash("git status", MAX_DEPTH);
        assert_eq!(classify(&within), "allow", "{within}");

        // One layer deeper is refused whole, whatever it holds.
        for inner in ["git checkout main", "git stash", "git status", "echo hi"] {
            let too_deep = nest_bash(inner, MAX_DEPTH + 1);
            assert_eq!(classify(&too_deep), "refuse", "{too_deep}");
            let message = refusal(&too_deep);
            assert!(
                message.contains("more than 4 levels deep"),
                "{too_deep}: {message}"
            );
            assert!(message.contains("hard rules 1, 2 and 9"), "{message}");
        }
    }

    fn config_source_overridden(command: &str) -> bool {
        let mut scan = Scan::default();
        scan_script(command, 0, true, &mut scan);
        scan.config_source_overridden
    }

    /// #2319: an assignment on the command line reaches only the command it
    /// prefixes, so the probe cannot see the configuration it selects.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn config_source_assignments_are_flagged_in_every_spelling() {
        for command in [
            "GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git x",
            "GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig git x",
            "GIT_CONFIG=/tmp/evil.gitconfig git x",
            "GIT_CONFIG_NOSYSTEM=0 git x",
            "GIT_COMMON_DIR=/tmp/evil git x",
            "HOME=/tmp/evil git x",
            "XDG_CONFIG_HOME=/tmp/evil git x",
            "HOME+=/evil git x",
            "env GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git x",
            "env -i HOME=/tmp/evil git x",
            "export GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig; git x",
            "export XDG_CONFIG_HOME=/tmp/evil && git x",
            "declare -x HOME=/tmp/evil; git x",
            "bash -c 'GIT_CONFIG_GLOBAL=/tmp/evil.gitconfig git x'",
            "sudo env GIT_CONFIG_SYSTEM=/tmp/evil.gitconfig git x",
        ] {
            assert!(config_source_overridden(command), "{command:?}");
        }
        for command in [
            "git x",
            "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0=cat git x",
            "GIT_PAGER=cat git x",
            "env FOO=1 git x",
            "echo HOME",
            "cd $HOME && git x",
        ] {
            assert!(!config_source_overridden(command), "{command:?}");
        }
    }

    /// #2571: a failed `cd` into a scratch repository lets the shell carry on,
    /// and the next unpinned mutating git runs in the sandbox (the #2291 worker
    /// made two junk commits and a `reset --mixed` that way). The same scan
    /// guards the CLI hook (`boundary_mail_hook`) and the Harness `shell` tool.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn a_scratch_cd_before_an_unpinned_mutating_git_is_refused() {
        for command in [
            "cd /tmp/fixture; git commit -m x",
            "cd /tmp/fixture && git init -q && git commit -m x",
            "mkdir -p /tmp/f; cd /tmp/f; git reset --mixed HEAD~1",
            "cd \"$(mktemp -d)\"; git commit --allow-empty -m x",
            "cd $(mktemp -d); git commit --allow-empty -m x",
            "d=$(mktemp -d); cd \"$d\"; git add -A",
            "scratch=/var/tmp/x; cd $scratch; git update-ref refs/heads/x HEAD",
            "cd $TMPDIR/fixture && git branch stray HEAD",
            "cd \"$TMPDIR/fixture\" && git tag v1",
            "pushd /dev/shm/f; git config user.name x",
            "bash -c 'cd /tmp/fixture; git commit -m x'",
            "(cd /tmp/fixture; git tag v1)",
            "cd ./target/.rsi-tmp/x; git remote add origin u",
            "cd /tmp/f; git -C . commit -m x",
            "cd /tmp/f; git -C sub commit -m x",
            "cd /tmp/f; git -C '' commit -m x",
            "cd /tmp/f; git -C \"$PWD\" commit -m x",
            // An unknown, possibly empty pin is no pin (#2571).
            "cd /tmp/f; git -C \"$scratch\" commit -m x",
            "cd /tmp/f; git -C $SCRATCH_REPO add -A",
            "cd /tmp/f; git -C \"$(mktemp -d)\" commit -m x",
            "cd /tmp/f; git -C ~/fixture commit -m x",
            "cd /tmp/f; GIT_DIR=\"$d\" GIT_WORK_TREE=\"$d\" git commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git git commit -m x",
            "cd /tmp/f; git --git-dir=/tmp/f/.git commit -m x",
            // A pin belongs to its own invocation, not to an earlier line.
            "export GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f; cd /tmp/f; git commit -m x",
            "GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git status; cd /tmp/f; git commit -m x",
            "cd /tmp/f; git -C /tmp/f status; git commit -m x",
            // A later cd may fail as well, so it does not clear the risk.
            "cd /tmp/f; cd /home/agent/sandboxes/session; git commit -m x",
            "cd /tmp/f || true; git commit -m x",
            "cd /tmp/f; git init -q; git add . ; git commit -m x",
            // #2580: env removes the pins, so the sandbox is the target.
            "cd /tmp/missing; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -u GIT_DIR -u GIT_WORK_TREE git commit --allow-empty -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f env -i git commit -m x",
            // #2581: a value spelled `list` or `get` is still a write.
            "cd /tmp/missing; git config rsi.fixture list",
            "cd /tmp/missing; git config set rsi.fixture list",
            "cd /tmp/missing; git config rsi.fixture get",
        ] {
            assert_eq!(classify(command), "refuse", "{command:?}");
            let message = refusal(command);
            assert!(
                message.contains("scratch or temporary"),
                "{command:?}: {message}"
            );
            assert!(
                message.contains("git -C <absolute scratch dir>"),
                "{message}"
            );
            assert!(message.contains("hard rule 2"), "{message}");
            assert!(message.contains("Nothing was run"), "{message}");
        }
    }

    /// #2571: only the dangerous chain is refused. Pinned repositories, reads,
    /// ordinary assigned-branch work and words inside quoted messages stay
    /// allowed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn the_scratch_cd_refusal_leaves_pinned_reads_and_ordinary_work_alone() {
        for command in [
            "git -C /tmp/fixture commit -m x",
            "git -C \"$scratch\" commit -m x",
            "GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "git --git-dir=/tmp/f/.git --work-tree=/tmp/f commit -m x",
            "cd /tmp/f; git -C /tmp/f commit -m x",
            "cd /tmp/f; git --git-dir=/tmp/f/.git --work-tree=/tmp/f commit -m x",
            "cd /tmp/f; GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; git -C /tmp/f/sub add -A",
            "cd /tmp/f; env GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            "cd /tmp/f; git --git-dir /tmp/f/.git --work-tree /tmp/f commit -m x",
            "cd /tmp/f && git status && git log --oneline -3 && git diff",
            "cd /tmp/f && git branch --show-current && git config --get user.name",
            "cd /tmp/f && git init -q",
            "cd crates/rsi-common && git commit -m x",
            "cd scripts && git add -A && git commit -m 'cd /tmp/scratch; mktemp'",
            "git commit -m 'cd /tmp/scratch; git reset --mixed HEAD~2'",
            "git commit -m \"fix the scratch tmp dir cd\"",
            "echo cd /tmp/x; git commit -m x",
            "mktemp -d; git commit -m x",
            "git add a.txt && git commit -m x",
            "git fetch origin && git merge origin/rolling && git push origin HEAD:rolling",
            "cd /tmp/f && ls",
            // #2582: a child's cd never reaches the shell that started it.
            "bash -c 'cd /tmp; ls'; git commit -m fix",
            "(cd /tmp && ls); git commit -m fix",
            // #2580: a surviving pin still pins.
            "cd /tmp/f; env -u GIT_DIR GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f git commit -m x",
            // #2581: real config reads.
            "cd /tmp/f && git config list && git config get user.name",
        ] {
            assert_eq!(
                classify(command),
                "allow",
                "{command:?}: {}",
                refusal(command)
            );
        }
        // The `git` tool has no shell and no `cd`: nothing to chain.
        for line in [
            "commit -m x",
            "reset --mixed HEAD",
            "add -A",
            "branch topic",
        ] {
            let mut scan = Scan::default();
            scan_git(&git_words(line), &mut scan);
            assert_eq!(scan.refusal, None, "{line}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    macro_rules! rsi_common_manifest_dir {
        () => {
            rsi_common::manifest_dir!()
        };
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    mod external_fixtures {
        include!("custody_scan_fixtures.rs");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn external_commands_require_independent_scratch_and_exact_dispatch() {
        let fixture = external_fixtures::ExternalFixture::new();
        let cancel = CancellationToken::new();
        let probe = Probe {
            session_id: None,
            invocation_id: None,
            working_dir: &fixture.sandbox,
            cancel: &cancel,
            sandbox_base: &fixture.base,
        };
        for (label, command, reason) in &fixture.refused {
            let message = check_shell(command, &probe)
                .await
                .expect(label)
                .message()
                .to_owned();
            assert!(message.contains(reason.as_str()), "{label}: {message}");
        }
        if fixture.shipped_subtree() {
            for command in fixture.allowed() {
                assert!(check_shell(&command, &probe).await.is_none(), "{command}");
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn external_commands_canonicalize_symlinked_parents() {
        let fixture = external_fixtures::ExternalFixture::new();
        let cancel = CancellationToken::new();
        let probe = Probe {
            session_id: None,
            invocation_id: None,
            working_dir: &fixture.sandbox,
            cancel: &cancel,
            sandbox_base: &fixture.base,
        };
        let scratch_parent = fixture.symlink(fixture.root.path(), "scratch-parent");
        let managed_parent = fixture.symlink(&fixture.base, "managed-parent");
        let refused = format!(
            "git -C {}/assigned subtree split --prefix=d -b escaped HEAD",
            managed_parent.display()
        );
        let message = check_shell(&refused, &probe)
            .await
            .expect("managed symlink")
            .message()
            .to_owned();
        assert!(
            message.contains("independent scratch repository"),
            "{message}"
        );
        if fixture.shipped_subtree() {
            let allowed = format!(
                "git -C {}/scratch subtree split --prefix=d -b desktop-only HEAD",
                scratch_parent.display()
            );
            assert!(check_shell(&allowed, &probe).await.is_none(), "{allowed}");
        }
    }

    // The probe-backed tests below run real git in a throwaway repository.

    use super::test_support::{git, repo};

    async fn check(command: &str, dir: &Path) -> Option<String> {
        let cancel = CancellationToken::new();
        let probe = Probe {
            session_id: None,
            invocation_id: None,
            working_dir: dir,
            cancel: &cancel,
            sandbox_base: dir,
        };
        check_shell(command, &probe)
            .await
            .map(|refusal| refusal.message().to_owned())
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_discard_is_allowed_on_a_clean_tree_and_refused_on_dirty_paths() {
        let repo = repo();
        let dir = repo.path();
        for command in ["git checkout -- a.txt", "git restore .", "git clean -fd"] {
            assert_eq!(check(command, dir).await, None, "clean tree: {command}");
        }

        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        for command in [
            "git checkout -- a.txt",
            "git checkout -- .",
            "git checkout .",
            "git checkout HEAD -- a.txt",
            "git checkout -f",
            "git restore a.txt",
            "git restore --worktree .",
            "bash -c 'git restore .'",
        ] {
            let message = check(command, dir).await.unwrap_or_default();
            assert!(
                message.contains("hard rules 8 and 9"),
                "{command}: {message}"
            );
            assert!(message.contains("a.txt"), "{command}: {message}");
        }
        // Only the dirty path is at risk; the other file and the index are not.
        assert_eq!(check("git checkout -- b.txt", dir).await, None);
        assert_eq!(check("git restore --staged a.txt", dir).await, None);
        assert_eq!(check("git clean -fd", dir).await, None);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "changed\n"
        );
    }

    /// #156: mid-merge, taking one side of an unmerged path passes; a dirty
    /// path that is not unmerged keeps the refusal, which names that form.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_conflict_side_resolution_mid_merge_is_allowed_only_for_unmerged_paths() {
        let repo = repo();
        let dir = repo.path();
        git(dir, &["branch", "other"]);
        std::fs::write(dir.join("a.txt"), "ours\n").unwrap();
        git(dir, &["commit", "-q", "-am", "ours"]);
        git(dir, &["checkout", "-q", "other"]);
        std::fs::write(dir.join("a.txt"), "theirs\n").unwrap();
        git(dir, &["commit", "-q", "-am", "theirs"]);
        git(dir, &["checkout", "-q", "main"]);
        let merge = std::process::Command::new("git")
            .args(["merge", "-q", "other"])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git merge");
        assert!(!merge.status.success(), "the merge must conflict");
        std::fs::write(dir.join("b.txt"), "real work\n").unwrap();
        for command in [
            "git checkout --theirs -- a.txt",
            "git checkout --ours -- a.txt",
            "git restore --theirs a.txt",
        ] {
            assert_eq!(check(command, dir).await, None, "{command}");
        }
        let b = vec!["b.txt".to_owned()];
        assert_eq!(
            check("git checkout --theirs -- b.txt", dir).await,
            Some(dirty_during_merge("git checkout", &b))
        );
        assert_eq!(
            check("git checkout -- a.txt", dir).await,
            Some(dirty_during_merge("git checkout", &["a.txt".to_owned()]))
        );
        let mut scan = Scan::default();
        scan_git(&git_words("restore --theirs a.txt"), &mut scan);
        let cancel = CancellationToken::new();
        let probe = Probe {
            session_id: None,
            invocation_id: None,
            working_dir: dir,
            cancel: &cancel,
            sandbox_base: dir,
        };
        assert!(settle(scan, &probe).await.is_none(), "the git tool argv");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn clean_f_is_refused_only_for_files_it_would_delete() {
        let repo = repo();
        let dir = repo.path();
        std::fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
        git(dir, &["add", ".gitignore"]);
        git(dir, &["commit", "-q", "-m", "ignore"]);
        std::fs::write(dir.join("ignored.log"), "x").unwrap();
        // Ignored files survive `clean -f` and `-X` is the only one to take them.
        assert_eq!(check("git clean -fd", dir).await, None);
        let message = check("git clean -fdX", dir).await.unwrap_or_default();
        assert!(message.contains("ignored.log"), "{message}");
        let message = check("git clean -fdx", dir).await.unwrap_or_default();
        assert!(message.contains("ignored.log"), "{message}");

        std::fs::write(dir.join("scratch.txt"), "x").unwrap();
        let message = check("git clean -fd", dir).await.unwrap_or_default();
        assert!(message.contains("scratch.txt"), "{message}");
        assert!(!message.contains("ignored.log"), "{message}");
        assert_eq!(check("git clean -fdX scratch.txt", dir).await, None);
        assert_eq!(check("git clean -nfd", dir).await, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_discard_after_a_directory_change_is_refused_as_unverifiable() {
        let repo = repo();
        let dir = repo.path();
        std::fs::create_dir(dir.join("sub")).unwrap();
        for command in [
            "cd sub && git checkout -- ../a.txt",
            "git -C sub restore .",
            "git --work-tree=. checkout -- a.txt",
            "GIT_DIR=.git git restore .",
            // #2289: `env -C` changes the child's directory, not the probe's.
            "env -C sub git checkout -- ../a.txt",
            "env --chdir=sub git restore .",
            "env --chdir sub git clean -fd",
            "env -iC sub git restore .",
            "sudo env -C sub git checkout -- ../a.txt",
            "bash -c 'env -C sub git restore .'",
        ] {
            let message = check(command, dir).await.unwrap_or_default();
            assert!(message.contains("cannot check"), "{command}: {message}");
            assert!(
                message.contains("hard rules 8 and 9"),
                "{command}: {message}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_discard_sharing_its_line_is_refused_even_on_a_clean_tree() {
        let repo = repo();
        let dir = repo.path();
        // #2290: the probe ran before the shell, so `sleep 10` could let an edit
        // land that the probe never saw.
        for command in [
            "sleep 1; git checkout -- a.txt",
            "sleep 1 && git restore .",
            "git status && git restore .",
            "git clean -fd; git status",
            "(git clean -fd)",
            "git restore . &",
            "git restore \"$(sleep 1; echo a.txt)\"",
            "bash -c 'sleep 1; git restore .'",
            "echo x | git restore .",
        ] {
            let message = check(command, dir).await.unwrap_or_default();
            assert!(message.contains("shares its line"), "{command}: {message}");
            assert!(
                message.contains("hard rules 8 and 9"),
                "{command}: {message}"
            );
        }
        // Alone, with the usual wrappers, it is still allowed on a clean tree.
        for command in [
            "git checkout -- a.txt",
            "git restore .",
            "git clean -fd",
            "env FOO=1 git clean -fd",
            "time git restore a.txt",
            "bash -c 'git restore .'",
            "git restore a.txt # undo the edit\n",
            "git status && git log -1 && git restore --staged a.txt",
        ] {
            assert_eq!(check(command, dir).await, None, "alone: {command}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_configured_alias_is_refused_and_external_commands_need_scratch_custody() {
        let repo = repo();
        let dir = repo.path();
        git(dir, &["config", "alias.sx", "!git stash"]);
        git(dir, &["config", "alias.co", "checkout"]);
        git(dir, &["config", "alias.Wip", "status -s"]);
        for command in [
            "git sx",
            "git SX pop",
            "git co main",
            "git wip",
            "git --no-pager sx",
            "git -c core.pager=cat co -- a.txt",
            "bash -c 'git sx'",
            "echo $(git sx)",
        ] {
            let message = check(command, dir).await.unwrap_or_default();
            assert!(
                message.contains("git alias from your git configuration"),
                "{command}: {message}"
            );
            assert!(
                message.contains("hard rules 1, 2 and 9"),
                "{command}: {message}"
            );
        }
        for command in ["git lfs version", "git nosuchcommand"] {
            let message = check(command, dir)
                .await
                .expect("external command needs scratch custody");
            assert!(
                message.contains("independent scratch repository"),
                "{message}"
            );
        }
        for command in [
            "git status",
            "git log --oneline -1",
            "git config --get alias.sx",
            "git checkout -- a.txt",
        ] {
            assert_eq!(check(command, dir).await, None, "{command}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn an_unknown_subcommand_is_refused_when_its_aliases_cannot_be_checked() {
        let repo = repo();
        let dir = repo.path();
        std::fs::create_dir(dir.join("sub")).unwrap();
        for command in [
            "git -C sub lfs ls-files",
            "cd sub && git foo",
            "env -C sub git foo",
        ] {
            let message = check(command, dir).await.unwrap_or_default();
            assert!(
                message.contains("may be a git alias"),
                "{command}: {message}"
            );
            assert!(message.contains("cannot check"), "{command}: {message}");
        }
        // Built-ins cannot hide an alias, so relocating them is unaffected.
        assert_eq!(check("git -C sub status", dir).await, None);
        assert_eq!(check("cd sub && git log -1", dir).await, None);

        let message = check("git foo", Path::new("/nonexistent/custody-test"))
            .await
            .unwrap_or_default();
        assert!(message.contains("`git config` failed"), "{message}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_discard_outside_a_repository_is_refused_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        // The scratch root can itself sit inside a worktree (a sandbox's
        // `target/.rsi-tmp`), which would make this directory part of a
        // repository. A `.git` file naming a missing gitdir makes git stop
        // here and report "not a repository" wherever the tempdir lives.
        std::fs::write(
            dir.path().join(".git"),
            "gitdir: /nonexistent/custody-test-gitdir\n",
        )
        .unwrap();
        let message = check("git checkout -- a.txt", dir.path())
            .await
            .unwrap_or_default();
        assert!(message.contains("`git status` failed"), "{message}");
    }

    /// #2319: `GIT_CONFIG_GLOBAL=<file> git x` ran an alias from a file the
    /// probe never read.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn a_config_source_override_is_refused_for_a_non_builtin_subcommand() {
        let repo = repo();
        let dir = repo.path();
        let evil = tempfile::tempdir().unwrap();
        let file = evil.path().join("evil.gitconfig");
        std::fs::write(&file, "[alias]\n\tx = !git stash\n").unwrap();
        let file = file.display();
        let home = evil.path().display();
        for command in [
            format!("GIT_CONFIG_GLOBAL={file} git x"),
            format!("GIT_CONFIG_SYSTEM={file} git x"),
            format!("GIT_CONFIG={file} git x"),
            "GIT_CONFIG_NOSYSTEM=0 git x".to_owned(),
            format!("HOME={home} git x"),
            format!("XDG_CONFIG_HOME={home} git x"),
            format!("env GIT_CONFIG_GLOBAL={file} git x"),
            format!("env -i GIT_CONFIG_GLOBAL={file} git x"),
            format!("export GIT_CONFIG_GLOBAL={file}; git x"),
            format!("export GIT_CONFIG_GLOBAL={file} && git x"),
            format!("bash -c 'GIT_CONFIG_GLOBAL={file} git x'"),
            format!("GIT_CONFIG_GLOBAL={file} git --no-pager x"),
        ] {
            let message = check(&command, dir).await.unwrap_or_default();
            assert!(
                message.contains("may be a git alias"),
                "{command}: {message}"
            );
            assert!(
                message.contains("points git at other configuration"),
                "{command}: {message}"
            );
            assert!(
                message.contains("hard rules 1, 2 and 9"),
                "{command}: {message}"
            );
        }
        let message = check("git x", dir)
            .await
            .expect("unproven external command");
        assert!(
            message.contains("independent scratch repository"),
            "{message}"
        );
        // A built-in cannot be shadowed by an alias, so it is unaffected.
        for command in [
            format!("GIT_CONFIG_GLOBAL={file} git status"),
            "GIT_CONFIG_GLOBAL=/dev/null git log -1".to_owned(),
            format!("HOME={home} git diff"),
            format!("export GIT_CONFIG_GLOBAL={file}; git log -1"),
        ] {
            assert_eq!(check(&command, dir).await, None, "{command}");
        }
    }

    /// #2319: the same bypass through the depth limit, end to end.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn check_shell_refuses_a_script_nested_past_the_depth_limit() {
        let repo = repo();
        let dir = repo.path();
        let too_deep = nest_bash("git checkout main", MAX_DEPTH + 1);
        let message = check(&too_deep, dir).await.unwrap_or_default();
        assert!(message.contains("more than 4 levels deep"), "{message}");
        assert!(message.contains("hard rules 1, 2 and 9"), "{message}");
        assert_eq!(check(&nest_bash("git status", MAX_DEPTH), dir).await, None);
    }
}
