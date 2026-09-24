use super::*;
use crate::error::Result;

#[tokio::test]
async fn ownership_expansion_survives_work_row_advance_and_keeps_ownership_cas() -> Result<()> {
    let f = fixture().await;
    let initial_files = vec!["crates/rsid/src/store/a.rs".to_string()];
    let initial = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Ownership {
                    key: "product".into(),
                    expected_row_version: 0,
                    domain: "store-ledger".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: initial_files,
                    active: true,
                },
                "ownership-initial",
            ),
        )
        .await?;
    assert_eq!(initial.row_version, 1);

    // Simulate implementation progress advancing the work row while ownership
    // is being adjusted. The ownership row has its own independent CAS.
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "Implementation expanded the known file set".into(),
                    evidence: None,
                },
                "implementation-progress",
            ),
        )
        .await?;

    let expanded_files = vec![
        "crates/rsid/src/store/a.rs".to_string(),
        "crates/rsid/src/store/b.rs".to_string(),
        "crates/rsid/src/store/c.rs".to_string(),
    ];
    let expansion = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Ownership {
                    key: "product".into(),
                    expected_row_version: 1,
                    domain: "store-ledger".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: expanded_files.clone(),
                    active: true,
                },
                "ownership-expanded",
            ),
        )
        .await?;
    assert_eq!(expansion.row_version, 2);

    let stale = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Ownership {
                    key: "product".into(),
                    expected_row_version: 1,
                    domain: "store-ledger".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: expanded_files.clone(),
                    active: true,
                },
                "ownership-stale-version",
            ),
        )
        .await
        .err();
    let Some(stale) = stale else {
        panic!("stale ownership row version should be refused");
    };
    assert!(stale.to_string().contains("manager_v2_record_changed"));

    let store = f.handle.store.lock().await;
    let (config, _) = store.manager_config_for_caller(f.manager)?;
    let ownership = store
        .manager_v2_records(&config, "ownership")?
        .into_iter()
        .find(|record| {
            record.payload["work_key"] == "product" && record.payload["domain"] == "store-ledger"
        })
        .unwrap_or_else(|| panic!("ownership record should persist"));
    assert_eq!(ownership.row_version, 2);
    assert_eq!(ownership.payload["files"], json!(expanded_files));
    drop(store);
    drop(f);
    Ok(())
}

#[tokio::test]
async fn ownership_expansion_still_refuses_an_exclusive_domain_conflict() -> Result<()> {
    let f = fixture().await;
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Ownership {
                    key: "product".into(),
                    expected_row_version: 0,
                    domain: "store-ledger".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: vec!["crates/rsid/src/store/a.rs".into()],
                    active: true,
                },
                "ownership-domain-owner",
            ),
        )
        .await?;
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Work {
                    key: "other-work".into(),
                    expected_row_version: 0,
                    epic_id: f.epic,
                    title: "Other work".into(),
                    kind: ManagerWorkKindV2::Product,
                    priority: 2,
                    weight: 1,
                    required_gates: vec![
                        ManagerWorkStageV2::Implementation,
                        ManagerWorkStageV2::Review,
                        ManagerWorkStageV2::Verification,
                    ],
                },
                "other-work-plan",
            ),
        )
        .await?;

    let conflict = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Ownership {
                    key: "other-work".into(),
                    expected_row_version: 0,
                    domain: "store-ledger".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: vec!["crates/rsid/src/store/b.rs".into()],
                    active: true,
                },
                "ownership-domain-conflict",
            ),
        )
        .await
        .err();
    let Some(conflict) = conflict else {
        panic!("a different work key cannot claim the active exclusive domain");
    };
    assert!(conflict.to_string().contains("manager_v2_domain_conflict"));
    drop(f);
    Ok(())
}

#[tokio::test]
async fn evidence_stage_still_refuses_when_source_head_moves() -> Result<()> {
    let f = fixture().await;
    std::fs::write(f.source_root.join("code.txt"), "advanced source\n")?;
    command(&f.source_root, &["commit", "-am", "advance source"]);

    let moved = f
        .handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Passed,
                    note: "Evidence names the earlier source".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: f.source,
                        source_commit: f.source_head.clone(),
                        artifact_path: String::new(),
                        artifact_commit: f.source_head.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "stage-with-moved-source",
            ),
        )
        .await
        .err();
    let Some(moved) = moved else {
        panic!("evidence tied to an earlier source head should be refused");
    };
    assert!(moved.to_string().contains("manager_v2_source_changed"));
    drop(f);
    Ok(())
}

