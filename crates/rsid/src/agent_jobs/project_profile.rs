//! Versioned project declarations, resolved without running project code (#412).
//! These are inputs to future job/landing/resource adapters, never authority.

use super::recipe::{MAX_MANIFEST_BYTES, parse_manifest, target_token, validate_recipes};
pub use super::recipe::{Recipe, Runner};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const PROFILE_PATH: &str = ".rsi/project.toml";
const MAX_RESOURCES: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectProfile {
    pub version: u32,
    #[serde(default)]
    pub recipes: BTreeMap<String, Recipe>,
    /// Recipe names, in execution order. An empty list is never a passing test.
    #[serde(default)]
    pub verification: Vec<String>,
    pub build: Option<String>,
    pub integration: Option<Integration>,
    #[serde(default)]
    pub warnings: WarningPolicy,
    #[serde(default)]
    pub resources: BTreeMap<String, Resource>,
    pub cleanup: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Integration {
    pub branch: String,
    pub mode: LandingMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LandingMode {
    Queue,
    FastForward,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarningPolicy {
    /// No Rust baseline is presumed. An adapter must report applicability.
    #[default]
    Generic,
    /// Preserve the Rust no-new-warning policy; this is not a waiver.
    NoNewRustWarnings,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    /// One non-overlapping directory relative to the exact custody root.
    pub path: String,
    pub setup: Option<String>,
    pub cleanup: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    Project,
    LegacyJobs,
    LocalEvidence,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProjectCommand {
    Recipe(Recipe),
    CargoTest,
    CargoBuild,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "status", content = "commands", rename_all = "snake_case")]
pub enum Verification {
    Available(Vec<ProjectCommand>),
    NoTests,
    /// Local files exist, but this resolver cannot prove a supported test role.
    Unsupported,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub root: PathBuf,
    pub source: ProfileSource,
    pub recipes: BTreeMap<String, Recipe>,
    pub verification: Verification,
    pub build: Option<ProjectCommand>,
    /// None disables publication by default; no branch is guessed from Git.
    pub integration: Option<Integration>,
    pub warnings: WarningPolicy,
    pub resources: BTreeMap<String, Resource>,
    pub cleanup: Option<ProjectCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileError {
    Io,
    Invalid,
    UnknownVersion(u32),
    UnsafePath(String),
    AmbiguousOwnership(String),
    ConflictingRecipe(String),
    UnknownRecipe(String),
    MissingRecipeFile(String),
}

/// Parse only the declaration. Filesystem ownership is checked by `resolve`.
pub fn parse(text: &str) -> Result<ProjectProfile, ProfileError> {
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ProfileError::Invalid);
    }
    // Inspect the version first so future formats receive a useful refusal.
    let value: toml::Value = toml::from_str(text).map_err(|_| ProfileError::Invalid)?;
    let version = value
        .get("version")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(ProfileError::Invalid)?;
    if version != 1 {
        return Err(ProfileError::UnknownVersion(version));
    }
    let profile: ProjectProfile = value.try_into().map_err(|_| ProfileError::Invalid)?;
    validate_recipes(&profile.recipes).map_err(|_| ProfileError::Invalid)?;
    if profile.verification.len() > 64 || profile.resources.len() > MAX_RESOURCES {
        return Err(ProfileError::Invalid);
    }
    if let Some(integration) = &profile.integration {
        if !branch_name(&integration.branch) {
            return Err(ProfileError::Invalid);
        }
    }
    for (name, resource) in &profile.resources {
        if !target_token(name) {
            return Err(ProfileError::Invalid);
        }
        relative_directory(&resource.path)?;
    }
    for name in role_names(&profile) {
        if !target_token(name) || matches!(name, "scoped-test" | "scoped-test-tmpfs") {
            return Err(ProfileError::Invalid);
        }
    }
    Ok(profile)
}

fn branch_name(name: &str) -> bool {
    // Conservative portable subset of Git branch names, without spawning Git.
    !name.is_empty()
        && name.len() <= 200
        && name != "HEAD"
        && !name.starts_with('-')
        && !name.starts_with("refs/")
        && !name.contains("..")
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
}

fn role_names(profile: &ProjectProfile) -> Vec<&str> {
    profile
        .verification
        .iter()
        .map(String::as_str)
        .chain(profile.build.as_deref())
        .chain(profile.cleanup.as_deref())
        .chain(profile.resources.values().flat_map(|r| {
            [r.setup.as_deref(), r.cleanup.as_deref()]
                .into_iter()
                .flatten()
        }))
        .collect()
}

fn relative_directory(path: &str) -> Result<(), ProfileError> {
    // Use '/' on every platform; reject Windows prefixes even on Unix.
    if path.is_empty()
        || path.len() > 512
        || path.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.starts_with('.')
                || part.ends_with('.')
                || part
                    .bytes()
                    .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'-' | b'_' | b'.'))
                || matches!(
                    part.split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
    {
        return Err(ProfileError::UnsafePath(path.into()));
    }
    Ok(())
}

fn redirected(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions and other reparse points can redirect directory ownership.
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Recheck immediately before any future resource operation. A resolved profile
/// is a snapshot, not a custody grant or protection against filesystem races.
pub fn resource_path(root: &Path, relative: &str) -> Result<PathBuf, ProfileError> {
    relative_directory(relative)?;
    let root = root.canonicalize().map_err(|_| ProfileError::Io)?;
    let mut path = root;
    for part in relative.split('/') {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if redirected(&meta) || !meta.is_dir() => {
                return Err(ProfileError::UnsafePath(relative.into()));
            }
            Ok(_) => {
                // A nested worktree/repository is another ownership domain.
                match std::fs::symlink_metadata(path.join(".git")) {
                    Ok(_) => return Err(ProfileError::AmbiguousOwnership(relative.into())),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err(ProfileError::Io),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ProfileError::Io),
        }
    }
    Ok(path)
}

/// Read a bounded local regular file, refusing even internal symlink aliases.
pub(super) fn local_text(root: &Path, relative: &str) -> Result<Option<String>, ProfileError> {
    let mut path = root.to_path_buf();
    let parts: Vec<_> = relative.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta)
                if redirected(&meta)
                    || (i + 1 < parts.len() && !meta.is_dir())
                    || (i + 1 == parts.len() && !meta.is_file()) =>
            {
                return Err(ProfileError::UnsafePath(relative.into()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ProfileError::Io),
        }
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_MANIFEST_BYTES + 1).read_to_string(&mut text))
        .map_err(|_| ProfileError::Io)?;
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ProfileError::Invalid);
    }
    Ok(Some(text))
}

