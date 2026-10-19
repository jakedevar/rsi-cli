//! hub #3044: operator-only input-bar assets, so the desktop never touches
//! the daemon host's files itself.
//!
//! `UploadPasteImage` stores a pasted image under a content-addressed name in
//! `RpcServer::paste_dir` (`~/.rsi/paste/rsid`) and `ListSlashCommands` lists
//! a project or session root's `.claude` commands and skills. `ListSkills`
//! and `GetSkill` (hub #3062) list the skills of a project or session root or
//! of the daemon user (`RpcServer::skill_home`) and read one `SKILL.md`,
//! read-only and contained in the root. `SetSkillEnabled` and `DeleteSkill`
//! (#97) are the TUI's Settings > Skills `e` and `d` on the daemon user's
//! skills: a rename to or from `<name>.disabled`, and removal of the skill
//! directory tree. None is in
//! the attributed verb registry, so a tokened caller is default-denied
//! (AGENTS.md rule 10), and the rsi-remote gateway never calls them. Wire
//! types and the shared discovery are `rsi_common::input_assets`.

use super::*;
use crate::project_files::refuse;
use base64::Engine as _;
use rsi_common::asset_fs::AssetDir;
use rsi_common::input_assets::{
    DEFAULT_SKILLS, DEFAULT_SLASH_COMMANDS, DeleteSkillRequestV1, GetSkillRequestV1,
    ListSkillsRequestV1, ListSlashCommandsRequestV1, MAX_PASTE_IMAGE_BYTES, MAX_SKILL_BYTES,
    SetSkillEnabledRequestV1, SkillScope, UploadPasteImageRequestV1, UploadPasteImageResponseV1,
    base64_len_bound, delete_skill, discover_slash_commands, list_skills, paste_image_extension,
    read_skill, set_skill_enabled, sniff_image_mime,
};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::Path;

pub(crate) const PASTE_INVALID_REQUEST: &str = "paste_image_invalid_request";
pub(crate) const PASTE_EMPTY: &str = "paste_image_empty";
pub(crate) const PASTE_TOO_LARGE: &str = "paste_image_too_large";
pub(crate) const PASTE_UNSUPPORTED_TYPE: &str = "paste_image_unsupported_type";
pub(crate) const PASTE_TYPE_MISMATCH: &str = "paste_image_type_mismatch";
pub(crate) const SLASH_INVALID_REQUEST: &str = "slash_commands_invalid_request";
pub(crate) const SKILL_INVALID_REQUEST: &str = "skill_invalid_request";

/// The default upload directory: `~/.rsi/paste/rsid` (the rsi home helper).
pub(crate) fn default_paste_dir() -> std::path::PathBuf {
    rsi_common::identity::data_path("paste", "paste")
        .join(rsi_common::input_assets::PASTE_UPLOAD_SUBDIR)
}

/// Decode params by reference: an image request is tens of megabytes, so the
/// params `Value` is never cloned.
fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest, code: &str) -> Result<T> {
    let empty = serde_json::json!({});
    let params = if request.params.is_null() {
        &empty
    } else {
        &request.params
    };
    T::deserialize(params).map_err(|e| refuse(code, e))
}

/// Validate and store one image in `dir` (created when missing).
///
/// The bytes must decode, be non-empty and at most `MAX_PASTE_IMAGE_BYTES`,
/// and their magic bytes must declare the allowlisted `mime`. The name is
/// `<sha256>.<ext>`, so nothing in the request chooses a path. An identical
/// regular file already there is reused; anything else at that name (a
/// symlink, a different file) is replaced by renaming a freshly written
/// temporary file over it, which never follows a link.
pub(crate) fn store_paste_image(
    dir: &Path,
    request: &UploadPasteImageRequestV1,
) -> Result<UploadPasteImageResponseV1> {
    store_paste_image_before_write(dir, request, || {})
}

