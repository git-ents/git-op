use super::*;

/// Verify that generated messages identify changed snapshot components.
#[test]
fn generated_snapshot_message_identifies_changes() {
    let temporary = TemporaryRepository::new();
    let (initial, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append initial snapshot");
    assert_eq!(
        temporary
            .repo
            .find_commit(initial)
            .expect("find initial snapshot")
            .message()
            .expect("read initial message")
            .summary()
            .as_bytes(),
        b"op: capture initial repository state"
    );

    fs::write(
        temporary.repo.common_dir().join("description"),
        b"example\n",
    )
    .expect("write description");
    Command::new("git")
        .current_dir(&temporary.root)
        .args(["update-ref", "refs/heads/example", &initial.to_string()])
        .status()
        .expect("update example ref")
        .success()
        .then_some(())
        .expect("update example ref succeeds");
    let (update, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append changed snapshot");
    assert_eq!(
        temporary
            .repo
            .find_commit(update)
            .expect("find changed snapshot")
            .message()
            .expect("read changed message")
            .summary()
            .as_bytes(),
        b"op: update refs and description"
    );
}
/// Verify that a single changed part produces a singular update message.
#[test]
fn generated_snapshot_message_reports_single_change() {
    let temporary = TemporaryRepository::new();
    let (initial, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append initial snapshot");
    Command::new("git")
        .current_dir(&temporary.root)
        .args(["update-ref", "refs/heads/example", &initial.to_string()])
        .status()
        .expect("update example ref")
        .success()
        .then_some(())
        .expect("update example ref succeeds");
    let (update, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append changed snapshot");
    assert_eq!(
        temporary
            .repo
            .find_commit(update)
            .expect("find changed snapshot")
            .message()
            .expect("read changed message")
            .summary()
            .as_bytes(),
        b"op: update refs"
    );
}
/// Verify that three changed parts are joined with an Oxford comma.
#[test]
fn generated_snapshot_message_reports_three_changes() {
    let temporary = TemporaryRepository::new();
    let (initial, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append initial snapshot");
    Command::new("git")
        .current_dir(&temporary.root)
        .args(["update-ref", "refs/heads/example", &initial.to_string()])
        .status()
        .expect("update example ref")
        .success()
        .then_some(())
        .expect("update example ref succeeds");
    let config_path = temporary.repo.common_dir().join("config");
    let mut config = fs::read(&config_path).expect("read config");
    config.extend_from_slice(b"[extra]\n\tflag = true\n");
    fs::write(&config_path, config).expect("write config");
    fs::write(
        temporary.repo.common_dir().join("description"),
        b"example\n",
    )
    .expect("write description");
    let (update, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append changed snapshot");
    assert_eq!(
        temporary
            .repo
            .find_commit(update)
            .expect("find changed snapshot")
            .message()
            .expect("read changed message")
            .summary()
            .as_bytes(),
        b"op: update refs, config, and description"
    );
}
/// Verify that a generated append with nothing to record neither creates a
/// commit nor advances the operation ref.
#[test]
fn generated_append_is_noop_when_nothing_changed() {
    let temporary = TemporaryRepository::new();
    let (initial, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append initial snapshot");
    let (repeat, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append unchanged snapshot");
    assert_eq!(repeat, initial);
    assert_eq!(
        temporary
            .repo
            .find_reference(OP_REF)
            .expect("read operation ref")
            .target()
            .try_id(),
        Some(initial.as_ref())
    );
}
/// Verify that `snap` on an up-to-date repository returns the current tip
/// and leaves the operation ref untouched.
#[test]
fn snap_is_noop_when_log_is_current() {
    let temporary = TemporaryRepository::new();
    let SnapOutcome::Recorded(initial) = snap(&temporary.repo).expect("append initial snapshot")
    else {
        panic!("first snap on a branch records the initial snapshot");
    };
    let SnapResult::Appended(initial) = initial else {
        panic!("first snap should append");
    };
    let SnapOutcome::Recorded(snapped) = snap(&temporary.repo).expect("snap up-to-date repository")
    else {
        panic!("snap on a branch records or finds the current state");
    };
    assert_eq!(snapped, SnapResult::Current(initial));
    assert_eq!(
        temporary
            .repo
            .find_reference(OP_REF)
            .expect("read operation ref")
            .target()
            .try_id(),
        Some(initial.as_ref())
    );
}
/// Verify that `snap` records drift, such as a direct edit to the
/// repository description that no reference transaction observed.
#[test]
fn snap_records_metadata_drift() {
    let temporary = TemporaryRepository::new();
    let SnapOutcome::Recorded(initial) = snap(&temporary.repo).expect("append initial snapshot")
    else {
        panic!("snap on a branch records the initial snapshot");
    };
    let initial = initial.operation();
    fs::write(
        temporary.repo.common_dir().join("description"),
        b"drifted description\n",
    )
    .expect("write description");
    let snapped = match snap(&temporary.repo).expect("snap drifted repository") {
        SnapOutcome::Recorded(snapped) => snapped.operation(),
        SnapOutcome::Detached => panic!("snap on a branch records the drifted state"),
    };
    assert_ne!(snapped, initial);
    let changed = changes(&temporary.repo, snapped)
        .expect("compute changes")
        .expect("snapped snapshot has a parent");
    assert!(matches!(changed.as_slice(), [Changes::Description]));
    let state = read(&temporary.repo, snapped).expect("read snapped state");
    let description = temporary
        .repo
        .find_blob(state.description.expect("description was captured").oid())
        .expect("read description blob");
    assert_eq!(description.data, b"drifted description\n");
}
/// Verify that `changes` reports `None` for the initial snapshot and the
/// correct parts changed for a later one.
#[test]
fn changes_reports_parts_changed_since_parent() {
    let temporary = TemporaryRepository::new();
    let (initial, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append initial snapshot");
    assert_eq!(
        changes(&temporary.repo, initial).expect("compute changes"),
        None
    );

    Command::new("git")
        .current_dir(&temporary.root)
        .args(["update-ref", "refs/heads/example", &initial.to_string()])
        .status()
        .expect("update example ref")
        .success()
        .then_some(())
        .expect("update example ref succeeds");
    fs::write(
        temporary.repo.common_dir().join("description"),
        b"example\n",
    )
    .expect("write description");
    let (update, _) = append_internal(
        &temporary.repo,
        CommitMessage::Generated,
        AppendOptions::default(),
    )
    .expect("append changed snapshot");
    let changed = changes(&temporary.repo, update)
        .expect("compute changes")
        .expect("updated snapshot has a parent");
    assert!(
        matches!(changed.as_slice(), [Changes::Refs(refs), Changes::Description] if refs.len() == 1)
    );
}
