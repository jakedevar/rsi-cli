//! Non-Linux cross-check discovery and bounded execution (#253/#256). Never
//! install a toolchain from a job; uncovered host capability stays explicit.
use super::{process, provenance::SourcePin};
use crate::Ctx;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

pub const TARGET: &str = "x86_64-unknown-linux-gnu";
const CROSS_GCC: &str = "x86_64-linux-gnu-gcc";
const CC_VARS: &[&str] = &[
    "CC_x86_64_unknown_linux_gnu",
    "CC_x86_64-unknown-linux-gnu",
    "TARGET_CC",
];
fn remedy(darwin: bool) -> String {
    if darwin {
        "brew install zig".into()
    } else {
        format!("install zig (https://ziglang.org/download/) or {CROSS_GCC}")
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Toolchain {
    Configured { cc: String, variable: String },
    CrossGcc { cc: PathBuf },
    Zig { zig: PathBuf },
}
fn which(ctx: &Ctx, name: &str) -> Option<PathBuf> {
    let mut names = vec![name.to_owned()];
    if cfg!(windows) {
        names.extend(
            ctx.var("PATHEXT")
                .unwrap_or(".COM;.EXE;.BAT;.CMD")
                .split(';')
                .map(|ext| format!("{name}{ext}")),
        );
    }
    std::env::split_paths(ctx.var("PATH").unwrap_or_default()).find_map(|dir| {
        names.iter().find_map(|name| {
            let path = ctx.path(&dir).join(name);
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            Some(path)
        })
    })
}
pub fn discover(ctx: &Ctx) -> Option<Toolchain> {
    discover_with(ctx, cfg!(windows), |name| which(ctx, name))
}
fn discover_with(
    ctx: &Ctx,
    windows: bool,
    mut find: impl FnMut(&str) -> Option<PathBuf>,
) -> Option<Toolchain> {
    for variable in CC_VARS {
        if let Some(cc) = ctx.var(variable).filter(|v| !v.is_empty()) {
            return Some(Toolchain::Configured {
                cc: cc.into(),
                variable: (*variable).into(),
            });
        }
    }
    if let Some(cc) = find(CROSS_GCC) {
        return Some(Toolchain::CrossGcc { cc });
    }
    if !windows {
        if let Some(zig) = find("zig") {
            return Some(Toolchain::Zig { zig });
        }
    }
    None
}
pub fn installed_targets(ctx: &Ctx, cancel: &AtomicUsize) -> Option<BTreeSet<String>> {
    let mut stdout = String::new();
    let mut oversized = false;
    let done = process::stream_channels(
        &[
            "rustup".into(),
            "target".into(),
            "list".into(),
            "--installed".into(),
        ],
        &ctx.cwd,
        &process::build_env(ctx),
        Instant::now() + Duration::from_secs(60),
        cancel,
        |pipe, line| {
            if pipe == 0 {
                if stdout.len() + line.len() <= 1024 * 1024 {
                    stdout.push_str(line);
                } else {
                    oversized = true;
                }
            }
        },
        |_, _| {},
    )
    .ok()?;
    if done.code != 0 || !done.output_complete || oversized {
        return None;
    }
    Some(
        stdout
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Into::into)
            .collect(),
    )
}
pub fn choice(
    linux: bool,
    packages: &[String],
    installed: Option<&BTreeSet<String>>,
    toolchain: Option<&Toolchain>,
) -> Option<Value> {
    if linux {
        return None;
    }
    let mut entry = json!({"target":TARGET,"packages":packages});
    let missing = if packages.is_empty() {
        Some(("not_applicable", "no_touched_rust_package"))
    } else if installed.is_none() {
        Some(("not_covered", "rustup_unavailable"))
    } else if !installed.is_some_and(|targets| targets.contains(TARGET)) {
        Some(("not_covered", "linux_target_missing"))
    } else if toolchain.is_none() {
        Some(("not_covered", "c_toolchain_missing"))
    } else {
        None
    };
    if let Some((status, reason)) = missing {
        entry["status"] = json!(status);
        entry["reason"] = json!(reason);
        if ["rustup_unavailable", "linux_target_missing"].contains(&reason) {
            entry["remedy"] = json!(format!("rustup target add {TARGET}"));
        } else if reason == "c_toolchain_missing" {
            entry["remedy"] = json!(remedy(cfg!(target_os = "macos")));
        }
    } else {
        entry["status"] = json!("pending");
        entry["c_toolchain"] = json!(toolchain);
    }
    Some(entry)
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
fn wrapper(zig: &Path, tool: &str) -> String {
    let zig = shell_quote(&zig.to_string_lossy());
    if tool == "ar" {
        return format!(
            "#!/bin/sh\n# Written by the Rust scoped-test Linux lane (#256).\nexec {zig} ar \"$@\"\n"
        );
    }
    format!(
        "#!/bin/sh\n# Written by the Rust scoped-test Linux lane (#256).\nskip=0\nfor arg do\n  shift\n  if [ \"$skip\" = 1 ]; then skip=0; continue; fi\n  case $arg in\n    --target=*) continue ;;\n    -target|--target) skip=1; continue ;;\n  esac\n  set -- \"$@\" \"$arg\"\ndone\nexec {zig} {tool} -target x86_64-linux-gnu \"$@\"\n"
    )
}
/// Only the cross lane sees these variables. Wrapper paths/bytes stay stable
/// across runs, avoiding a spurious build-script fingerprint change.
pub fn cross_env(toolchain: &Toolchain, target: &Path) -> io::Result<BTreeMap<String, String>> {
    let Toolchain::Zig { zig } = toolchain else {
        return Ok(BTreeMap::new());
    };
    if cfg!(windows) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "zig shell wrappers unavailable on Windows",
        ));
    }
    let root = target.join(TARGET).join("rsi-zig-cc");
    std::fs::create_dir_all(&root)?;
    let mut vars = BTreeMap::new();
    for (name, tool, key) in [
        ("cc", "cc", "CC_x86_64_unknown_linux_gnu"),
        ("c++", "c++", "CXX_x86_64_unknown_linux_gnu"),
        ("ar", "ar", "AR_x86_64_unknown_linux_gnu"),
    ] {
        let path = root.join(name);
        let text = wrapper(zig, tool);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::other("cross wrapper path is a symlink"));
        }
        if std::fs::read(&path).ok().as_deref() != Some(text.as_bytes()) {
            std::fs::write(&path, text)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::metadata(&path)?.permissions();
            if permissions.mode() & 0o755 != 0o755 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        vars.insert(key.into(), path.to_string_lossy().into_owned());
    }
    for key in ["ZIG_GLOBAL_CACHE_DIR", "ZIG_LOCAL_CACHE_DIR"] {
        vars.insert(
            key.into(),
            root.join("zig-cache").to_string_lossy().into_owned(),
        );
    }
    Ok(vars)
}
pub fn command(
    cargo: &[String],
    packages: &[String],
    features: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut args = cargo.to_vec();
    args.extend(
        [
            "check",
            "--target",
            TARGET,
            "--all-targets",
            "--keep-going",
            "--message-format=json",
        ]
        .map(Into::into),
    );
    for package in packages {
        args.extend(["-p".into(), package.clone()]);
    }
    let features = packages
        .iter()
        .flat_map(|package| {
            features
                .get(package)
                .into_iter()
                .flatten()
                .map(move |feature| format!("{package}/{feature}"))
        })
        .collect::<Vec<_>>();
    if !features.is_empty() {
        args.extend(["--features".into(), features.join(",")]);
    }
    args
}
#[derive(Default)]
pub struct Diagnostics {
    rust_errors: usize,
    first_rust: Option<String>,
    scripts: Vec<String>,
    cross_c: bool,
    first_plain: Option<String>,
}
fn compact(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(400)
        .collect()
}
impl Diagnostics {
    pub fn observe(&mut self, line: &str) {
        static NOISE: crate::re::Lazy = std::sync::LazyLock::new(|| {
            crate::re::Re::new(r"^(?:\d+ warnings? emitted|aborting due to .*)$")
        });
        static SCRIPT: crate::re::Lazy = std::sync::LazyLock::new(|| {
            crate::re::Re::new(r"failed to run custom build command for `([^`]+)`")
        });
        static CROSS_C: crate::re::Lazy = std::sync::LazyLock::new(|| {
            crate::re::Re::new(
                r"(?i)failed to find tool|ToolNotFound|ToolExecError|error occurred in cc-rs|occurred in cc-rs|is `[\w.+-]*` installed|linker `[^`]+` not found|x86_64-linux-gnu-|cross-compil|cannot find (?:-l|crt)|sysroot",
            )
        });
        let text = line.trim_end_matches('\n');
        if text.starts_with('{') {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                if value.is_object() {
                    let detail = &value["message"];
                    let message = detail["message"].as_str().unwrap_or("");
                    if value["reason"] == "compiler-message"
                        && detail["level"] == "error"
                        && !NOISE.is_match(message)
                    {
                        self.rust_errors += 1;
                        if self.first_rust.is_none() {
                            let code = detail["code"]["code"]
                                .as_str()
                                .map(|s| format!("[{s}]"))
                                .unwrap_or_default();
                            let location = detail["spans"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .find(|span| span["is_primary"] == true)
                                .map(|span| {
                                    format!(
                                        " ({}:{})",
                                        span["file_name"].as_str().unwrap_or("?"),
                                        span["line_start"]
                                    )
                                })
                                .unwrap_or_default();
                            self.first_rust =
                                Some(compact(&format!("error{code}: {message}{location}")));
                        }
                    }
                    return;
                }
            }
        }
        let script = SCRIPT
            .search(text)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()));
        if let Some(script) = &script {
            if !self.scripts.contains(script) {
                self.scripts.push(script.chars().take(400).collect());
            }
        }
        self.cross_c |= CROSS_C.is_match(text);
        if text.starts_with("error") && script.is_none() && self.first_plain.is_none() {
            self.first_plain = Some(compact(text));
        }
    }
    pub fn verdict(&self, code: i32, timed_out: bool) -> Value {
        let mut result = json!({"rust_errors":self.rust_errors});
        let (status, reason) = if code == 0 && !timed_out {
            ("passed", None)
        } else if self.rust_errors > 0 {
            result["first_error"] = json!(self.first_rust);
            ("failed", Some("linux_check_failed"))
        } else if timed_out {
            ("not_covered", Some("timed_out"))
        } else if !self.scripts.is_empty() {
            result["build_scripts"] = json!(self.scripts);
            (
                "not_covered",
                Some(if self.cross_c {
                    "c_toolchain_missing"
                } else {
                    "build_script_failed"
                }),
            )
        } else {
            if let Some(error) = &self.first_plain {
                result["first_error"] = json!(error);
            }
            ("not_covered", Some("cargo_failed"))
        };
        result["status"] = json!(status);
        if let Some(reason) = reason {
            result["reason"] = json!(reason);
        }
        result
    }
}

