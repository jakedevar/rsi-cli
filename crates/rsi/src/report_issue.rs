//! `rsi report-issue` (#3789): file a harness defect upstream as a GitHub Issue.
//!
//! Works on any install, paired with the hub or not: it needs only the
//! GitHub CLI `gh`, signed in on THIS machine. Every field is scrubbed (tokens,
//! credentials, e-mail addresses, home-directory user names, this machine's
//! secret environment values) before it reaches `gh`; the report carries the
//! versioned template marker and title prefix the hub imports. rsi never asks
//! for or stores a token. The pure logic is `rsi_common::external_report`.

use rsi_common::external_report::{
    GhError, GhRunner, ReportDraft, create_args, default_repo, local_identity_literals,
    render_report, validate_repo,
};
use std::io::Read;

pub const USAGE: &str = "\
  rsi report-issue --title TEXT --failure TEXT [--steps TEXT] [--evidence TEXT | --evidence-file PATH|-]
                   [--repo OWNER/REPO] [--dry-run]
      Report a harness defect to https://github.com/jakedevar/rsi-cli/issues
      with this machine's own `gh` login; no repository write access or label
      permission is needed. The hub recognizes the versioned template marker
      and [rsi-report] title prefix. --failure is what
      failed, as someone would search for it (an error line, a refusal code);
      reports of the same failure are deduplicated by it. Tokens, e-mail
      addresses and home-directory names are redacted before sending; --dry-run
      prints the redacted report and sends nothing.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportIssueArgs {
    pub title: String,
    pub failure: String,
    pub steps: String,
    pub evidence: Evidence,
    pub repo: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    None,
    Text(String),
    File(String),
}

/// `None` when `args` is not `report-issue`.
#[must_use]
pub fn parse(args: &[String]) -> Option<Result<ReportIssueArgs, String>> {
    let (command, rest) = args.split_first()?;
    (command == "report-issue").then(|| parse_flags(rest))
}

fn parse_flags(args: &[String]) -> Result<ReportIssueArgs, String> {
    let mut title = None;
    let mut failure = None;
    let mut steps = String::new();
    let mut evidence = Evidence::None;
    let mut repo = default_repo().to_string();
    let mut dry_run = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--title" => title = Some(value("--title")?),
            "--failure" => failure = Some(value("--failure")?),
            "--steps" => steps = value("--steps")?,
            "--evidence" => evidence = Evidence::Text(value("--evidence")?),
            "--evidence-file" => evidence = Evidence::File(value("--evidence-file")?),
            "--repo" => repo = value("--repo")?,
            "--dry-run" => dry_run = true,
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    validate_repo(&repo)?;
    Ok(ReportIssueArgs {
        title: title
            .filter(|title| !title.trim().is_empty())
            .ok_or("`rsi report-issue` needs --title")?,
        failure: failure
            .filter(|failure| !failure.trim().is_empty())
            .ok_or("`rsi report-issue` needs --failure")?,
        steps,
        evidence,
        repo,
        dry_run,
    })
}

/// Secret values in this process's environment (`RSI_SESSION_TOKEN`, `GH_TOKEN`,
/// any variable named like a credential) to remove by value from the report.
fn secret_env_values() -> Vec<String> {
    std::env::vars()
        .filter(|(name, value)| {
            let name = name.to_ascii_uppercase();
            value.len() >= 8
                && [
                    "TOKEN",
                    "SECRET",
                    "PASSWORD",
                    "PASSWD",
                    "API_KEY",
                    "APIKEY",
                    "CREDENTIAL",
                ]
                .iter()
                .any(|needle| name.contains(needle))
        })
        .map(|(_, value)| value)
        .collect()
}

fn read_evidence(evidence: &Evidence) -> Result<String, String> {
    match evidence {
        Evidence::None => Ok(String::new()),
        Evidence::Text(text) => Ok(text.clone()),
        Evidence::File(path) if path == "-" => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("cannot read evidence from stdin: {error}"))?;
            Ok(text)
        }
        Evidence::File(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read evidence file {path}: {error}")),
    }
}