#[test]
fn source_bound_update_classification_is_explicit() {
    let evidence = ManagerEvidenceV2 {
        source_session_id: Uuid::new_v4(),
        source_commit: "a".repeat(40),
        artifact_path: String::new(),
        artifact_commit: "a".repeat(40),
        closure_evidence_id: None,
    };
    let bookkeeping = ManagerUpdateV2::Ownership {
        key: "work".into(),
        expected_row_version: 1,
        domain: "store-ledger".into(),
        mode: ManagerOwnershipModeV2::Exclusive,
        files: vec!["crates/rsid/src/store/a.rs".into()],
        active: true,
    };
    let source_bound = ManagerUpdateV2::Stage {
        key: "work".into(),
        expected_row_version: 1,
        stage: ManagerWorkStageV2::Implementation,
        state: ManagerStageStateV2::Partial,
        note: String::new(),
        evidence: Some(evidence.clone()),
    };
    assert!(!super::super::manager_update_work_snapshot_changed(
        &bookkeeping,
        4,
        5,
    ));
    assert!(super::super::manager_update_work_snapshot_changed(
        &source_bound,
        4,
        5,
    ));
    assert!(!super::super::manager_update_work_snapshot_changed(
        &source_bound,
        5,
        5,
    ));
    assert!(!super::super::requires_work_version_match(
        &ManagerUpdateV2::Stage {
            key: "work".into(),
            expected_row_version: 1,
            stage: ManagerWorkStageV2::Implementation,
            state: ManagerStageStateV2::Partial,
            note: String::new(),
            evidence: None,
        }
    ));
    assert!(super::super::requires_work_version_match(
        &ManagerUpdateV2::Stage {
            key: "work".into(),
            expected_row_version: 1,
            stage: ManagerWorkStageV2::Implementation,
            state: ManagerStageStateV2::Partial,
            note: String::new(),
            evidence: Some(evidence),
        }
    ));
    assert!(super::super::requires_work_version_match(
        &ManagerUpdateV2::Migration {
            key: "work".into(),
            expected_row_version: 1,
            version: 999,
            baseline_commit: "a".repeat(40),
            inventory_digest: "sha256:test".into(),
        }
    ));
    assert!(super::super::requires_work_version_match(
        &ManagerUpdateV2::Integration {
            key: "work".into(),
            expected_row_version: 1,
            source_commit: "a".repeat(40),
            target_commit: "b".repeat(40),
            verification: None,
        }
    ));
    assert!(super::super::requires_work_version_match(
        &ManagerUpdateV2::RequestReview {
            key: "work".into(),
            expected_row_version: 1,
            source_commit: "a".repeat(40),
            query: "Review exact source".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: SessionProvider::Claude,
                model: "test".into(),
                effort: None,
            },
        }
    ));
}

#[tokio::test]
async fn ownership_update_commits_across_concurrent_work_row_advance() -> Result<()> {
    let f = fixture().await;
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Ownership {
                    key: "product".into(),
                    expected_row_version: 0,
                    domain: "store-ledger".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: vec!["crates/rsid/src/store/a.rs".into()],
                    active: true,
                },
                "ownership-initial",
            ),
        )
        .await?;
    let key = "ownership-race";
    let (reached, release) = super::super::install_manager_update_test_pause(key);
    let handle = f.handle.clone();
    let paused = tokio::spawn(async move {
        handle
            .agent_manager_update(
                f.manager,
                req(
                    ManagerUpdateV2::Ownership {
                        key: "product".into(),
                        expected_row_version: 1,
                        domain: "store-ledger".into(),
                        mode: ManagerOwnershipModeV2::Exclusive,
                        files: vec!["crates/rsid/src/store/b.rs".into()],
                        active: true,
                    },
                    "ownership-race",
                ),
            )
            .await
    });
    reached.await.unwrap();
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "work advanced".into(),
                    evidence: None,
                },
                "stage-advance",
            ),
        )
        .await?;
    let _ = release.send(());
    paused.await.unwrap()?;
    Ok(())
}

#[tokio::test]
async fn source_bound_stage_update_rejects_across_concurrent_work_row_advance() -> Result<()> {
    let f = fixture().await;
    let key = "source-bound-race";
    let (reached, release) = super::super::install_manager_update_test_pause(key);
    let handle = f.handle.clone();
    let paused = tokio::spawn(async move {
        handle
            .agent_manager_update(
                f.manager,
                req(
                    ManagerUpdateV2::Stage {
                        key: "product".into(),
                        expected_row_version: 1,
                        stage: ManagerWorkStageV2::Implementation,
                        state: ManagerStageStateV2::Partial,
                        note: "source-bound stage".into(),
                        evidence: Some(ManagerEvidenceV2 {
                            source_session_id: f.source,
                            source_commit: f.source_head.clone(),
                            artifact_path: String::new(),
                            artifact_commit: f.source_head.clone(),
                            closure_evidence_id: None,
                        }),
                    },
                    "source-bound-race",
                ),
            )
            .await
    });
    reached.await.unwrap();
    f.handle
        .agent_manager_update(
            f.manager,
            req(
                ManagerUpdateV2::Stage {
                    key: "product".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Partial,
                    note: "work advanced".into(),
                    evidence: None,
                },
                "stage-advance",
            ),
        )
        .await?;
    let _ = release.send(());
    let result = paused.await.unwrap();
    assert!(
        result.is_err(),
        "source-bound update must reject when the work row advanced"
    );
    Ok(())
}
