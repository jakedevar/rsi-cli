//! #192: the lander's environment as one explicit value.
//!
//! The lander reads its knobs (`RSI_LANDER_*`, `CARGO_TARGET_DIR`, ...) from
//! this value and hands it whole to every child it starts (git, cargo,
//! python3, rustc, the guard commands): [`command`] and [`tokio_command`]
//! clear the inherited environment and set exactly these variables. Production
//! takes one snapshot of the process environment at startup ([`init`] in
//! `main`; the canary runner adds its `CARGO_TARGET_DIR` with
//! [`set_startup_var`]). #3391: the snapshot keeps only the variables
//! `rsid::lander_env_policy` allows (gate tooling, locale, `RSI_LANDER_*`
//! knobs, a documented few), so a build script, test module or git hook the
//! gate runs from agent-written sources never sees a credential in the
//! daemon's user environment. A test installs
//! its own value for the current thread ([`LanderEnv::enter`]), so a fixture's
//! fake `PATH`, target directory or knob never reaches the process environment
//! that concurrent tests spawn processes under. Work moved to another thread
//! ([`propagate`]) carries the value with it.

use std::collections::BTreeMap;
use std::env::VarError;
use std::ffi::{OsStr, OsString};
use std::sync::{Arc, PoisonError, RwLock};

/// A complete environment: every variable a child gets and every knob the
/// lander reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LanderEnv {
    vars: BTreeMap<OsString, OsString>,
}

static PROCESS: RwLock<Option<Arc<LanderEnv>>> = RwLock::new(None);

#[cfg(test)]
thread_local! {
    static SCOPED: std::cell::RefCell<Option<Arc<LanderEnv>>> =
        const { std::cell::RefCell::new(None) };
}

impl LanderEnv {
    pub(crate) fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self {
            vars: vars.into_iter().collect(),
        }
    }

    /// The lander's startup environment from the process's variables: only
    /// the names `rsid::lander_env_policy` allows (#3391).
    pub(crate) fn from_process_vars(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self::from_vars(rsid::lander_env_policy::filter(vars))
    }

    /// `std::env::var_os` over this environment.
    pub(crate) fn var_os(&self, name: impl AsRef<OsStr>) -> Option<OsString> {
        self.vars.get(name.as_ref()).cloned()
    }

    /// `std::env::var` over this environment (same errors).
    pub(crate) fn var(&self, name: impl AsRef<OsStr>) -> Result<String, VarError> {
        match self.vars.get(name.as_ref()) {
            None => Err(VarError::NotPresent),
            Some(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| VarError::NotUnicode(value.clone())),
        }
    }

    /// Every variable, as a base environment for a child.
    pub(crate) const fn as_map(&self) -> &BTreeMap<OsString, OsString> {
        &self.vars
    }

    /// Give `command`'s child exactly this environment. Later `env` and
    /// `env_remove` calls on `command` still apply on top of it.
    pub(crate) fn apply<'a>(
        &self,
        command: &'a mut std::process::Command,
    ) -> &'a mut std::process::Command {
        command.env_clear().envs(&self.vars)
    }

    #[cfg(test)]
    pub(crate) fn with_var(
        mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    #[cfg(test)]
    pub(crate) fn without_var(mut self, name: impl AsRef<OsStr>) -> Self {
        self.vars.remove(name.as_ref());
        self
    }

    /// Make this the current thread's lander environment until the returned
    /// scope drops (it restores the one it replaced; drop scopes in reverse
    /// order of entry).
    #[cfg(test)]
    pub(crate) fn enter(self) -> Scope {
        let previous = SCOPED.with(|scoped| scoped.replace(Some(Arc::new(self))));
        Scope { previous }
    }
}

/// A test's lander environment, installed for the current thread.
#[cfg(test)]
#[must_use = "the environment is current only while the scope lives"]
pub(crate) struct Scope {
    previous: Option<Arc<LanderEnv>>,
}

#[cfg(test)]
impl Drop for Scope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SCOPED.with(|scoped| *scoped.borrow_mut() = previous);
    }
}