fn store_paste_image_before_write(
    dir: &Path,
    request: &UploadPasteImageRequestV1,
    before_write: impl FnOnce(),
) -> Result<UploadPasteImageResponseV1> {
    let Some(ext) = paste_image_extension(&request.mime) else {
        return Err(refuse(
            PASTE_UNSUPPORTED_TYPE,
            format!("'{}' is not an allowed image type", request.mime.trim()),
        ));
    };
    if request.data_base64.len() > base64_len_bound(MAX_PASTE_IMAGE_BYTES) {
        return Err(refuse(
            PASTE_TOO_LARGE,
            format!("the image exceeds {MAX_PASTE_IMAGE_BYTES} bytes"),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(request.data_base64.as_bytes())
        .map_err(|e| refuse(PASTE_INVALID_REQUEST, format!("data_base64: {e}")))?;
    if bytes.is_empty() {
        return Err(refuse(PASTE_EMPTY, "the image has no bytes"));
    }
    if bytes.len() > MAX_PASTE_IMAGE_BYTES {
        return Err(refuse(
            PASTE_TOO_LARGE,
            format!("the image exceeds {MAX_PASTE_IMAGE_BYTES} bytes"),
        ));
    }
    let mime = request.mime.trim().to_ascii_lowercase();
    match sniff_image_mime(&bytes) {
        Some(sniffed) if sniffed == mime => {}
        Some(sniffed) => {
            return Err(refuse(
                PASTE_TYPE_MISMATCH,
                format!("declared {mime} but the bytes are {sniffed}"),
            ));
        }
        None => {
            return Err(refuse(
                PASTE_TYPE_MISMATCH,
                format!("declared {mime} but the bytes are not an allowed image"),
            ));
        }
    }
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let name = format!("{sha256}.{ext}");
    let io = |what: &str, e: std::io::Error| {
        DaemonError::Process(format!("paste image {what} {}: {e}", dir.display()))
    };
    let handle = create_upload_dir(dir).map_err(|e| io("directory", e))?;
    before_write();
    let path = dir.join(&name);
    let response = |reused| UploadPasteImageResponseV1 {
        path: path.to_string_lossy().into_owned(),
        name: name.clone(),
        mime: mime.clone(),
        bytes: bytes.len() as u64,
        sha256: sha256.clone(),
        reused,
    };
    if already_stored(&handle, Path::new(&name), &bytes) {
        return Ok(response(true));
    }
    let temp = std::path::PathBuf::from(format!(".upload-{}.tmp", Uuid::new_v4()));
    let written = write_new_private(&handle, &temp, &bytes)
        .and_then(|()| handle.rename(&temp, Path::new(&name)));
    if let Err(e) = written {
        let _ = handle.remove_file(&temp);
        return Err(io("write in", e));
    }
    Ok(response(false))
}

/// True when `path` is a regular file (not a link) holding exactly `bytes`.
fn already_stored(dir: &AssetDir, name: &Path, bytes: &[u8]) -> bool {
    use std::io::Read as _;
    let Ok(file) = dir.read_file(name) else {
        return false;
    };
    if !file
        .metadata()
        .is_ok_and(|meta| meta.len() == bytes.len() as u64)
    {
        return false;
    }
    let mut existing = Vec::new();
    file.take(bytes.len() as u64 + 1)
        .read_to_end(&mut existing)
        .is_ok_and(|_| existing == bytes)
}

fn create_upload_dir(dir: &Path) -> std::io::Result<AssetDir> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "upload directory needs a paste parent",
        )
    };
    let paste = dir.parent().ok_or_else(invalid)?;
    let data_root = paste.parent().ok_or_else(invalid)?;
    // Resolve the legitimate data/home root, then refuse links in both paste
    // components. All creation and writes stay relative to pinned handles.
    let root = AssetDir::create_data_root(data_root)?;
    let paste = root.create_dir(Path::new(paste.file_name().ok_or_else(invalid)?))?;
    paste.create_dir(Path::new(dir.file_name().ok_or_else(invalid)?))
}