/// The draft for this machine: version, build and platform are filled in here.
#[must_use]
pub fn draft_for(args: &ReportIssueArgs, evidence: String) -> ReportDraft {
    ReportDraft {
        title: args.title.clone(),
        failure: args.failure.clone(),
        steps: args.steps.clone(),
        evidence,
        rsi_version: env!("CARGO_PKG_VERSION").to_string(),
        build_sha: option_env!("RSI_BUILD_SHA")
            .unwrap_or("unknown")
            .to_string(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    }
}

/// Render, scrub and (unless `--dry-run`) file the report. Returns the text to
/// print: the redacted report for a dry run, else the Issue URL `gh` printed.
pub fn run(
    args: &ReportIssueArgs,
    evidence: String,
    extra_literals: &[String],
    gh: &dyn GhRunner,
) -> Result<String, GhError> {
    let mut literals = local_identity_literals();
    literals.extend_from_slice(extra_literals);
    let report = render_report(&draft_for(args, evidence), &literals);
    if args.dry_run {
        return Ok(format!(
            "title: {}\nsignature: {}\n\n{}",
            report.title, report.signature, report.body
        ));
    }
    let output = gh
        .run(&create_args(&args.repo, &report), Some(&report.body))?
        .checked()?;
    Ok(output.stdout.trim().to_string())
}

/// The `rsi report-issue` entry point: read evidence, then [`run`] with `gh`.
pub fn run_cli(args: &ReportIssueArgs) -> Result<String, String> {
    let evidence = read_evidence(&args.evidence)?;
    run(
        args,
        evidence,
        &secret_env_values(),
        &rsi_common::external_report::SystemGh::default(),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::external_report::GhOutput;
    use std::sync::Mutex;

    struct FakeGh {
        calls: Mutex<Vec<(Vec<String>, Option<String>)>>,
        reply: GhOutput,
    }

    impl GhRunner for FakeGh {
        fn run(&self, args: &[String], stdin: Option<&str>) -> Result<GhOutput, GhError> {
            self.calls
                .lock()
                .unwrap()
                .push((args.to_vec(), stdin.map(str::to_string)));
            Ok(self.reply.clone())
        }
    }

    fn args(flags: &[&str]) -> Vec<String> {
        std::iter::once("report-issue")
            .chain(flags.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn ok_gh() -> FakeGh {
        FakeGh {
            calls: Mutex::new(Vec::new()),
            reply: GhOutput {
                success: true,
                stdout: "https://github.com/o/r/issues/7\n".into(),
                stderr: String::new(),
            },
        }
    }

    #[test]
    fn only_report_issue_is_parsed() {
        assert!(parse(&["land".to_string()]).is_none());
        assert!(parse(&args(&["--title", "t"])).unwrap().is_err());
        assert!(
            parse(&args(&[
                "--title",
                "t",
                "--failure",
                "f",
                "--repo",
                "--web"
            ]))
            .unwrap()
            .is_err()
        );
    }

    #[test]
    fn report_is_scrubbed_and_sent_to_public_repo_without_label_permissions() {
        // Built at run time so no secret-shaped literal sits in the source.
        let token = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        let session_token = "sessiontok-0123456789";
        let parsed = parse(&args(&[
            "--title",
            &format!("stall for dev@example.com using {token}"),
            "--failure",
            "queue_gate_stalled",
            "--steps",
            "enqueue then wait",
        ]))
        .unwrap()
        .unwrap();
        let gh = ok_gh();
        let evidence = format!(
            "log from /home/someone/rsi\nRSI_SESSION_TOKEN={session_token}\ncontact dev@example.com\n{token}"
        );
        let url = run(&parsed, evidence, &[session_token.to_string()], &gh).unwrap();
        assert_eq!(url, "https://github.com/o/r/issues/7");

        let calls = gh.calls.lock().unwrap();
        let (argv, body) = &calls[0];
        let expected_repo = "https://github.com/jakedevar/rsi-cli"
            .strip_prefix("https://github.com/")
            .unwrap();
        assert_eq!(
            argv,
            &vec![
                "issue",
                "create",
                "--repo",
                expected_repo,
                "--title",
                &argv[5],
                "--body-file",
                "-"
            ]
        );
        assert!(argv[5].starts_with(rsi_common::external_report::REPORT_TITLE_PREFIX));
        let sent = format!("{} {}", argv.join(" "), body.as_deref().unwrap());
        for leaked in [token.as_str(), session_token, "example.com", "someone"] {
            assert!(!sent.contains(leaked), "`{leaked}` reached gh: {sent}");
        }
        let body = body.as_deref().unwrap();
        assert!(body.contains("## Failure signature"));
        assert!(body.contains("rsi version:"));
        assert!(body.contains("platform:"));
        // No credential flag or env is ever passed on the command line.
        assert!(!argv.iter().any(|arg| arg.contains("token")));
    }

    #[test]
    fn dry_run_sends_nothing_and_unauthenticated_gh_says_how_to_log_in() {
        let mut parsed = parse(&args(&["--title", "t", "--failure", "f", "--dry-run"]))
            .unwrap()
            .unwrap();
        let gh = ok_gh();
        let shown = run(&parsed, String::new(), &[], &gh).unwrap();
        assert!(shown.contains(rsi_common::external_report::REPORT_MARKER));
        assert!(shown.starts_with("title: [rsi-report] t\n"));
        assert!(gh.calls.lock().unwrap().is_empty());

        parsed.dry_run = false;
        let denied = FakeGh {
            calls: Mutex::new(Vec::new()),
            reply: GhOutput {
                success: false,
                stdout: String::new(),
                stderr: "To get started with GitHub CLI, please run:  gh auth login".into(),
            },
        };
        let error = run(&parsed, String::new(), &[], &denied).unwrap_err();
        assert_eq!(error, GhError::Unauthenticated);
    }

    #[test]
    fn missing_and_expired_auth_refuse_once_without_credential_fallback() {
        struct RefusedGh {
            error: GhError,
            calls: Mutex<usize>,
        }
        impl GhRunner for RefusedGh {
            fn run(&self, _: &[String], _: Option<&str>) -> Result<GhOutput, GhError> {
                *self.calls.lock().unwrap() += 1;
                Err(self.error)
            }
        }
        let parsed = parse(&args(&["--title", "t", "--failure", "f"]))
            .unwrap()
            .unwrap();
        for error in [GhError::Missing, GhError::Unauthenticated, GhError::Expired] {
            let gh = RefusedGh {
                error,
                calls: Mutex::new(0),
            };
            assert_eq!(run(&parsed, String::new(), &[], &gh), Err(error));
            assert_eq!(*gh.calls.lock().unwrap(), 1);
        }
        let denied = FakeGh {
            calls: Mutex::new(Vec::new()),
            reply: GhOutput {
                success: false,
                stdout: String::new(),
                stderr: "token expired: nonstandard!short~credential".into(),
            },
        };
        assert_eq!(
            run(&parsed, String::new(), &[], &denied),
            Err(GhError::Expired)
        );
        assert_eq!(denied.calls.lock().unwrap().len(), 1);
    }
}
