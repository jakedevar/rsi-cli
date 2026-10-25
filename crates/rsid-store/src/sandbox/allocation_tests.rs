use super::{AllocationPermit, effective_min_free_bytes};
use tempfile::tempdir;
use uuid::Uuid;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn allocation_permit_measures_only_uuid_named_direct_entries_from_exact_base() {
    let fixture = tempdir().unwrap();
    let base = fixture.path().join("sandboxes");
    std::fs::create_dir(&base).unwrap();

    let direct_root = base.join(Uuid::new_v4().to_string());
    std::fs::create_dir(&direct_root).unwrap();
    std::fs::write(base.join("not-a-root"), "ignored").unwrap();
    std::fs::create_dir_all(direct_root.join(Uuid::new_v4().to_string())).unwrap();

    let alias = fixture.path().join("sandbox-alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&base, &alias).unwrap();
    let error = AllocationPermit::acquire(&alias)
        .await
        .err()
        .expect("an alias must not establish a permit for a noncanonical base");
    assert!(error.to_string().contains("exact canonical directory"));

    let permit = AllocationPermit::acquire(&base).await.unwrap();
    let capacity = permit.measure().unwrap();
    assert_eq!(capacity.source_roots, 1);
    assert!(capacity.available_bytes > 0);

    // The permit is bound to the directory identity as well as its pathname.
    let displaced = fixture.path().join("displaced-sandboxes");
    std::fs::rename(&base, &displaced).unwrap();
    std::fs::create_dir(&base).unwrap();
    assert!(permit.measure().is_err());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn allocation_permit_serializes_capacity_observation_across_allocators() {
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    let fixture = tempdir().unwrap();
    let base = fixture.path().join("sandboxes");
    std::fs::create_dir(&base).unwrap();
    let first = AllocationPermit::acquire(&base).await.unwrap();
    let other_allocator_base = base.clone();
    let (started_tx, started_rx) = oneshot::channel();
    let mut waiting = tokio::spawn(async move {
        let _ = started_tx.send(());
        AllocationPermit::acquire(&other_allocator_base).await
    });
    started_rx.await.unwrap();
    assert!(
        timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err()
    );

    drop(first);
    let permit = timeout(Duration::from_secs(2), &mut waiting)
        .await
        .expect("permit lock should become available")
        .unwrap()
        .unwrap();
    assert_eq!(permit.measure().unwrap().source_roots, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn launch_floor_is_capped_at_a_quarter_of_a_small_disk() {
    const GIB: u64 = 1024 * 1024 * 1024;
    // A 60 GiB floor on a 192 GiB disk is 48 GiB; a 775 GiB hub keeps all 60.
    assert_eq!(effective_min_free_bytes(60, 192 * GIB), 48 * GIB);
    assert_eq!(effective_min_free_bytes(60, 775 * GIB), 60 * GIB);
    assert_eq!(effective_min_free_bytes(30, 192 * GIB), 30 * GIB);
    assert_eq!(effective_min_free_bytes(0, 192 * GIB), 0);
    assert_eq!(
        effective_min_free_bytes(60, 0),
        60 * GIB,
        "unknown size: no cap"
    );
    assert_eq!(effective_min_free_bytes(u64::MAX, u64::MAX), u64::MAX / 4);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[tokio::test]
async fn canonical_base_resolves_ancestor_symlinks_but_keeps_a_symlinked_base_refused() {
    use super::SandboxAllocator;

    let fixture = tempdir().unwrap();
    let real_parent = std::fs::canonicalize(fixture.path()).unwrap().join("real");
    std::fs::create_dir(&real_parent).unwrap();
    let real_base = real_parent.join("sandboxes");
    std::fs::create_dir(&real_base).unwrap();

    // A symlinked ancestor is resolved and the result is accepted.
    let linked_parent = fixture.path().join("linked");
    std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();
    let allocator = SandboxAllocator::new(linked_parent.join("sandboxes")).with_canonical_base();
    assert_eq!(allocator.base_dir(), real_base);
    AllocationPermit::acquire(allocator.base_dir())
        .await
        .expect("a base under a symlinked ancestor resolves to an accepted base");

    // A base that is itself a symlink to a foreign directory is not adopted.
    let foreign = std::fs::canonicalize(fixture.path())
        .unwrap()
        .join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    let symlinked_base = fixture.path().join("base-link");
    std::os::unix::fs::symlink(&foreign, &symlinked_base).unwrap();
    let allocator = SandboxAllocator::new(symlinked_base.clone()).with_canonical_base();
    assert_eq!(allocator.base_dir(), symlinked_base);
    let error = AllocationPermit::acquire(allocator.base_dir())
        .await
        .err()
        .expect("a symlinked base must remain refused");
    assert!(error.to_string().contains("exact canonical directory"));
}
