use std::fs;

use super::{
    EMPTY_BLOB, action_header, action_summary, ensure_clean, snap_outcome_summary, snap_summary,
};
fn repository() -> (gix::Repository, tempfile::TempDir) {
    let temp = tempfile::TempDir::new().expect("create temporary directory");
    let repo = gix::init(temp.path()).expect("initialize repository");
    (repo, temp)
}

#[test]
fn action_summary_lists_ref_updates() {
    let old = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
        .expect("parse old object ID");
    let new = gix::ObjectId::from_hex(b"2222222222222222222222222222222222222222")
        .expect("parse new object ID");
    let result = git_op::ActionResult {
        operation: old,
        target: new,
        restored: new,
        changes: vec![git_op::Changes::Refs(vec![git_op::RefChange {
            name: "refs/heads/main".to_owned(),
            kind: git_op::RefChangeKind::Updated {
                old: gix::refs::Target::Object(old),
                new: gix::refs::Target::Object(new),
            },
        }])],
    };
    assert_eq!(
        action_summary(
            &action_header("Restored to operation", &result.target, None),
            &result
        ),
        "Restored to operation 2222222\nrefs/heads/main -> 2222222"
    );
    assert_eq!(
        action_header("Undid operation", &old, Some(&new)),
        "Undid operation 1111111 (restored 2222222)"
    );
}
#[test]
fn action_summary_reports_noop() {
    let oid = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
        .expect("parse object ID");
    let result = git_op::ActionResult {
        operation: oid,
        target: oid,
        restored: oid,
        changes: Vec::new(),
    };
    assert_eq!(
        action_summary(
            &action_header("Restored to operation", &result.target, None),
            &result
        ),
        "Restored to operation 1111111; no updates"
    );
}

/// Verify that a snap only claims a snapshot for this invocation when
/// this call appended it.
#[test]
fn snap_summary_reports_whether_this_invocation_appended() {
    let oid = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
        .expect("parse object ID");
    let appended = git_op::SnapResult::Appended(oid);
    assert_eq!(snap_summary(appended), "Recorded snapshot 1111111");
    let current = git_op::SnapResult::Current(oid);
    assert_eq!(snap_summary(current), "Operation log is current (1111111)");
}

/// Verify that a detached HEAD reports an informational no-op.
#[test]
fn snap_outcome_reports_detached_head() {
    assert_eq!(
        snap_outcome_summary(&git_op::SnapOutcome::Detached),
        "HEAD is detached; no snapshot recorded"
    );
    let recorded = git_op::SnapOutcome::Recorded(git_op::SnapResult::Current(
        gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .expect("parse object ID"),
    ));
    assert_eq!(
        snap_outcome_summary(&recorded),
        "Operation log is current (1111111)"
    );
}

#[test]
fn clean_worktree_is_allowed() {
    let (repo, _temp) = repository();
    ensure_clean(&repo).expect("clean worktree should be allowed");
}

#[test]
fn dirty_worktree_is_rejected() {
    let (repo, temp) = repository();
    fs::write(temp.path().join("untracked"), b"change").expect("write untracked file");
    let error = ensure_clean(&repo).expect_err("dirty worktree should be rejected");
    assert_eq!(
        error.to_string(),
        "working tree is dirty; commit or restore before changing repository state"
    );
}

/// Verify that the well-known empty object IDs match what Git computes.
#[test]
fn well_known_empty_object_ids_match_git() {
    let (_repo, temp) = repository();
    let output = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(temp.path())
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_owned()
    };
    assert_eq!(output(&["hash-object", "-w", "--stdin"]), EMPTY_BLOB);
    assert_eq!(
        output(&["mktree"]),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    );
}