fn process() -> Arc<LanderEnv> {
    if let Some(env) = PROCESS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
    {
        return Arc::clone(env);
    }
    let mut slot = PROCESS.write().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(slot.get_or_insert_with(|| Arc::new(startup_env())))
}

/// The process environment reduced to the allowlist (#3391).
fn startup_env() -> LanderEnv {
    LanderEnv::from_process_vars(std::env::vars_os())
}

/// Take the process snapshot now. `main` calls this before any work, so
/// every later read sees the startup environment.
pub(crate) fn init() {
    let _ = process();
}

/// Set one variable in the process snapshot. Only for a change the binary
/// makes to its own environment at startup, before any child starts (the
/// canary runner's `CARGO_TARGET_DIR`); the caller makes the same change to
/// the process environment.
pub(crate) fn set_startup_var(name: impl Into<OsString>, value: impl Into<OsString>) {
    let mut slot = PROCESS.write().unwrap_or_else(PoisonError::into_inner);
    let env = slot.get_or_insert_with(|| Arc::new(startup_env()));
    Arc::make_mut(env).vars.insert(name.into(), value.into());
}

/// The lander environment of the current thread: a test's installed value,
/// else the process snapshot.
pub(crate) fn current() -> Arc<LanderEnv> {
    #[cfg(test)]
    if let Some(scoped) = SCOPED.with(|scoped| scoped.borrow().clone()) {
        return scoped;
    }
    process()
}

pub(crate) fn var(name: impl AsRef<OsStr>) -> Result<String, VarError> {
    current().var(name)
}

pub(crate) fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    current().var_os(name)
}

/// `std::env::vars_os` over the current lander environment.
pub(crate) fn vars_os() -> Vec<(OsString, OsString)> {
    current()
        .as_map()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// `std::process::Command::new(program)` with the current lander environment.
/// The program is looked up on that environment's `PATH`.
pub(crate) fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    current().apply(&mut command);
    command
}

/// [`command`] for the async runtime.
pub(crate) fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    current().apply(command.as_std_mut());
    command
}

/// Carry the current lander environment into `work` when it runs on another
/// thread (`spawn_blocking`, `std::thread::spawn`).
#[cfg(test)]
pub(crate) fn propagate<R>(work: impl FnOnce() -> R) -> impl FnOnce() -> R {
    let env = current();
    move || {
        let _scope = Arc::unwrap_or_clone(env).enter();
        work()
    }
}

/// Production has one environment for every thread.
#[cfg(not(test))]
pub(crate) const fn propagate<R, F: FnOnce() -> R>(work: F) -> F {
    work
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #3391: a child the lander starts does not see a planted credential or
    /// the session token, and still sees the gate variables it needs.
    #[cfg(unix)]
    #[test]
    fn a_lander_child_sees_gate_variables_and_no_planted_credential() {
        let os = |value: &str| OsString::from(value);
        let startup = LanderEnv::from_process_vars([
            (os("PATH"), std::env::var_os("PATH").unwrap_or_default()),
            (os("HOME"), os("/home/gate")),
            (os("FOO_API_KEY"), os("planted-key")),
            (os("RSI_SESSION_TOKEN"), os("planted-token")),
            (os("ANTHROPIC_FOO"), os("planted-anthropic")),
            (os("CARGO_TARGET_DIR"), os("/queue/target")),
            (os("RSI_LANDER_EXACT_GATE"), os("1")),
            (os("RUSTC_WRAPPER"), os("sccache")),
            (os("LC_ALL"), os("C")),
        ]);
        assert_eq!(startup.var_os("FOO_API_KEY"), None);
        assert_eq!(startup.var_os("RSI_SESSION_TOKEN"), None);
        let _scope = startup.enter();
        let output = command("env").output().expect("env runs");
        let text = String::from_utf8_lossy(&output.stdout);
        for required in [
            "HOME=/home/gate",
            "CARGO_TARGET_DIR=/queue/target",
            "RSI_LANDER_EXACT_GATE=1",
            "RUSTC_WRAPPER=sccache",
            "LC_ALL=C",
        ] {
            assert!(
                text.lines().any(|line| line == required),
                "{required}: {text}"
            );
        }
        assert!(!text.contains("planted"), "{text}");
    }
}