pub struct Check<'a> {
    pub source: &'a SourcePin,
    pub cargo: &'a [String],
    pub packages: &'a [String],
    pub features: &'a BTreeMap<String, Vec<String>>,
    pub toolchain: &'a Toolchain,
    pub log_dir: &'a Path,
    pub budget: Duration,
}
/// Execute only after the parent holds its single admission. Package selection
/// must include changed Rust/manifests and the complete downstream compile set.
pub fn run_pending(
    entry: &mut Value,
    check: &Check<'_>,
    ctx: &Ctx,
    cancel: &AtomicUsize,
) -> io::Result<()> {
    if ctx.var("RSI_CARGO_SLOT_HELD") != Some("1")
        || entry["status"] != "pending"
        || check.packages.is_empty()
        || check.cargo.is_empty()
        || check.budget.is_zero()
        || ctx.cwd != check.source.repo
        || entry["target"] != TARGET
        || entry["packages"] != json!(check.packages)
        || entry["c_toolchain"] != json!(check.toolchain)
    {
        return Err(io::Error::other(
            "Linux cross check requires pending scope, matching source and held admission",
        ));
    }
    let target = ctx
        .var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.cwd.join("target"));
    let target = ctx.path(target);
    let lane_env = match cross_env(check.toolchain, &target) {
        Ok(env) => env,
        Err(error) => {
            entry["status"] = json!("not_covered");
            entry["reason"] = json!("c_toolchain_setup_failed");
            entry["first_error"] = json!(compact(&error.to_string()));
            entry["remedy"] = json!(remedy(cfg!(target_os = "macos")));
            return Ok(());
        }
    };
    let mut env = process::build_env(ctx);
    for key in [
        "CC_x86_64_unknown_linux_gnu",
        "CC_x86_64-unknown-linux-gnu",
        "TARGET_CC",
        "CXX_x86_64_unknown_linux_gnu",
        "CXX_x86_64-unknown-linux-gnu",
        "TARGET_CXX",
        "AR_x86_64_unknown_linux_gnu",
        "AR_x86_64-unknown-linux-gnu",
        "TARGET_AR",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER",
        "PKG_CONFIG_ALLOW_CROSS",
        "PKG_CONFIG_SYSROOT_DIR",
        "PKG_CONFIG_SYSROOT_DIR_x86_64_unknown_linux_gnu",
        "PKG_CONFIG_PATH_x86_64_unknown_linux_gnu",
    ] {
        if let Some(value) = ctx.var(key) {
            env.insert(key.into(), value.into());
        }
    }
    env.extend(lane_env.clone());
    if !lane_env.is_empty() {
        entry["c_toolchain"]["env"] = json!(lane_env.keys().collect::<Vec<_>>());
    }
    let command = command(check.cargo, check.packages, check.features);
    let path = check.log_dir.join("linux-check.log");
    entry["command"] = json!(command);
    entry["log"] = json!(path);
    let mut log = std::fs::File::create(path)?;
    let mut diagnostics = Diagnostics::default();
    let mut write_error = None;
    let started = Instant::now();
    let done = check.source.monitored(&env, cancel, |cancel| {
        process::stream(
            &command,
            &ctx.cwd,
            &env,
            Instant::now() + check.budget,
            cancel,
            |line| {
                diagnostics.observe(line);
                if let Err(error) = log.write_all(line.as_bytes()).and_then(|()| log.flush()) {
                    write_error = Some(error);
                }
            },
            |_| {},
        )
    })?;
    if let Some(error) = write_error {
        return Err(error);
    }
    if done.cancelled.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Linux cross check interrupted",
        ));
    }
    if !done.output_complete {
        return Err(io::Error::other("Linux cross check output was incomplete"));
    }
    entry
        .as_object_mut()
        .ok_or_else(|| io::Error::other("Linux check entry must be object"))?
        .extend(
            diagnostics
                .verdict(done.code, done.timed_out)
                .as_object()
                .expect("verdict object")
                .clone(),
        );
    entry["exit_code"] = json!(done.code);
    entry["elapsed_sec"] = json!(started.elapsed().as_secs_f64());
    if entry["reason"] == "c_toolchain_missing" {
        entry["remedy"] = json!(remedy(cfg!(target_os = "macos")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compiler_precedence_and_windows_no_shell_fallback_match_the_lane_contract() {
        for variable in CC_VARS {
            let ctx = Ctx::new(".", &[(variable, "configured compiler")]);
            assert!(
                matches!(discover_with(&ctx,false,|_|panic!("configured compiler has priority")),Some(Toolchain::Configured{cc,..})if cc=="configured compiler")
            );
        }
        let ctx = Ctx::new(".", &[]);
        assert!(matches!(
            discover_with(&ctx, false, |name| Some(PathBuf::from(name))),
            Some(Toolchain::CrossGcc { .. })
        ));
        assert!(matches!(
            discover_with(&ctx, false, |name| (name == "zig")
                .then(|| PathBuf::from(name))),
            Some(Toolchain::Zig { .. })
        ));
        assert!(
            discover_with(&ctx, true, |name| (name == "zig")
                .then(|| PathBuf::from(name)))
            .is_none()
        );
        let packages = vec!["pkg".into()];
        let installed = BTreeSet::from([TARGET.into()]);
        let toolchain = Toolchain::CrossGcc {
            cc: CROSS_GCC.into(),
        };
        assert!(choice(true, &packages, None, None).is_none());
        assert_eq!(
            choice(false, &[], None, None).unwrap()["status"],
            "not_applicable"
        );
        assert_eq!(
            choice(false, &packages, None, None).unwrap()["reason"],
            "rustup_unavailable"
        );
        assert_eq!(
            choice(false, &packages, Some(&BTreeSet::new()), None).unwrap()["reason"],
            "linux_target_missing"
        );
        assert_eq!(
            choice(false, &packages, Some(&installed), None).unwrap()["reason"],
            "c_toolchain_missing"
        );
        assert_eq!(
            choice(false, &packages, Some(&installed), Some(&toolchain)).unwrap()["status"],
            "pending"
        );
    }
    #[test]
    fn command_and_diagnostics_preserve_features_and_rust_error_priority() {
        let command = command(
            &["cargo".into()],
            &["pkg".into(), "other".into()],
            &BTreeMap::from([("pkg".into(), vec!["one".into(), "two".into()])]),
        );
        assert_eq!(
            command,
            [
                "cargo",
                "check",
                "--target",
                TARGET,
                "--all-targets",
                "--keep-going",
                "--message-format=json",
                "-p",
                "pkg",
                "-p",
                "other",
                "--features",
                "pkg/one,pkg/two"
            ]
        );
        let mut d = Diagnostics::default();
        d.observe("error: failed to run custom build command for `sqlite v1`");
        d.observe("cc-rs: x86_64-linux-gnu-gcc missing");
        assert_eq!(d.verdict(1, false)["reason"], "c_toolchain_missing");
        assert_eq!(d.verdict(124, true)["reason"], "timed_out");
        d.observe(&json!({"reason":"compiler-message","message":{"level":"error","message":"type mismatch","code":{"code":"E0308"},"spans":[{"file_name":"src/lib.rs","line_start":7,"is_primary":true}]}}).to_string());
        d.observe(&json!({"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error"}}).to_string());
        let verdict = d.verdict(124, true);
        assert_eq!(verdict["status"], "failed");
        assert_eq!(verdict["rust_errors"], 1);
        assert_eq!(
            verdict["first_error"],
            "error[E0308]: type mismatch (src/lib.rs:7)"
        );
        assert_eq!(Diagnostics::default().verdict(0, false)["status"], "passed");
        assert_eq!(
            Diagnostics::default().verdict(1, false)["reason"],
            "cargo_failed"
        );
    }
    #[cfg(unix)]
    #[test]
    fn zig_wrappers_strip_target_flags_preserve_argv_and_reuse_unchanged_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let zig = dir.path().join("zig's tool");
        std::fs::write(&zig, "#!/bin/sh\nprintf '<%s>\\n' \"$@\"\n").unwrap();
        std::fs::set_permissions(&zig, std::fs::Permissions::from_mode(0o755)).unwrap();
        let toolchain = Toolchain::Zig { zig };
        let env = cross_env(&toolchain, dir.path()).unwrap();
        let cc = PathBuf::from(&env["CC_x86_64_unknown_linux_gnu"]);
        let modified = std::fs::metadata(&cc).unwrap().modified().unwrap();
        let done = std::process::Command::new(&cc)
            .args([
                "--target=wrong",
                "--target",
                "wrong",
                "-target",
                "wrong",
                "-Dspace=a b",
                "-c",
                "input.c",
            ])
            .output()
            .unwrap();
        assert!(done.status.success());
        assert_eq!(
            String::from_utf8(done.stdout).unwrap(),
            "<cc>\n<-target>\n<x86_64-linux-gnu>\n<-Dspace=a b>\n<-c>\n<input.c>\n"
        );
        assert_eq!(cross_env(&toolchain, dir.path()).unwrap(), env);
        assert_eq!(
            std::fs::metadata(&cc).unwrap().modified().unwrap(),
            modified
        );
        let done = std::process::Command::new(&env["AR_x86_64_unknown_linux_gnu"])
            .args(["crs", "lib.a"])
            .output()
            .unwrap();
        assert_eq!(done.stdout, b"<ar>\n<crs>\n<lib.a>\n");
    }
    #[cfg(unix)]
    #[test]
    fn admitted_cross_process_preserves_logs_errors_deadline_and_local_environment() {
        let dir = tempfile::Builder::new()
            .prefix("rsi-linux-check-")
            .tempdir_in("/tmp")
            .unwrap();
        crate::testutil::git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        crate::testutil::git(dir.path(), &["add", ".gitignore"]);
        crate::testutil::git(dir.path(), &["commit", "-qm", "cross fixture"]);
        let target = dir.path().join("target");
        let log_dir = target.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let path = std::env::var("PATH").unwrap();
        let target_text = target.to_string_lossy().into_owned();
        let ctx = Ctx::new(
            dir.path(),
            &[
                ("PATH", &path),
                ("RSI_CARGO_SLOT_HELD", "1"),
                ("CARGO_TARGET_DIR", &target_text),
                ("TARGET_CC", "configured compiler"),
            ],
        );
        let source = SourcePin::capture(&ctx, "HEAD", "HEAD").unwrap();
        let packages = vec!["pkg".into()];
        let features = BTreeMap::new();
        let toolchain = discover_with(&ctx, false, |_| None).unwrap();
        let script = target.join("check-stub.sh");
        let cargo = vec!["/bin/sh".into(), script.to_string_lossy().into_owned()];
        let mut check = Check {
            source: &source,
            cargo: &cargo,
            packages: &packages,
            features: &features,
            toolchain: &toolchain,
            log_dir: &log_dir,
            budget: Duration::from_secs(3),
        };
        let pending = || {
            choice(
                false,
                &packages,
                Some(&BTreeSet::from([TARGET.into()])),
                Some(&toolchain),
            )
            .unwrap()
        };
        std::fs::write(&script, "printf 'compiler:%s\\n' \"$TARGET_CC\"; exit 0\n").unwrap();
        let mut entry = pending();
        run_pending(&mut entry, &check, &ctx, &AtomicUsize::new(0)).unwrap();
        assert_eq!(entry["status"], "passed");
        assert!(
            std::fs::read_to_string(log_dir.join("linux-check.log"))
                .unwrap()
                .contains("compiler:configured compiler")
        );
        assert_eq!(ctx.var("TARGET_CC"), Some("configured compiler"));
        assert_eq!(ctx.var("ZIG_GLOBAL_CACHE_DIR"), None);
        let diagnostic = json!({"reason":"compiler-message","message":{"level":"error","message":"cross type error","code":{"code":"E0308"},"spans":[]}});
        std::fs::write(
            &script,
            format!("printf '%s\\n' '{}'; exit 1\n", diagnostic),
        )
        .unwrap();
        let mut entry = pending();
        run_pending(&mut entry, &check, &ctx, &AtomicUsize::new(0)).unwrap();
        assert_eq!(entry["status"], "failed");
        assert_eq!(entry["reason"], "linux_check_failed");
        std::fs::write(&script, "sleep 30 & wait\n").unwrap();
        check.budget = Duration::from_millis(80);
        let started = Instant::now();
        let mut entry = pending();
        run_pending(&mut entry, &check, &ctx, &AtomicUsize::new(0)).unwrap();
        assert_eq!(entry["status"], "not_covered");
        assert_eq!(entry["reason"], "timed_out");
        assert!(started.elapsed() < Duration::from_secs(2));
        let mut entry = pending();
        let without_admission = Ctx::new(dir.path(), &[("PATH", &path)]);
        assert!(run_pending(&mut entry, &check, &without_admission, &AtomicUsize::new(0)).is_err());
        assert_eq!(entry["status"], "pending");
    }
}