fn recipe_command(
    recipes: &BTreeMap<String, Recipe>,
    name: &str,
) -> Result<ProjectCommand, ProfileError> {
    let recipe = recipes
        .get(name)
        .ok_or_else(|| ProfileError::UnknownRecipe(name.into()))?;
    if matches!(recipe.target.as_str(), "scoped-test" | "scoped-test-tmpfs") {
        return Err(ProfileError::Invalid);
    }
    Ok(ProjectCommand::Recipe(recipe.clone()))
}

/// Resolve declarations against one caller-proven custody root. No commands are
/// run, resources created, settings written, receipts minted or landing enabled.
pub fn resolve(custody_root: &Path) -> Result<ResolvedProfile, ProfileError> {
    let root = custody_root.canonicalize().map_err(|_| ProfileError::Io)?;
    if !root.is_dir() {
        return Err(ProfileError::Io);
    }
    let declaration = local_text(&root, PROFILE_PATH)?;
    let legacy = local_text(&root, ".rsi/jobs.toml")?;
    let mut recipes = match &legacy {
        Some(text) => {
            parse_manifest(text)
                .map_err(|_| ProfileError::Invalid)?
                .recipes
        }
        None => BTreeMap::new(),
    };
    if let Some(text) = declaration {
        let profile = parse(&text)?;
        for (name, recipe) in &profile.recipes {
            if recipes.get(name).is_some_and(|old| old != recipe) {
                return Err(ProfileError::ConflictingRecipe(name.clone()));
            }
            recipes.insert(name.clone(), recipe.clone());
        }
        validate_recipes(&recipes).map_err(|_| ProfileError::Invalid)?;
        for name in role_names(&profile) {
            let ProjectCommand::Recipe(recipe) = recipe_command(&recipes, name)? else {
                unreachable!("recipe_command always returns a recipe");
            };
            let file = match recipe.runner {
                Runner::Make => "Makefile",
                Runner::Just => "justfile",
            };
            if local_text(&root, file)?.is_none() {
                return Err(ProfileError::MissingRecipeFile(file.into()));
            }
        }
        let mut owned: Vec<String> = Vec::new();
        for resource in profile.resources.values() {
            resource_path(&root, &resource.path)?;
            // Case folding is conservative on Linux, necessary on macOS/Windows.
            let path = resource.path.to_ascii_lowercase();
            if owned.iter().any(|old| {
                path == *old
                    || path.starts_with(&format!("{old}/"))
                    || old.starts_with(&format!("{path}/"))
            }) {
                return Err(ProfileError::AmbiguousOwnership(resource.path.clone()));
            }
            owned.push(path);
        }
        let commands = profile
            .verification
            .iter()
            .map(|name| recipe_command(&recipes, name))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(ResolvedProfile {
            root,
            source: ProfileSource::Project,
            verification: if commands.is_empty() {
                Verification::NoTests
            } else {
                Verification::Available(commands)
            },
            build: profile
                .build
                .as_deref()
                .map(|name| recipe_command(&recipes, name))
                .transpose()?,
            cleanup: profile
                .cleanup
                .as_deref()
                .map(|name| recipe_command(&recipes, name))
                .transpose()?,
            recipes,
            integration: profile.integration,
            warnings: profile.warnings,
            resources: profile.resources,
        });
    }
    fallback(root, recipes, legacy.is_some())
}

