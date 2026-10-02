use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

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
        || metadata.len() > 16 * 1024
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe remote policy file",
        ));
    }
    let mut content = String::new();
    file.take(16 * 1024 + 1).read_to_string(&mut content)?;
    if content.len() > 16 * 1024 {
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

pub fn write(path: &Path, config: &Config, create_only: bool) -> io::Result<()> {
    config.validate()?;
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
    if path.exists() || fs::symlink_metadata(path).is_ok() {
        if create_only {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "policy exists",
            ));
        }
        read(path)?;
    }
    let content = toml::to_string_pretty(config).map_err(io::Error::other)?;
    let temp = temporary_path(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
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
}
