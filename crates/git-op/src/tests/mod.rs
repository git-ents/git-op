use super::*;
use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

/// The all-zero object ID Git reports for ref deletions.
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// Own a temporary repository and remove it when the test finishes.
struct TemporaryRepository {
    root: PathBuf,
    repo: gix::Repository,
}

impl TemporaryRepository {
    /// Create a repository with deterministic commit identity settings.
    fn new() -> Self {
        let root = loop {
            let sequence = NEXT_TEMP_REPOSITORY.fetch_add(1, Ordering::Relaxed);
            let candidate =
                std::env::temp_dir().join(format!("git-op-test-{}-{sequence}", std::process::id()));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary repository directory: {error}"),
            }
        };
        gix::init(&root).expect("initialize temporary repository");
        for (key, value) in [("user.name", "git-op test"), ("user.email", "git-op@test")] {
            let status = Command::new("git")
                .current_dir(&root)
                .args(["config", "--local", key, value])
                .status()
                .expect("configure temporary repository");
            assert!(status.success(), "git config {key} failed with {status}");
        }
        let repo = gix::open(&root).expect("reopen configured temporary repository");
        Self { root, repo }
    }
}

impl Drop for TemporaryRepository {
    /// Remove the temporary repository after the test completes.
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.root));
    }
}
/// Verify that the test repository can create a snapshot commit.
#[test]
fn temporary_repository_has_commit_identity() {
    let temporary = TemporaryRepository::new();
    append(&temporary.repo, "test snapshot").expect("append snapshot");
}
#[test]
fn generated_commit_records_invoking_command() {
    let temporary = TemporaryRepository::new();
    let tree = temporary
        .repo
        .write_buf(Kind::Tree, b"")
        .expect("write empty tree");
    let signature = gix::actor::Signature {
        name: "git-op test".into(),
        email: "git-op@test".into(),
        time: gix::date::Time {
            seconds: 1_700_000_000,
            offset: 0,
        },
    };
    let invoked_by = InvokedBy("git commit".to_owned());
    let commit = write_commit(
        &temporary.repo,
        CommitTreeRequest {
            tree,
            parent: None,
            message: "op: capture initial repository state",
            invoked_by: Some(&invoked_by),
            author: &signature,
            committer: &signature,
            signing: false,
        },
    )
    .expect("write operation commit");
    assert_eq!(
        temporary
            .repo
            .find_commit(commit)
            .expect("find operation commit")
            .message_raw_sloppy(),
        b"op: capture initial repository state\n\nInvoked-by: git commit\n"
    );
}

/// Run Git in a temporary repository and require successful completion.
fn git(temporary: &TemporaryRepository, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(&temporary.root)
        .args(args)
        .status()
        .expect("run Git");
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn git_output(temporary: &TemporaryRepository, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(&temporary.root)
        .args(args)
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "git {args:?} failed with {}",
        output.status
    );
    String::from_utf8(output.stdout).expect("Git output is UTF-8")
}

mod capture;
mod restore;
mod transaction;
