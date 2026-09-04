use super::*;

/// Verify that a no-op snap does not resolve write-only commit
/// prerequisites such as signing configuration or commit identity.
#[test]
fn snap_noop_does_not_resolve_commit_prerequisites() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["config", "--unset", "user.name"]);
    git(&temporary, &["config", "--unset", "user.email"]);
    // An invalid boolean makes commit_signing_enabled fail regardless of
    // any globally configured identity.
    git(&temporary, &["config", "commit.gpgSign", "maybe"]);

    // Publish the parent snapshot directly with explicit signatures so
    // its creation does not depend on the broken prerequisites that the
    // captured state itself contains.
    let state = capture(&temporary.repo).expect("capture state");
    let tree = serialize(&temporary.repo, &state).expect("serialize state");
    let signature = gix::actor::Signature {
        name: "git-op test".into(),
        email: "git-op@test".into(),
        time: gix::date::Time {
            seconds: 1_700_000_000,
            offset: 0,
        },
    };
    let parent = write_commit(
        &temporary.repo,
        CommitTreeRequest {
            tree,
            parent: None,
            message: "op: capture initial repository state",
            invoked_by: None,
            author: &signature,
            committer: &signature,
            signing: false,
        },
    )
    .expect("write parent snapshot");
    GixRefStore::new(&temporary.repo)
        .apply(RefEdit::Create {
            name: RefName::new(OP_REF).expect("valid OP_REF"),
            new: parent,
        })
        .expect("publish parent snapshot");

    let snapped = snap(&temporary.repo).expect("snap on a current repository");
    let SnapOutcome::Recorded(snapped) = snapped else {
        panic!("snap on a branch records or finds the current state");
    };
    assert_eq!(snapped, SnapResult::Current(parent));
}
/// Verify that a no-op restore still reports snapshot refs that point at
/// missing objects instead of silently reporting success.
#[test]
fn restore_noop_reports_missing_snapshot_referents() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let missing = temporary.repo.head_id().expect("read commit").detach();
    let snapshot = append(&temporary.repo, "snapshot").expect("append snapshot");
    let hex = missing.to_string();
    let loose = temporary
        .repo
        .git_dir()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    std::fs::remove_file(&loose).expect("remove loose commit object");

    match restore_action(&temporary.repo, snapshot) {
        Err(Error::PrunedObject { ref_name, oid }) => {
            assert_eq!(ref_name, "refs/heads/main");
            assert_eq!(oid, missing);
        }
        result => panic!("expected PrunedReferent, got {result:?}"),
    }
}
/// Verify that restoring a snapshot whose ref target has been pruned
/// aborts with an error and leaves the repository unmodified.
#[test]
fn restore_aborts_when_snapshot_ref_target_is_pruned() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let pruned = temporary
        .repo
        .head_id()
        .expect("read pruned commit")
        .detach();
    let snapshot = append(&temporary.repo, "snapshot of pruned commit")
        .expect("append snapshot of pruned commit");
    git(&temporary, &["commit", "--amend", "-m", "one amended"]);
    git(
        &temporary,
        &[
            "reflog",
            "expire",
            "--expire=now",
            "--expire-unreachable=now",
            "--all",
        ],
    );
    git(&temporary, &["gc", "--prune=now"]);

    let status = Command::new("git")
        .current_dir(&temporary.root)
        .args(["cat-file", "-t", &pruned.to_string()])
        .status()
        .expect("check pruned object");
    assert!(
        !status.success(),
        "object {pruned} survived garbage collection"
    );

    match restore_action(&temporary.repo, snapshot) {
        Err(Error::PrunedObject { ref_name, oid }) => {
            assert_eq!(ref_name, "refs/heads/main");
            assert_eq!(oid, pruned);
        }
        result => panic!("expected PrunedObject, got {result:?}"),
    }
    let amended = temporary
        .repo
        .head_id()
        .expect("read amended HEAD")
        .detach()
        .to_string();
    assert_eq!(
        git_output(&temporary, &["rev-parse", "refs/heads/main"]).trim(),
        amended
    );
    git_output(&temporary, &["status"]);
    git_output(&temporary, &["log", "--oneline", "-1"]);
}
/// Verify that restoring a snapshot whose branch targets an existing
/// non-commit object is rejected before refs move.
#[test]
fn restore_aborts_when_snapshot_branch_targets_non_commit() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("blob"), b"blob\n").expect("write blob file");
    let blob = git_output(&temporary, &["hash-object", "-w", "blob"])
        .trim()
        .to_owned();
    // Git refuses to point a branch at a blob, so write the loose ref
    // directly; a snapshot records ref targets as plain bytes either way.
    std::fs::create_dir_all(temporary.repo.git_dir().join("refs/heads"))
        .expect("create refs/heads");
    std::fs::write(
        temporary.repo.git_dir().join("refs/heads/broken"),
        format!("{blob}\n"),
    )
    .expect("write loose ref");
    let snapshot =
        append(&temporary.repo, "snapshot of blob branch").expect("append snapshot of blob branch");
    git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
    let moved = temporary.repo.head_id().expect("read moved HEAD").detach();

    match restore_action(&temporary.repo, snapshot) {
        Err(Error::UnusableObject { ref_name, oid }) => {
            assert_eq!(ref_name, "refs/heads/broken");
            assert_eq!(oid.to_string(), blob);
        }
        result => panic!("expected UnusableReferent, got {result:?}"),
    }
    assert_eq!(
        temporary.repo.head_id().expect("read HEAD"),
        moved,
        "refs must not move when a branch target is unusable"
    );
}
/// Verify that a restore refuses to overwrite a colliding untracked file
/// and leaves the repository untouched.
#[test]
fn restore_refuses_to_overwrite_untracked_files() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    fs::remove_file(temporary.root.join("tracked")).expect("remove tracked file");
    git(&temporary, &["commit", "-am", "two"]);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    std::fs::write(temporary.root.join("tracked"), b"untracked\n")
        .expect("write colliding untracked file");

    let error =
        restore_action(&temporary.repo, initial).expect_err("untracked collision must refuse");
    assert!(
        matches!(error, Error::OverwrittenPaths { ref collisions }
            if collisions.len() == 1
                && collisions[0]
                    == PathCollision { path: "tracked".to_owned(), untracked: true }),
        "expected the untracked collision, got {error:?}"
    );
    assert_eq!(
        fs::read_to_string(temporary.root.join("tracked")).expect("read untracked file"),
        "untracked\n",
        "the colliding file must be untouched"
    );
    assert_eq!(
        git_output(&temporary, &["status", "--porcelain"]),
        "?? tracked\n",
        "refs must not move when the restore is refused"
    );
}
/// Verify that a restore refuses to discard unstaged worktree
/// modifications on a path the target tree touches.
#[test]
fn restore_refuses_unstaged_modifications_on_affected_paths() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
    git(&temporary, &["commit", "-am", "two"]);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    // Even an index entry matching the target loses the worktree change:
    // read-tree --reset -u rewrites the file, so the path must block.
    git(&temporary, &["checkout", "--", "tracked"]);
    std::fs::write(temporary.root.join("tracked"), b"local edit\n").expect("modify tracked file");

    let error = restore_action(&temporary.repo, initial).expect_err("unstaged change must refuse");
    assert!(
        matches!(error, Error::OverwrittenPaths { ref collisions }
            if collisions.len() == 1
                && collisions[0]
                    == PathCollision { path: "tracked".to_owned(), untracked: false }),
        "expected the uncommitted change, got {error:?}"
    );
    assert_eq!(
        fs::read_to_string(temporary.root.join("tracked")).expect("read tracked file"),
        "local edit\n"
    );
}
/// Verify that restoring and then undoing returns refs and checkout state.
#[test]
fn restore_then_undo_restores_original_state() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"original\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let target = append(&temporary.repo, "target snapshot").expect("append target snapshot");
    std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
    git(&temporary, &["commit", "-am", "two"]);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    let original_head = temporary
        .repo
        .head_id()
        .expect("read original HEAD")
        .detach();
    let hooks = temporary.root.join("hooks");
    fs::create_dir(&hooks).expect("create hooks directory");
    let hook = hooks.join("reference-transaction");
    fs::write(&hook, b"#!/bin/sh\nexit 1\n").expect("write rejecting hook");
    let status = Command::new("chmod")
        .args(["+x", hook.to_str().expect("hook path is UTF-8")])
        .status()
        .expect("make hook executable");
    assert!(status.success(), "chmod failed with {status}");
    git(
        &temporary,
        &[
            "config",
            "core.hooksPath",
            hooks.to_str().expect("hooks path is UTF-8"),
        ],
    );

    let restored = restore(&temporary.repo, target).expect("restore target snapshot");
    undo(&temporary.repo).expect("undo restore");
    assert_ne!(restored, target);

    assert_eq!(
        temporary.repo.head_id().expect("read restored HEAD"),
        original_head
    );
    assert_eq!(
        fs::read_to_string(temporary.root.join("tracked")).expect("read restored file"),
        "changed\n"
    );
    assert_eq!(
        git_output(&temporary, &["status", "--porcelain"]),
        "?? hooks/\n"
    );
}
/// Verify that a restore refuses to discard staged changes on a path the
/// target tree would change.
#[test]
fn restore_refuses_staged_changes_on_affected_paths() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
    git(&temporary, &["commit", "-am", "two"]);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    std::fs::write(temporary.root.join("tracked"), b"staged\n").expect("stage change");
    git(&temporary, &["add", "tracked"]);

    let error = restore_action(&temporary.repo, initial).expect_err("staged change must refuse");
    assert!(matches!(error, Error::OverwrittenPaths { .. }));
    assert_eq!(
        git_output(&temporary, &["diff", "--cached", "--name-only"]),
        "tracked\n",
        "the staged change must survive the refused restore"
    );
}
#[test]
fn undo_refuses_to_remove_checked_out_branch() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "base"]);
    append(&temporary.repo, "initial").expect("append initial");
    git(&temporary, &["checkout", "-b", "topic"]);
    append(&temporary.repo, "topic").expect("append topic");
    let error = undo_action(&temporary.repo).expect_err("checked-out branch must survive");
    assert!(matches!(error, Error::CheckedOutBranch { .. }));
    assert!(temporary.repo.head_name().expect("read HEAD").is_some());
    assert!(temporary.repo.find_reference("refs/heads/topic").is_ok());
}