/// Write `bytes` to a new file (`create_new`: an existing path or link fails).
fn write_new_private(dir: &AssetDir, name: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = dir.create_file(name)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn upload_keeps_pinned_directory_after_directory_or_parent_replacement() {
        for replace_parent in [false, true] {
            let fixture = tempfile::TempDir::new().unwrap();
            let paste = fixture.path().join("paste");
            let upload = paste.join("rsid");
            let outside = fixture.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            let bytes = b"\x89PNG\r\n\x1a\nfixture";
            let request = UploadPasteImageRequestV1 {
                mime: "image/png".into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            };
            let replaced = if replace_parent { &paste } else { &upload };
            let saved = fixture.path().join("saved");
            let response = store_paste_image_before_write(&upload, &request, || {
                std::fs::rename(replaced, &saved).unwrap();
                std::os::unix::fs::symlink(&outside, replaced).unwrap();
            })
            .unwrap();
            let stored_dir = if replace_parent {
                saved.join("rsid")
            } else {
                saved
            };
            assert_eq!(
                std::fs::read(stored_dir.join(&response.name)).unwrap(),
                bytes
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn upload_resolves_a_legitimate_linked_data_root() {
        let fixture = tempfile::TempDir::new().unwrap();
        let data = fixture.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let linked = fixture.path().join("linked-data");
        std::os::unix::fs::symlink(&data, &linked).unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        let request = UploadPasteImageRequestV1 {
            mime: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        let response = store_paste_image(&linked.join("paste/rsid"), &request).unwrap();
        assert_eq!(
            std::fs::read(data.join("paste/rsid").join(response.name)).unwrap(),
            bytes
        );
    }
}

impl RpcServer {
    pub(super) async fn handle_upload_paste_image(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UploadPasteImageRequestV1 = decode(request, PASTE_INVALID_REQUEST)?;
        let dir = self.paste_dir.clone();
        let stored = tokio::task::spawn_blocking(move || store_paste_image(&dir, &params))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))??;
        Ok(serde_json::to_value(stored)?)
    }

    pub(super) async fn handle_list_slash_commands(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListSlashCommandsRequestV1 = decode(request, SLASH_INVALID_REQUEST)?;
        let root = self
            .project_file_root(params.project_id, params.session_id)
            .await?;
        let max = params.max_entries.unwrap_or(DEFAULT_SLASH_COMMANDS);
        let listing = tokio::task::spawn_blocking(move || discover_slash_commands(&root, max))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))?
            .map_err(|e| refuse(crate::project_files::UNKNOWN_ROOT, e))?;
        Ok(serde_json::to_value(listing)?)
    }

    /// The scope a skills request names: `user: true` alone, else exactly one
    /// of `project_id` and `session_id` (resolved like the project-file RPCs).
    async fn skill_scope(
        &self,
        project_id: Option<Uuid>,
        session_id: Option<Uuid>,
        user: bool,
    ) -> Result<SkillScope> {
        if !user {
            return Ok(SkillScope::Root(
                self.project_file_root(project_id, session_id).await?,
            ));
        }
        if project_id.is_some() || session_id.is_some() {
            return Err(refuse(
                SKILL_INVALID_REQUEST,
                "name exactly one of project_id, session_id and user",
            ));
        }
        Ok(self.user_skill_scope())
    }

    pub(super) async fn handle_list_skills(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListSkillsRequestV1 = decode(request, SKILL_INVALID_REQUEST)?;
        let scope = self
            .skill_scope(params.project_id, params.session_id, params.user)
            .await?;
        let max = params.max_entries.unwrap_or(DEFAULT_SKILLS);
        let listing = tokio::task::spawn_blocking(move || list_skills(&scope, max))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))?
            .map_err(|e| refuse("skill_read_failed", e))?;
        Ok(serde_json::to_value(listing)?)
    }

    pub(super) async fn handle_get_skill(&self, request: &RpcRequest) -> Result<serde_json::Value> {
        let params: GetSkillRequestV1 = decode(request, SKILL_INVALID_REQUEST)?;
        let scope = self
            .skill_scope(params.project_id, params.session_id, params.user)
            .await?;
        let name = params.name;
        let skill = tokio::task::spawn_blocking(move || read_skill(&scope, &name, MAX_SKILL_BYTES))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))?
            .map_err(|e| refuse(e.code(), e))?;
        Ok(serde_json::to_value(skill)?)
    }

    /// The user skill tier `SetSkillEnabled` and `DeleteSkill` change.
    fn user_skill_scope(&self) -> SkillScope {
        SkillScope::User {
            home: self.skill_home.clone(),
        }
    }

    pub(super) async fn handle_set_skill_enabled(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: SetSkillEnabledRequestV1 = decode(request, SKILL_INVALID_REQUEST)?;
        let scope = self.user_skill_scope();
        let changed = tokio::task::spawn_blocking(move || {
            set_skill_enabled(&scope, &params.name, params.enabled)
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))?
        .map_err(|e| refuse(e.code(), e))?;
        Ok(serde_json::to_value(changed)?)
    }

    pub(super) async fn handle_delete_skill(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: DeleteSkillRequestV1 = decode(request, SKILL_INVALID_REQUEST)?;
        let scope = self.user_skill_scope();
        let deleted = tokio::task::spawn_blocking(move || delete_skill(&scope, &params.name))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))?
            .map_err(|e| refuse(e.code(), e))?;
        Ok(serde_json::to_value(deleted)?)
    }
}
