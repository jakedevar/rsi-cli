use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Largest policy file the reader accepts. The writer enforces the same bound
/// so an accepted edit can never produce a file the gateway cannot read.
pub const MAX_POLICY_BYTES: usize = 16 * 1024;

/// Local operator policy. This foundation does not grant data access even when enabled.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub canonical_host: String,
    #[serde(default)]
    pub owner_user_id: u64,
    #[serde(default)]
    pub allowed_node_ids: Vec<String>,
    #[serde(default)]
    pub project_ids: Vec<String>,
}

impl Config {
    pub fn validate(&self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let host = &self.canonical_host;
        if self.owner_user_id == 0
            || self.allowed_node_ids.is_empty()
            || self.project_ids.is_empty()
            || self.project_ids.len() > 32
            || !canonical_fqdn(host)
            || self
                .allowed_node_ids
                .iter()
                .any(|id| id.is_empty() || id.len() > 128)
            || self.project_ids.iter().any(|id| !canonical_uuid(id))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid remote policy",
            ));
        }
        Ok(())
    }
}

fn canonical_fqdn(host: &str) -> bool {
    let labels: Vec<_> = host.split('.').collect();
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn canonical_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

pub fn read(path: &Path) -> io::Result<Config> {
    // Pin validation and parsing to one descriptor. A path replaced between
    // metadata and read must never make us parse a different file.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_POLICY_BYTES as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe remote policy file",
        ));
    }
    let mut content = String::new();
    file.take(MAX_POLICY_BYTES as u64 + 1)
        .read_to_string(&mut content)?;
    if content.len() > MAX_POLICY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote policy too large",
        ));
    }
    let config: Config = toml::from_str(&content)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid remote policy"))?;
    config.validate()?;
    Ok(config)
}

/// Serialize `config`, refusing a policy the reader would reject.
pub fn encode(config: &Config) -> io::Result<String> {
    config.validate()?;
    let content = toml::to_string_pretty(config).map_err(io::Error::other)?;
    if content.len() > MAX_POLICY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote policy too large",
        ));
    }
    Ok(content)
}

fn check_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "policy path has no parent"))?;
    let dir = fs::symlink_metadata(parent)?;
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    let uid = unsafe { libc::geteuid() };
    if !dir.is_dir() || dir.uid() != uid || dir.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe policy directory",
        ));
    }
    Ok(())
}

pub fn write(path: &Path, config: &Config, create_only: bool) -> io::Result<()> {
    let content = encode(config)?;
    check_parent(path)?;
    if path.exists() || fs::symlink_metadata(path).is_ok() {
        if create_only {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "policy exists",
            ));
        }
        read(path)?;
    }
    replace_atomically(path, &content)
}

/// Replace a policy that may be unreadable (corrupt, oversized, too open) so
/// the operator can always disable Remote. The target must still be a regular
/// file we own: a symlink or foreign file is never overwritten.
pub fn write_replacing_unreadable(path: &Path, config: &Config) -> io::Result<()> {
    let content = encode(config)?;
    check_parent(path)?;
    if let Ok(existing) = fs::symlink_metadata(path) {
        // SAFETY: geteuid has no preconditions and does not dereference pointers.
        let uid = unsafe { libc::geteuid() };
        if !existing.file_type().is_file() || existing.uid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe remote policy file",
            ));
        }
    }
    replace_atomically(path, &content)
}

fn replace_atomically(path: &Path, content: &str) -> io::Result<()> {
    let temp = temporary_path(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        // Explicit, so a permissive umask or inherited ACL cannot widen it.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn temporary_path(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid policy name"))?;
    let mut temp = path.to_path_buf();
    temp.set_file_name(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn enabled_requires_explicit_scoped_policy() {
        let mut config = Config {
            enabled: true,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        config.canonical_host = "host.example.ts.net".into();
        config.owner_user_id = 1;
        config.allowed_node_ids.push("node-1".into());
        config
            .project_ids
            .push("550e8400-e29b-41d4-a716-446655440000".into());
        assert!(config.validate().is_ok());
        config.project_ids[0] = "550E8400-e29b-41d4-a716-446655440000".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn canonical_host_rejects_empty_and_hyphen_edge_labels() {
        let mut config = Config {
            enabled: true,
            canonical_host: "host.example.ts.net".into(),
            owner_user_id: 1,
            allowed_node_ids: vec!["node-1".into()],
            project_ids: vec!["550e8400-e29b-41d4-a716-446655440000".into()],
        };
        assert!(config.validate().is_ok());
        for host in [
            "host..ts.net",
            "-host.ts.net",
            "host-.ts.net",
            "host.ts.net.",
            "host.TS.net",
        ] {
            config.canonical_host = host.into();
            assert!(config.validate().is_err(), "{host}");
        }
        config.canonical_host = format!("{}.ts.net", "a".repeat(64));
        assert!(config.validate().is_err());
    }

    #[test]
    fn policy_read_rejects_symlink_and_oversized_file() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("rsi-remote-config-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("policy.toml");
        fs::write(&path, "enabled = false\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read(&path).is_ok());
        let link = dir.join("link.toml");
        symlink(&path, &link).unwrap();
        assert!(read(&link).is_err());
        fs::write(&path, "x".repeat(16 * 1024 + 1)).unwrap();
        assert!(read(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    fn enabled_with_nodes(count: usize) -> Config {
        Config {
            enabled: true,
            canonical_host: "host.example.ts.net".into(),
            owner_user_id: 1,
            allowed_node_ids: (0..count).map(|n| format!("{n:0>128}")).collect(),
            project_ids: vec!["550e8400-e29b-41d4-a716-446655440000".into()],
        }
    }

    #[test]
    fn writer_refuses_a_policy_the_reader_would_reject_and_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("policy.toml");
        write(&path, &enabled_with_nodes(2), true).unwrap();
        let before = fs::read(&path).unwrap();
        let error = write(&path, &enabled_with_nodes(200), false).unwrap_err();
        assert!(error.to_string().contains("too large"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(read(&path).is_ok());
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn replacing_writer_recovers_an_unreadable_policy_but_never_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("policy.toml");
        fs::write(&path, "x".repeat(MAX_POLICY_BYTES + 5)).unwrap();
        assert!(read(&path).is_err());
        assert!(write(&path, &Config::default(), false).is_err());
        write_replacing_unreadable(&path, &Config::default()).unwrap();
        assert!(read(&path).is_ok_and(|policy| !policy.enabled));
        let target = dir.path().join("other.toml");
        fs::write(&target, "keep").unwrap();
        let link = dir.path().join("link.toml");
        symlink(&target, &link).unwrap();
        assert!(write_replacing_unreadable(&link, &Config::default()).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
    }
}
