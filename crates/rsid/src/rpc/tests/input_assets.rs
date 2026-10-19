//! hub #3044: `UploadPasteImage` stores a verified image under a
//! content-addressed name in the paste directory and nowhere else;
//! `ListSlashCommands` lists a root's commands and skills without following a
//! link out of it; `ListSkills`/`GetSkill` (hub #3062) list and read skills
//! of a root or the daemon user and refuse a path or symlink escape;
//! `SetSkillEnabled`/`DeleteSkill` (#97) rename and remove a user skill like
//! the TUI and refuse links and non-directories; all stay operator-only and
//! off the rsi-remote gateway.
use super::*;
use base64::Engine as _;
use rsi_common::input_assets::{INPUT_ASSET_METHODS, MAX_PASTE_IMAGE_BYTES, base64_len_bound};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-test-image-bytes";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn refusal(response: &RpcResponse) -> (i32, String) {
    let error = response.error.as_ref().expect("refusal");
    (error.code, error.message.clone())
}

fn dir_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn upload_paste_image_stores_a_content_addressed_copy_once() {
    let mut fixture = recursive_dag_rpc_fixture();
    let paste = fixture._dir.path().join("home/.rsi/paste/rsid");
    fixture.server.paste_dir = paste.clone();

    let first = call_rpc(
        &fixture.server,
        "UploadPasteImage",
        serde_json::json!({"mime": "IMAGE/PNG", "data_base64": b64(PNG)}),
    )
    .await
    .result
    .expect("upload");
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(PNG));
    let name = format!("{sha}.png");
    assert_eq!(first["name"], name.as_str());
    assert_eq!(first["sha256"], sha.as_str());
    assert_eq!(first["mime"], "image/png");
    assert_eq!(first["bytes"], PNG.len() as u64);
    assert_eq!(first["reused"], false);
    assert_eq!(first["path"], paste.join(&name).to_string_lossy().as_ref());
    assert_eq!(std::fs::read(paste.join(&name)).unwrap(), PNG);
    assert_eq!(dir_names(&paste), vec![name.clone()]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(paste.join(&name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let again = call_rpc(
        &fixture.server,
        "UploadPasteImage",
        serde_json::json!({"mime": "image/png", "data_base64": b64(PNG)}),
    )
    .await
    .result
    .expect("upload again");
    assert_eq!(again["reused"], true);
    assert_eq!(again["path"], first["path"]);
    assert_eq!(dir_names(&paste), vec![name]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn upload_paste_image_refuses_bad_types_sizes_and_fields_and_writes_nothing() {
    let mut fixture = recursive_dag_rpc_fixture();
    let paste = fixture._dir.path().join("paste");
    fixture.server.paste_dir = paste.clone();

    let mut oversize = PNG.to_vec();
    oversize.resize(MAX_PASTE_IMAGE_BYTES + 1, 0);
    let oversize = b64(&oversize);
    assert_eq!(oversize.len(), base64_len_bound(MAX_PASTE_IMAGE_BYTES));
    let overlong = "A".repeat(base64_len_bound(MAX_PASTE_IMAGE_BYTES) + 4);
    let cases = [
        (
            serde_json::json!({"mime": "image/svg+xml", "data_base64": b64(b"<svg/>")}),
            "paste_image_unsupported_type",
        ),
        (
            serde_json::json!({"mime": "../../etc/passwd", "data_base64": b64(PNG)}),
            "paste_image_unsupported_type",
        ),
        (
            serde_json::json!({"mime": "image/jpeg", "data_base64": b64(PNG)}),
            "paste_image_type_mismatch",
        ),
        (
            serde_json::json!({"mime": "image/png", "data_base64": b64(b"#!/bin/sh\n")}),
            "paste_image_type_mismatch",
        ),
        (
            serde_json::json!({"mime": "image/png", "data_base64": ""}),
            "paste_image_empty",
        ),
        (
            serde_json::json!({"mime": "image/png", "data_base64": "not base64!"}),
            "paste_image_invalid_request",
        ),
        (
            serde_json::json!({"mime": "image/png", "data_base64": oversize}),
            "paste_image_too_large",
        ),
        (
            serde_json::json!({"mime": "image/png", "data_base64": overlong}),
            "paste_image_too_large",
        ),
        (
            serde_json::json!({"mime": "image/png", "data_base64": b64(PNG), "name": "x.png"}),
            "paste_image_invalid_request",
        ),
        (serde_json::Value::Null, "paste_image_invalid_request"),
    ];
    for (params, code) in cases {
        let response = call_rpc(&fixture.server, "UploadPasteImage", params).await;
        let (rpc_code, message) = refusal(&response);
        assert_eq!(rpc_code, INVALID_PARAMS, "{code}");
        assert!(message.contains(code), "{code}: {message}");
    }
    assert!(!paste.exists(), "a refused upload creates nothing");
}

/// A link already at the content-addressed name is replaced, never followed.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn upload_paste_image_replaces_a_planted_link_without_following_it() {
    let mut fixture = recursive_dag_rpc_fixture();
    let paste = fixture._dir.path().join("paste");
    std::fs::create_dir_all(&paste).unwrap();
    fixture.server.paste_dir = paste.clone();
    let victim = fixture._dir.path().join("victim.txt");
    std::fs::write(&victim, "keep").unwrap();
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(PNG));
    let target = paste.join(format!("{sha}.png"));
    std::os::unix::fs::symlink(&victim, &target).unwrap();

    let stored = call_rpc(
        &fixture.server,
        "UploadPasteImage",
        serde_json::json!({"mime": "image/png", "data_base64": b64(PNG)}),
    )
    .await
    .result
    .expect("upload");
    assert_eq!(stored["reused"], false);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    assert!(
        std::fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(std::fs::read(&target).unwrap(), PNG);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn upload_paste_image_refuses_escaping_directory_and_parent_links() {
    for linked_parent in [false, true] {
        let mut fixture = recursive_dag_rpc_fixture();
        let base = fixture._dir.path().join("home/.rsi");
        let paste = base.join("paste");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        if linked_parent {
            std::os::unix::fs::symlink(&outside, &paste).unwrap();
        } else {
            std::fs::create_dir(&paste).unwrap();
            std::os::unix::fs::symlink(&outside, paste.join("rsid")).unwrap();
        }
        fixture.server.paste_dir = paste.join("rsid");
        let response = call_rpc(
            &fixture.server,
            "UploadPasteImage",
            serde_json::json!({"mime": "image/png", "data_base64": b64(PNG)}),
        )
        .await;
        assert!(response.error.is_some());
        assert_eq!(dir_names(&outside), Vec::<String>::new());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_slash_commands_serves_project_and_session_roots() {
    let fixture = recursive_dag_rpc_fixture();
    let dir = tempfile::TempDir::new().unwrap();
    let commands = dir.path().join(".claude/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(commands.join("plan.md"), "---\ndescription: Plan it\n---\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".claude/skills/debug")).unwrap();
    std::fs::write(dir.path().join(".claude/skills/debug/SKILL.md"), "").unwrap();
    let project = Uuid::new_v4();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_project(&rsi_common::types::Project {
            id: project,
            name: format!("slash-{project}"),
            path: Some(dir.path().to_path_buf()),
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    let mut session = make_recursive_dag_test_session(Uuid::new_v4());
    session.working_dir = dir.path().to_path_buf();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .unwrap();

    let expected = serde_json::json!([
        {"name": "debug", "description": "Skill from .claude/skills/", "source": "skill"},
        {"name": "plan", "description": "Plan it", "source": "command"},
    ]);
    for params in [
        serde_json::json!({"project_id": project}),
        serde_json::json!({"session_id": session.id}),
    ] {
        let listing = call_rpc(&fixture.server, "ListSlashCommands", params.clone())
            .await
            .result
            .expect("listing");
        assert_eq!(listing["commands"], expected, "{params}");
        assert_eq!(
            listing["root"],
            dir.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(listing["truncated"], false);
        assert_eq!(listing["skipped_outside_root"], 0);
    }
    let one = call_rpc(
        &fixture.server,
        "ListSlashCommands",
        serde_json::json!({"project_id": project, "max_entries": 1}),
    )
    .await
    .result
    .expect("bounded listing");
    assert_eq!(one["commands"].as_array().unwrap().len(), 1);
    assert_eq!(one["truncated"], true);

    for (params, code) in [
        (
            serde_json::json!({"project_id": Uuid::new_v4()}),
            "project_file_unknown_root",
        ),
        (
            serde_json::json!({"project_id": project, "session_id": session.id}),
            "project_file_invalid_request",
        ),
        (serde_json::json!({}), "project_file_invalid_request"),
        (
            serde_json::json!({"working_dir": "/"}),
            "slash_commands_invalid_request",
        ),
    ] {
        let response = call_rpc(&fixture.server, "ListSlashCommands", params.clone()).await;
        let (rpc_code, message) = refusal(&response);
        assert_eq!(rpc_code, INVALID_PARAMS, "{params}");
        assert!(message.contains(code), "{params}: {message}");
    }
}

/// A command or skill linked from outside the root is skipped and counted.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn list_slash_commands_skips_symlink_escapes() {
    let fixture = recursive_dag_rpc_fixture();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(
        outside.path().join("secret.md"),
        "---\ndescription: leaked\n---\n",
    )
    .unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let commands = dir.path().join(".claude/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(commands.join("ok.md"), "").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.md"), commands.join("secret.md"))
        .unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".claude/skills")).unwrap();
    let mut session = make_recursive_dag_test_session(Uuid::new_v4());
    session.working_dir = dir.path().to_path_buf();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .unwrap();

    let listing = call_rpc(
        &fixture.server,
        "ListSlashCommands",
        serde_json::json!({"session_id": session.id}),
    )
    .await
    .result
    .expect("listing");
    assert_eq!(
        listing["commands"],
        serde_json::json!([{"name": "ok", "description": "Custom command from .claude/commands/", "source": "command"}])
    );
    assert_eq!(listing["skipped_outside_root"], 2);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
fn write_skill(skills: &std::path::Path, dir_name: &str, body: &str) {
    std::fs::create_dir_all(skills.join(dir_name)).unwrap();
    std::fs::write(skills.join(dir_name).join("SKILL.md"), body).unwrap();
}

/// A project, session or user skill reads back with its `SKILL.md`; a name
/// that is not one path component and a bad scope are refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn get_skill_reads_project_session_and_user_skills() {
    let mut fixture = recursive_dag_rpc_fixture();
    let dir = tempfile::TempDir::new().unwrap();
    let skills = dir.path().join(".claude/skills");
    write_skill(
        &skills,
        "debug",
        "---\ndescription: Debug it\n---\n\nSteps.\n",
    );
    write_skill(&skills, "parked.disabled", "Later.\n");
    let project = Uuid::new_v4();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_project(&rsi_common::types::Project {
            id: project,
            name: format!("skills-{project}"),
            path: Some(dir.path().to_path_buf()),
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    let mut session = make_recursive_dag_test_session(Uuid::new_v4());
    session.working_dir = dir.path().to_path_buf();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .unwrap();
    let home = fixture._dir.path().join("home");
    write_skill(&home.join(".claude/skills"), "mine", "Mine.\n");
    fixture.server.skill_home = home.clone();

    let md = skills.canonicalize().unwrap().join("debug/SKILL.md");
    for scope in [
        serde_json::json!({"project_id": project}),
        serde_json::json!({"session_id": session.id}),
    ] {
        let mut params = scope.clone();
        params["name"] = "debug".into();
        let skill = call_rpc(&fixture.server, "GetSkill", params)
            .await
            .result
            .expect("skill");
        assert_eq!(
            skill,
            serde_json::json!({
                "name": "debug",
                "enabled": true,
                "path": md.to_string_lossy(),
                "content": "---\ndescription: Debug it\n---\n\nSteps.\n",
                "bytes": 38,
                "truncated": false,
            }),
            "{scope}"
        );
        let listing = call_rpc(&fixture.server, "ListSkills", scope.clone())
            .await
            .result
            .expect("listing");
        assert_eq!(
            listing["skills"],
            serde_json::json!([
                {"name": "debug", "enabled": true, "description": "Debug it", "has_skill_md": true},
                {"name": "parked", "enabled": false, "description": null, "has_skill_md": true},
            ]),
            "{scope}"
        );
    }
    let parked = call_rpc(
        &fixture.server,
        "GetSkill",
        serde_json::json!({"project_id": project, "name": "parked"}),
    )
    .await
    .result
    .expect("disabled skill");
    assert_eq!(parked["content"], "Later.\n");
    assert_eq!(parked["enabled"], false);
    let mine = call_rpc(
        &fixture.server,
        "GetSkill",
        serde_json::json!({"user": true, "name": "mine"}),
    )
    .await
    .result
    .expect("user skill");
    assert_eq!(mine["content"], "Mine.\n");
    let user_listing = call_rpc(
        &fixture.server,
        "ListSkills",
        serde_json::json!({"user": true}),
    )
    .await
    .result
    .expect("user listing");
    assert_eq!(user_listing["skills"][0]["name"], "mine");

    for (method, params, code) in [
        (
            "GetSkill",
            serde_json::json!({"project_id": project, "name": "../debug"}),
            "skill_invalid_name",
        ),
        (
            "GetSkill",
            serde_json::json!({"project_id": project, "name": "debug/SKILL.md"}),
            "skill_invalid_name",
        ),
        (
            "GetSkill",
            serde_json::json!({"project_id": project, "name": ".."}),
            "skill_invalid_name",
        ),
        (
            "GetSkill",
            serde_json::json!({"project_id": project, "name": "absent"}),
            "skill_not_found",
        ),
        (
            "GetSkill",
            serde_json::json!({"user": true, "project_id": project, "name": "debug"}),
            "skill_invalid_request",
        ),
        (
            "GetSkill",
            serde_json::json!({"name": "debug"}),
            "project_file_invalid_request",
        ),
        (
            "GetSkill",
            serde_json::json!({"project_id": project}),
            "skill_invalid_request",
        ),
        (
            "ListSkills",
            serde_json::json!({"project_id": Uuid::new_v4()}),
            "project_file_unknown_root",
        ),
        (
            "ListSkills",
            serde_json::json!({"user": true, "path": "/"}),
            "skill_invalid_request",
        ),
    ] {
        let response = call_rpc(&fixture.server, method, params.clone()).await;
        let (rpc_code, message) = refusal(&response);
        assert_eq!(rpc_code, INVALID_PARAMS, "{method} {params}");
        assert!(message.contains(code), "{method} {params}: {message}");
    }
}

/// A skill directory or `SKILL.md` linked out of the root is refused by
/// `GetSkill` and skipped by `ListSkills`.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn get_skill_refuses_symlink_escapes() {
    let fixture = recursive_dag_rpc_fixture();
    let outside = tempfile::TempDir::new().unwrap();
    write_skill(outside.path(), "secret", "leaked\n");
    let dir = tempfile::TempDir::new().unwrap();
    let skills = dir.path().join(".claude/skills");
    write_skill(&skills, "ok", "fine\n");
    std::os::unix::fs::symlink(outside.path().join("secret"), skills.join("dirlink")).unwrap();
    std::fs::create_dir_all(skills.join("filelink")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret/SKILL.md"),
        skills.join("filelink/SKILL.md"),
    )
    .unwrap();
    let mut session = make_recursive_dag_test_session(Uuid::new_v4());
    session.working_dir = dir.path().to_path_buf();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&session)
        .unwrap();

    for name in ["dirlink", "filelink"] {
        let response = call_rpc(
            &fixture.server,
            "GetSkill",
            serde_json::json!({"session_id": session.id, "name": name}),
        )
        .await;
        let (code, message) = refusal(&response);
        assert_eq!(code, INVALID_PARAMS, "{name}");
        assert!(message.contains("skill_outside_root"), "{name}: {message}");
        assert!(!message.contains("leaked"), "{name}: {message}");
    }
    let listing = call_rpc(
        &fixture.server,
        "ListSkills",
        serde_json::json!({"session_id": session.id}),
    )
    .await
    .result
    .expect("listing");
    assert_eq!(
        listing["skills"],
        serde_json::json!([{"name": "ok", "enabled": true, "description": null, "has_skill_md": true}])
    );
    assert_eq!(listing["skipped_outside_root"], 2);
}

/// The operator-only fence: in no attributed audience, refused to a tokened
/// caller, and outside the `Remote*` family the rsi-remote gateway speaks.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn input_asset_rpcs_are_operator_only_and_off_the_remote_gateway() {
    let fixture = recursive_dag_rpc_fixture();
    assert_eq!(
        INPUT_ASSET_METHODS,
        &[
            "UploadPasteImage",
            "ListSlashCommands",
            "ListSkills",
            "GetSkill",
            "SetSkillEnabled",
            "DeleteSkill"
        ]
    );
    let cli = rsi_common::rpc_verb_registry::cli_verb_methods();
    for method in INPUT_ASSET_METHODS {
        assert!(
            !agent_gate::is_allowed_for_attributed_caller(method),
            "{method}"
        );
        assert!(!agent_gate::READ_VERBS.contains(method), "{method}");
        assert!(!agent_gate::AGENT_VERBS.contains(method), "{method}");
        assert_eq!(
            rsi_common::rpc_verb_registry::audience_of(method),
            None,
            "{method}"
        );
        assert!(!cli.contains(method), "{method}");
        assert!(
            !rsi_common::manager_operator_delegation::OPERATOR_DELEGATION_METHODS.contains(method),
            "{method}"
        );
        assert!(!method.starts_with("Remote"), "{method}");
        let mut request = RpcRequest::new(*method, serde_json::json!({}));
        request.session_token = Some("test-token".into());
        let HandleResult::Response(response) = fixture.server.handle_request_inner(&request).await
        else {
            panic!("expected response");
        };
        let (code, message) = refusal(&response);
        assert_eq!(code, INVALID_PARAMS, "{method}");
        assert!(
            message.contains("not available to session-attributed callers"),
            "{method}"
        );
    }
}

/// `SetSkillEnabled` and `DeleteSkill` change only the daemon user's
/// `~/.claude/skills`, with the TUI's rename and delete semantics, and refuse
/// a link, a non-directory entry, a path name and a scope field.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn skill_writes_toggle_and_delete_user_skills_only() {
    let mut fixture = recursive_dag_rpc_fixture();
    let home = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    write_skill(outside.path(), "secret", "kept\n");
    let skills = home.path().join(".claude/skills");
    write_skill(&skills, "demo", "Demo.\n");
    std::os::unix::fs::symlink(outside.path().join("secret"), skills.join("away")).unwrap();
    fixture.server.skill_home = home.path().to_path_buf();
    let canonical = skills.canonicalize().unwrap();

    let off = call_rpc(
        &fixture.server,
        "SetSkillEnabled",
        serde_json::json!({"name": "demo", "enabled": false}),
    )
    .await
    .result
    .expect("disable");
    assert_eq!(
        off,
        serde_json::json!({
            "name": "demo",
            "enabled": false,
            "changed": true,
            "path": canonical.join("demo.disabled").to_string_lossy(),
        })
    );
    assert_eq!(dir_names(&skills), vec!["away", "demo.disabled"]);
    let listing = call_rpc(
        &fixture.server,
        "ListSkills",
        serde_json::json!({"user": true}),
    )
    .await
    .result
    .expect("listing");
    assert_eq!(
        listing["skills"],
        serde_json::json!([{"name": "demo", "enabled": false, "description": null, "has_skill_md": true}])
    );
    let on = call_rpc(
        &fixture.server,
        "SetSkillEnabled",
        serde_json::json!({"name": "demo", "enabled": true}),
    )
    .await
    .result
    .expect("enable");
    assert_eq!(on["changed"], true);
    assert_eq!(dir_names(&skills), vec!["away", "demo"]);

    for (method, params, code) in [
        (
            "SetSkillEnabled",
            serde_json::json!({"name": "away", "enabled": false}),
            "skill_not_directory",
        ),
        (
            "DeleteSkill",
            serde_json::json!({"name": "away"}),
            "skill_not_directory",
        ),
        (
            "DeleteSkill",
            serde_json::json!({"name": "../demo"}),
            "skill_invalid_name",
        ),
        (
            "DeleteSkill",
            serde_json::json!({"name": "nope"}),
            "skill_not_found",
        ),
        (
            "DeleteSkill",
            serde_json::json!({"name": "demo", "project_id": Uuid::new_v4()}),
            "skill_invalid_request",
        ),
        (
            "SetSkillEnabled",
            serde_json::json!({"name": "demo"}),
            "skill_invalid_request",
        ),
    ] {
        let response = call_rpc(&fixture.server, method, params.clone()).await;
        let (rpc_code, message) = refusal(&response);
        assert_eq!(rpc_code, INVALID_PARAMS, "{method} {params}");
        assert!(message.contains(code), "{method} {params}: {message}");
    }
    assert_eq!(dir_names(&skills), vec!["away", "demo"]);

    let deleted = call_rpc(
        &fixture.server,
        "DeleteSkill",
        serde_json::json!({"name": "demo"}),
    )
    .await
    .result
    .expect("delete");
    assert_eq!(
        deleted,
        serde_json::json!({
            "name": "demo",
            "enabled": true,
            "path": canonical.join("demo").to_string_lossy(),
        })
    );
    assert_eq!(dir_names(&skills), vec!["away"]);
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret/SKILL.md")).unwrap(),
        "kept\n"
    );
}