fn literal_target(text: &str, target: &str) -> bool {
    text.lines().any(|line| {
        // No parsing includes, variables, shell recipes or parameterized tasks.
        !line.starts_with(char::is_whitespace)
            && line.strip_prefix(target).is_some_and(|tail| {
                tail.starts_with(':') && !tail.starts_with(":=") && !tail.starts_with("::")
            })
    })
}

fn fallback(
    root: PathBuf,
    recipes: BTreeMap<String, Recipe>,
    legacy: bool,
) -> Result<ResolvedProfile, ProfileError> {
    let makefile = local_text(&root, "Makefile")?;
    let justfile = local_text(&root, "justfile")?;
    let cargo = local_text(&root, "Cargo.toml")?;
    let rust = match &cargo {
        Some(text) => {
            let value: toml::Value = toml::from_str(text).map_err(|_| ProfileError::Invalid)?;
            value.get("package").is_some_and(toml::Value::is_table)
                || value.get("workspace").is_some_and(toml::Value::is_table)
        }
        None => false,
    };
    let infer = |role: &str| -> Result<Option<ProjectCommand>, ProfileError> {
        if recipes.contains_key(role) {
            let recipe = &recipes[role];
            let (text, file) = match recipe.runner {
                Runner::Make => (&makefile, "Makefile"),
                Runner::Just => (&justfile, "justfile"),
            };
            if text.is_none() {
                return Err(ProfileError::MissingRecipeFile(file.into()));
            }
            return recipe_command(&recipes, role).map(Some);
        }
        for (text, runner) in [(&makefile, Runner::Make), (&justfile, Runner::Just)] {
            if text
                .as_deref()
                .is_some_and(|text| literal_target(text, role))
            {
                return Ok(Some(ProjectCommand::Recipe(Recipe {
                    runner,
                    target: role.into(),
                    timeout_minutes: 20,
                    cpu_quota_percent: 200,
                })));
            }
        }
        Ok(None)
    };
    let verification = match infer("test")? {
        Some(command) => Verification::Available(vec![command]),
        None if rust => Verification::Available(vec![ProjectCommand::CargoTest]),
        None if makefile.is_some() || justfile.is_some() || cargo.is_some() || legacy => {
            Verification::Unsupported
        }
        None => Verification::NoTests,
    };
    let build = infer("build")?.or(rust.then_some(ProjectCommand::CargoBuild));
    Ok(ResolvedProfile {
        root,
        source: if legacy {
            ProfileSource::LegacyJobs
        } else {
            ProfileSource::LocalEvidence
        },
        recipes,
        verification,
        build,
        integration: None,
        warnings: if rust {
            WarningPolicy::NoNewRustWarnings
        } else {
            WarningPolicy::Generic
        },
        resources: BTreeMap::new(),
        cleanup: None,
    })
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-other-01")
))]
mod tests;
