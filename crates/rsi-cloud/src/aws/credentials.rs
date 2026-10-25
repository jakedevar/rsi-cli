//! Static AWS environment/shared-profile credentials only. No subprocess, SSO,
//! container or metadata fallback: unsupported providers fail closed.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{Error, Result};

// Deliberately no Debug/Display: these values must never enter diagnostics.
pub(super) struct Credentials {
    pub access: String,
    pub secret: String,
    pub token: Option<String>,
}

fn section(text: &str, wanted: &str) -> Result<BTreeMap<String, String>> {
    let mut selected = false;
    let mut values = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            selected = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) == Some(wanted);
        } else if selected {
            let (key, value) = line.split_once('=').ok_or(Error("invalid AWS profile"))?;
            if values
                .insert(key.trim().to_ascii_lowercase(), value.trim().to_owned())
                .is_some()
            {
                return Err(Error("duplicate AWS profile setting"));
            }
        }
    }
    Ok(values)
}

fn read_section(path: &Path, name: &str) -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => section(&text, name),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(_) => Err(Error("cannot read AWS profile")),
    }
}

pub(super) fn load(
    profile_flag: Option<&str>,
    region_flag: Option<&str>,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(Credentials, String)> {
    let profile = profile_flag
        .map(str::to_owned)
        .or_else(|| env("AWS_PROFILE"))
        .or_else(|| env("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|| "default".into());
    let home = env("HOME")
        .or_else(|| env("USERPROFILE"))
        .map(PathBuf::from);
    let path = |variable: &str, file: &str| -> Option<PathBuf> {
        env(variable)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".aws").join(file)))
    };
    let config_section = if profile == "default" {
        profile.clone()
    } else {
        format!("profile {profile}")
    };
    let config = path("AWS_CONFIG_FILE", "config")
        .map(|path| read_section(&path, &config_section))
        .transpose()?
        .unwrap_or_default();
    let region = region_flag
        .map(str::to_owned)
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| config.get("region").cloned())
        .ok_or(Error("AWS region is required"))?;

    // An explicit --profile disables environment credentials, like the AWS CLI.
    let environment = if profile_flag.is_none() {
        let access = env("AWS_ACCESS_KEY_ID");
        let secret = env("AWS_SECRET_ACCESS_KEY");
        match (access, secret) {
            (None, None) => None,
            (Some(access), Some(secret)) => Some(Credentials {
                access,
                secret,
                token: env("AWS_SESSION_TOKEN"),
            }),
            _ => return Err(Error("incomplete AWS environment credentials")),
        }
    } else {
        None
    };
    let credentials = if let Some(credentials) = environment {
        credentials
    } else {
        if [
            "role_arn",
            "credential_process",
            "credential_source",
            "source_profile",
            "sso_session",
            "sso_start_url",
            "web_identity_token_file",
        ]
        .iter()
        .any(|key| config.contains_key(*key))
        {
            return Err(Error(
                "unsupported AWS credential provider; supply static or temporary environment/shared-profile credentials",
            ));
        }
        let mut values = config;
        values.extend(read_section(
            &path("AWS_SHARED_CREDENTIALS_FILE", "credentials")
                .ok_or(Error("AWS profile location unavailable"))?,
            &profile,
        )?);
        Credentials {
            access: values
                .remove("aws_access_key_id")
                .ok_or(Error("AWS profile credentials unavailable"))?,
            secret: values
                .remove("aws_secret_access_key")
                .ok_or(Error("AWS profile credentials unavailable"))?,
            token: values.remove("aws_session_token"),
        }
    };
    if credentials.access.is_empty()
        || credentials.secret.is_empty()
        || credentials.token.as_ref().is_some_and(String::is_empty)
    {
        return Err(Error("incomplete AWS credentials"));
    }
    if [&credentials.access, &credentials.secret]
        .into_iter()
        .chain(credentials.token.as_ref())
        .any(|value| value.bytes().any(|b| !b.is_ascii_graphic()))
    {
        return Err(Error("invalid AWS credential characters"));
    }
    Ok((credentials, region))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_and_environment_precedence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "[profile gate]\nregion = us-west-1\naws_access_key_id=config-key\naws_secret_access_key=config-secret\n").unwrap();
        std::fs::write(dir.path().join("credentials"), "[gate]\naws_access_key_id=file-key\naws_secret_access_key=file-secret\naws_session_token=file-token\n").unwrap();
        let env = |key: &str| match key {
            "AWS_CONFIG_FILE" => Some(dir.path().join("config").to_string_lossy().into_owned()),
            "AWS_SHARED_CREDENTIALS_FILE" => Some(
                dir.path()
                    .join("credentials")
                    .to_string_lossy()
                    .into_owned(),
            ),
            "AWS_PROFILE" => Some("gate".into()),
            "AWS_ACCESS_KEY_ID" => Some("env-key".into()),
            "AWS_SECRET_ACCESS_KEY" => Some("env-secret".into()),
            "AWS_SESSION_TOKEN" => Some("env-token".into()),
            "AWS_DEFAULT_REGION" => Some("us-east-1".into()),
            _ => None,
        };
        let (creds, region) = load(None, Some("eu-west-1"), &env).unwrap();
        assert_eq!(creds.access, "env-key");
        assert_eq!(creds.token.as_deref(), Some("env-token"));
        assert_eq!(region, "eu-west-1");
        let (creds, region) = load(Some("gate"), None, &env).unwrap();
        assert_eq!(creds.access, "file-key");
        assert_eq!(creds.secret, "file-secret");
        assert_eq!(creds.token.as_deref(), Some("file-token"));
        assert_eq!(region, "us-east-1");
        let (_, region) = load(Some("gate"), None, &|key| {
            if key == "AWS_DEFAULT_REGION" {
                None
            } else {
                env(key)
            }
        })
        .unwrap();
        assert_eq!(region, "us-west-1");
    }

    #[test]
    fn missing_partial_and_unsupported_credentials_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let env = |key: &str| match key {
            "HOME" => Some(dir.path().to_string_lossy().into_owned()),
            _ => None,
        };
        assert!(load(None, Some("us-west-1"), &env).is_err());
        assert!(
            load(
                None,
                Some("us-west-1"),
                &|key| if key == "AWS_ACCESS_KEY_ID" {
                    Some("partial".into())
                } else {
                    env(key)
                }
            )
            .is_err()
        );
        std::fs::create_dir(dir.path().join(".aws")).unwrap();
        std::fs::write(dir.path().join(".aws/config"), "[default]\ncredential_process=never execute this\naws_access_key_id=stale\naws_secret_access_key=stale\n").unwrap();
        assert!(load(None, Some("us-west-1"), &env).is_err());
    }
}