#[test]
fn undo_and_redo_follow_the_logical_transition_table() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "base"]);
    let a = append(&temporary.repo, "A").expect("append A");
    let initial_description = fs::read(temporary.repo.common_dir().join("description"))
        .expect("read initial description");
    fs::write(temporary.repo.common_dir().join("description"), b"B\n").expect("write B");
    let b = append(&temporary.repo, "B").expect("append B");
    fs::write(temporary.repo.common_dir().join("description"), b"C\n").expect("write C");
    let c = append(&temporary.repo, "C").expect("append C");

    let undo_c = undo_action(&temporary.repo).expect("undo C");
    assert_eq!(undo_c.target, c);
    assert_eq!(
        fs::read(temporary.repo.common_dir().join("description")).expect("read B"),
        b"B\n"
    );
    let undo_b = undo_action(&temporary.repo).expect("undo B");
    assert_eq!(undo_b.target, b);
    assert_eq!(
        fs::read(temporary.repo.common_dir().join("description")).expect("read A"),
        initial_description
    );

    let redo_b = redo_action(&temporary.repo).expect("redo B");
    assert_eq!(redo_b.target, b);
    let redo_c = redo_action(&temporary.repo).expect("redo C");
    assert_eq!(redo_c.target, c);
    assert_eq!(
        fs::read(temporary.repo.common_dir().join("description")).expect("read C"),
        b"C\n"
    );
    assert!(matches!(
        redo_action(&temporary.repo),
        Err(Error::NothingToRedo)
    ));

    let operation = temporary
        .repo
        .find_commit(undo_c.operation)
        .expect("read undo operation");
    let message = operation.message_raw_sloppy();
    assert!(
        message
            .windows(b"Git-op: undo:".len())
            .any(|window| window == b"Git-op: undo:")
    );
    assert!(
        message
            .windows(b"Git-op: undo:".len())
            .any(|window| window == b"Git-op: undo:")
    );
    assert!(
        message
            .windows(b"Git-op: restore:".len())
            .any(|window| window == b"Git-op: restore:")
    );
    assert_ne!(a, b);
}
/// Verify that a ref-level no-op restore leaves uncommitted index and
/// worktree changes untouched.
#[test]
fn restore_noop_preserves_uncommitted_changes() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let head = temporary.repo.head_id().expect("read HEAD").detach();
    let snapshot = append(&temporary.repo, "snapshot").expect("append snapshot");

    std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
    std::fs::write(temporary.root.join("staged"), b"staged\n").expect("write staged file");
    git(&temporary, &["add", "staged"]);
    let dirty = git_output(&temporary, &["status", "--porcelain"]);
    assert!(
        !dirty.is_empty(),
        "index and worktree should start desynced"
    );

    let result = restore_action(&temporary.repo, snapshot).expect("restore snapshot");
    assert!(result.changes.is_empty());
    assert_eq!(result.operation, snapshot);
    assert_eq!(temporary.repo.head_id().expect("read HEAD"), head);
    assert_eq!(
        git_output(&temporary, &["status", "--porcelain"]),
        dirty,
        "uncommitted index and worktree changes must survive a no-op restore"
    );
}
#[test]
fn restore_of_current_state_does_not_append() {
    let temporary = TemporaryRepository::new();
    let current = append(&temporary.repo, "current").expect("append current");
    let result = restore_action(&temporary.repo, current).expect("restore current");
    assert!(result.changes.is_empty());
    assert_eq!(result.operation, current);
}
/// Verify that restoring a branch ref also updates its symbolic `HEAD` target.
#[test]
fn restore_updates_head_for_current_branch() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let first = temporary
        .repo
        .head_id()
        .expect("read first commit")
        .detach();
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
    let second = temporary
        .repo
        .head_id()
        .expect("read second commit")
        .detach();
    assert_ne!(first, second);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    let result = restore_action(&temporary.repo, initial).expect("restore initial snapshot");
    assert_eq!(
        result.changes,
        vec![Changes::Refs(vec![RefChange {
            name: "refs/heads/main".to_owned(),
            kind: RefChangeKind::Updated {
                old: Target::Object(second),
                new: Target::Object(first),
            },
        }])]
    );

    assert_eq!(temporary.repo.head_id().expect("read restored HEAD"), first);
    assert_eq!(
        fs::read_to_string(temporary.root.join("tracked")).expect("read restored file"),
        "initial\n"
    );
    assert_eq!(git_output(&temporary, &["status", "--porcelain"]), "");
    assert_eq!(
        temporary
            .repo
            .head_name()
            .expect("read HEAD name")
            .map(|name| name.to_string()),
        Some("refs/heads/main".to_owned())
    );
}
#[test]
fn new_work_after_undo_ends_the_redo_chain() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "base"]);
    append(&temporary.repo, "A").expect("append A");
    fs::write(temporary.repo.common_dir().join("description"), b"B\n").expect("write B");
    append(&temporary.repo, "B").expect("append B");
    fs::write(temporary.repo.common_dir().join("description"), b"C\n").expect("write C");
    let c = append(&temporary.repo, "C").expect("append C");
    undo_action(&temporary.repo).expect("undo C");
    fs::write(temporary.repo.common_dir().join("description"), b"D\n").expect("write D");
    append(&temporary.repo, "D").expect("append D");

    assert!(matches!(
        redo_action(&temporary.repo),
        Err(Error::NothingToRedo)
    ));
    undo_action(&temporary.repo).expect("undo D");
    assert_eq!(
        fs::read(temporary.repo.common_dir().join("description")).expect("read B"),
        b"B\n"
    );
    restore(&temporary.repo, c).expect("restore C");
    assert_eq!(
        fs::read(temporary.repo.common_dir().join("description")).expect("read C"),
        b"C\n"
    );
}
/// Verify that restoring a detached repository leaves `HEAD` detached at its current commit.
#[test]
fn restore_preserves_detached_head_commit() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
    let first = temporary
        .repo
        .head_id()
        .expect("read first commit")
        .detach();
    git(&temporary, &["checkout", "--detach", &first.to_string()]);
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
    let second = temporary
        .repo
        .head_id()
        .expect("read second commit")
        .detach();
    assert_ne!(first, second);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    restore(&temporary.repo, initial).expect("restore initial snapshot");

    assert_eq!(
        temporary.repo.head_id().expect("read restored HEAD"),
        second
    );
    assert!(
        temporary
            .repo
            .head_name()
            .expect("read HEAD name")
            .is_none()
    );
}
/// Verify that unrelated untracked files do not block a restore and
/// survive it.
#[test]
fn restore_proceeds_with_unrelated_untracked_files() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    std::fs::write(temporary.root.join("unrelated"), b"untracked\n")
        .expect("write unrelated untracked file");

    restore(&temporary.repo, initial).expect("restore with unrelated untracked file");

    assert_eq!(
        fs::read_to_string(temporary.root.join("unrelated")).expect("read untracked file"),
        "untracked\n",
        "unrelated untracked files must survive a restore"
    );
}
/// Verify that a restore refuses when the target deletes a file carrying
/// uncommitted changes.
#[test]
fn restore_refuses_when_target_deletes_a_dirty_file() {
    let temporary = TemporaryRepository::new();
    std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
    git(&temporary, &["add", "tracked"]);
    git(&temporary, &["commit", "-m", "one"]);
    let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
    std::fs::write(temporary.root.join("victim"), b"victim\n").expect("write victim file");
    git(&temporary, &["add", "victim"]);
    git(&temporary, &["commit", "-m", "add victim"]);
    append(&temporary.repo, "current snapshot").expect("append current snapshot");

    std::fs::write(temporary.root.join("victim"), b"dirty\n").expect("modify victim file");

    let error = restore_action(&temporary.repo, initial).expect_err("dirty deletion must refuse");
    assert!(
        matches!(error, Error::OverwrittenPaths { ref collisions }
            if collisions.len() == 1
                && collisions[0]
                    == PathCollision { path: "victim".to_owned(), untracked: false }),
        "expected the victim path, got {error:?}"
    );
    assert_eq!(
        fs::read_to_string(temporary.root.join("victim")).expect("read victim file"),
        "dirty\n"
    );
}
