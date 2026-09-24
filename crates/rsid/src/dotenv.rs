use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// Result of the daemon-scoped dotenv load attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonDotenvStatus {
    Loaded(PathBuf),
    Missing(PathBuf),
    Failed { path: PathBuf, error: String },
}

/// Load the canonical daemon secrets file (`~/.rsi/.env`) without overriding
/// values already supplied by the parent process.
pub fn load_daemon_dotenv() -> DaemonDotenvStatus {
    load_daemon_dotenv_from_dir(&rsi_common::identity::data_dir())
}

fn load_daemon_dotenv_from_dir(data_dir: &Path) -> DaemonDotenvStatus {
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        let path = data_dir.join(".env");
        warn!(
            path = %path.display(),
            error = %e,
            "Failed to create RSI data directory before dotenv load"
        );
        return DaemonDotenvStatus::Failed {
            path,
            error: e.to_string(),
        };
    }

    load_daemon_dotenv_from_path(&data_dir.join(".env"))
}

fn load_daemon_dotenv_from_path(path: &Path) -> DaemonDotenvStatus {
    if !path.exists() {
        return DaemonDotenvStatus::Missing(path.to_path_buf());
    }

    match dotenvy::from_path(path) {
        Ok(_) => {
            info!(path = %path.display(), "Loaded daemon dotenv file");
            DaemonDotenvStatus::Loaded(path.to_path_buf())
        }
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Failed to load daemon dotenv file"
            );
            DaemonDotenvStatus::Failed {
                path: path.to_path_buf(),
                error: e.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_dotenv_sets_missing_values_without_overriding_process_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "RSI_DOTENV_TEST_FROM_FILE=file-value\nRSI_DOTENV_TEST_KEEP=file-value\n",
        )
        .expect("write dotenv");

        temp_env::with_vars(
            [
                ("RSI_DOTENV_TEST_FROM_FILE", None),
                ("RSI_DOTENV_TEST_KEEP", Some("process-value")),
            ],
            || {
                assert_eq!(
                    load_daemon_dotenv_from_dir(dir.path()),
                    DaemonDotenvStatus::Loaded(path)
                );
                assert_eq!(
                    std::env::var("RSI_DOTENV_TEST_FROM_FILE").as_deref(),
                    Ok("file-value")
                );
                assert_eq!(
                    std::env::var("RSI_DOTENV_TEST_KEEP").as_deref(),
                    Ok("process-value")
                );
            },
        );
    }

    #[test]
    fn missing_dotenv_is_non_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".env");

        assert_eq!(
            load_daemon_dotenv_from_dir(dir.path()),
            DaemonDotenvStatus::Missing(path)
        );
    }

    #[test]
    fn malformed_dotenv_is_warning_only_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "NOT A VALID LINE\n").expect("write dotenv");

        assert!(matches!(
            load_daemon_dotenv_from_dir(dir.path()),
            DaemonDotenvStatus::Failed { .. }
        ));
    }

    #[test]
    fn load_dotenv_handles_comments_export_and_quotes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            r#"
# comment
RSI_DOTENV_TEST_A=one
export RSI_DOTENV_TEST_B=two
RSI_DOTENV_TEST_C="three # not comment"
RSI_DOTENV_TEST_D='four'
"#,
        )
        .expect("write dotenv");

        temp_env::with_vars(
            [
                ("RSI_DOTENV_TEST_A", None::<&str>),
                ("RSI_DOTENV_TEST_B", None::<&str>),
                ("RSI_DOTENV_TEST_C", None::<&str>),
                ("RSI_DOTENV_TEST_D", None::<&str>),
            ],
            || {
                assert_eq!(
                    load_daemon_dotenv_from_dir(dir.path()),
                    DaemonDotenvStatus::Loaded(path)
                );
                assert_eq!(std::env::var("RSI_DOTENV_TEST_A").as_deref(), Ok("one"));
                assert_eq!(std::env::var("RSI_DOTENV_TEST_B").as_deref(), Ok("two"));
                assert_eq!(
                    std::env::var("RSI_DOTENV_TEST_C").as_deref(),
                    Ok("three # not comment")
                );
                assert_eq!(std::env::var("RSI_DOTENV_TEST_D").as_deref(), Ok("four"));
            },
        );
    }
}
