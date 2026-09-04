use super::*;

/// Verify that trailer values flatten newlines.
#[test]
fn trailer_text_flattens_newlines() {
    assert_eq!(trailer_text("git commit"), "git commit");
    assert_eq!(trailer_text("git\nEvil: forged"), "git Evil: forged");
}
/// Verify that conflicting ref paths are rejected.
#[test]
fn ref_tree_rejects_file_directory_conflicts() {
    let refs = BTreeMap::from([
        ("refs/heads/main".to_owned(), Vec::new()),
        ("refs/heads/main/topic".to_owned(), Vec::new()),
    ]);
    assert!(matches!(
        validate_ref_paths(&refs),
        Err(Error::ConflictingRefs { .. })
    ));
}
/// Verify that a committed deletion of the operation ref uninstalls the
/// local hook and is not resurrected by a capture.
#[test]
fn op_ref_deletion_uninstalls_the_local_hook() {
    let temporary = TemporaryRepository::new();
    install_local(&temporary.repo).expect("install hook");
    append(&temporary.repo, "snapshot").expect("append snapshot");
    let tip = temporary
        .repo
        .try_find_reference(OP_REF)
        .expect("look up operation ref")
        .expect("operation ref exists")
        .target()
        .try_id()
        .expect("direct operation ref")
        .to_owned();
    // Git deletes the ref before invoking the committed-phase hook, and
    // gix applies the deletion without firing the installed hook, keeping
    // the simulation hermetic.
    temporary
        .repo
        .edit_reference(GixRefEdit {
            change: Change::Delete {
                expected: PreviousValue::MustExistAndMatch(Target::Object(tip)),
                log: RefLog::AndReference,
            },
            name: FullName::try_from(OP_REF).expect("valid operation ref name"),
            deref: false,
        })
        .expect("delete operation ref");

    reference_transaction(
        &temporary.repo,
        ReferenceTransactionPhase::Committed,
        format!("{tip} {ZERO_OID} {OP_REF}\n").as_bytes(),
    )
    .expect("process deletion transaction");

    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_none(),
        "a deleted operation log must not be resurrected"
    );
    assert!(
        !temporary
            .repo
            .git_dir()
            .join("hooks/reference-transaction")
            .exists(),
        "the committed deletion must remove the local hook"
    );
}
/// Verify that the hook parser recognizes only operation-ref deletions.
#[test]
fn hook_parser_detects_operation_ref_deletions() {
    let oid = "1111111111111111111111111111111111111111";
    assert!(
        transaction_deletes_operation_ref(format!("{oid} {ZERO_OID} {OP_REF}\n").as_bytes())
            .expect("parse deletion")
    );
    assert!(
        !transaction_deletes_operation_ref(format!("{ZERO_OID} {oid} {OP_REF}\n").as_bytes())
            .expect("parse creation"),
        "creating the operation ref is not a deletion"
    );
    assert!(
        !transaction_deletes_operation_ref(
            format!("{oid} {ZERO_OID} refs/heads/main\n").as_bytes()
        )
        .expect("parse branch deletion")
    );
    assert!(
        !transaction_deletes_operation_ref(b"").expect("parse empty input"),
        "empty input deletes nothing"
    );
}
/// Verify that empty hook input is treated as a no-op.
#[test]
fn hook_parser_accepts_empty_transactions() {
    assert!(!transaction_changes_captured_refs(b"\n").expect("empty transaction"));
    assert!(!transaction_changes_captured_refs(b"\r\n").expect("empty transaction"));
}
/// Verify that a detached HEAD records nothing: normal ref writes carry no
/// branch context, so neither the hook nor a manual snap captures one.
#[test]
fn snap_skips_detached_head() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
    git(&temporary, &["checkout", "--detach"]);

    let snapped = snap(&temporary.repo).expect("snap detached repository");
    assert_eq!(snapped, SnapOutcome::Detached);
    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_none(),
        "a detached HEAD must not start an operation log"
    );
}
/// Verify that every non-committed hook phase is accepted without a snapshot.
#[test]
fn hook_accepts_non_committed_phases() {
    let temporary = TemporaryRepository::new();
    let input = b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/main\n";
    for phase in [
        ReferenceTransactionPhase::Preparing,
        ReferenceTransactionPhase::Prepared,
        ReferenceTransactionPhase::Aborted,
    ] {
        reference_transaction(&temporary.repo, phase, input).expect("process hook phase");
    }
    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_none()
    );
}
/// Verify that hook input identifies only captured refs.
#[test]
fn hook_parser_classifies_only_captured_refs() {
    let oid = b"0000000000000000000000000000000000000000";
    assert!(
        transaction_changes_captured_refs(
            &[oid.as_slice(), oid.as_slice(), b"refs/heads/main", b"\n"].concat()
        )
        .is_err()
    );
    assert!(
        transaction_changes_captured_refs(
            &[
                oid.as_slice(),
                b" ",
                b"1111111111111111111111111111111111111111",
                b" ",
                b"refs/heads/main\n"
            ]
            .concat()
        )
        .expect("valid transaction")
    );
    assert!(
        !transaction_changes_captured_refs(
            &[
                oid.as_slice(),
                b" ",
                b"1111111111111111111111111111111111111111",
                b" ",
                b"refs/remotes/origin/main\n"
            ]
            .concat()
        )
        .expect("valid transaction")
    );
    assert!(
        !transaction_changes_captured_refs(
            &[
                oid.as_slice(),
                b" ",
                b"1111111111111111111111111111111111111111",
                b" ",
                OP_REF.as_bytes(),
                b"\n"
            ]
            .concat()
        )
        .expect("valid transaction")
    );
}
/// Verify that operation, remote, and pseudo refs are excluded.
#[test]
fn ref_filter_excludes_operation_and_remote_refs() {
    assert!(is_captured_ref(b"refs/heads/main"));
    assert!(is_captured_ref(b"refs/tags/v1"));
    assert!(!is_captured_ref(OP_REF.as_bytes()));
    assert!(!is_captured_ref(b"refs/remotes/origin/main"));
    assert!(!is_captured_ref(b"HEAD"));
}
/// Verify that an empty committed transaction leaves an existing operation log unchanged.
#[test]
fn hook_accepts_empty_committed_transaction() {
    let temporary = TemporaryRepository::new();
    reference_transaction(
        &temporary.repo,
        ReferenceTransactionPhase::Committed,
        b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/main\n",
    )
    .expect("record initial snapshot");
    let before = temporary
        .repo
        .try_find_reference(OP_REF)
        .expect("look up operation ref")
        .expect("operation ref exists")
        .id();
    reference_transaction(&temporary.repo, ReferenceTransactionPhase::Committed, b"")
        .expect("process empty transaction");
    assert_eq!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .expect("operation ref exists")
            .id(),
        before
    );
}
/// Verify that a `HEAD`-only transaction captures the initial state.
#[test]
fn hook_captures_initial_state_on_head_only_transaction() {
    let temporary = TemporaryRepository::new();
    let input = b"0000000000000000000000000000000000000000 ref:refs/heads/main HEAD\n";
    reference_transaction(&temporary.repo, ReferenceTransactionPhase::Committed, input)
        .expect("process committed HEAD transaction");
    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_some()
    );
}
/// Verify that a deletion seen before the committed phase changes nothing.
#[test]
fn op_ref_deletion_on_prepared_phase_leaves_the_hook() {
    let temporary = TemporaryRepository::new();
    install_local(&temporary.repo).expect("install hook");
    let tip = "1111111111111111111111111111111111111111";

    reference_transaction(
        &temporary.repo,
        ReferenceTransactionPhase::Prepared,
        format!("{tip} {ZERO_OID} {OP_REF}\n").as_bytes(),
    )
    .expect("process prepared deletion transaction");

    assert!(
        temporary
            .repo
            .git_dir()
            .join("hooks/reference-transaction")
            .exists(),
        "only the committed phase uninstalls"
    );
    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_none()
    );
}
/// Verify that deleting the operation log still uninstalls on a detached
/// HEAD, and that no snapshot is captured either way.
#[test]
fn op_ref_deletion_uninstalls_on_detached_head() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
    git(&temporary, &["checkout", "--detach"]);
    install_local(&temporary.repo).expect("install hook");

    reference_transaction(
        &temporary.repo,
        ReferenceTransactionPhase::Committed,
        format!("{ZERO_OID} {ZERO_OID} {OP_REF}\n").as_bytes(),
    )
    .expect("process deletion transaction on a detached head");

    assert!(
        !temporary
            .repo
            .git_dir()
            .join("hooks/reference-transaction")
            .exists(),
        "deletion handling takes precedence over the detached no-op"
    );
    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_none()
    );
}
/// Verify that a committed transaction on a detached HEAD records nothing.
#[test]
fn hook_skips_detached_head() {
    let temporary = TemporaryRepository::new();
    git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
    git(&temporary, &["checkout", "--detach"]);

    reference_transaction(
        &temporary.repo,
        ReferenceTransactionPhase::Committed,
        b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/main\n",
    )
    .expect("process committed transaction on a detached head");

    assert!(
        temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .is_none(),
        "a detached HEAD must not start an operation log"
    );
}
