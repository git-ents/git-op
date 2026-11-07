use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use git_op::OP_REF;
use tempfile::TempDir;

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_git-op"))
}

fn run(command: &str, root: &Path, home: &Path) {
    let output = Command::new(binary())
        .args(command.split_whitespace())
        .current_dir(root)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git-op");
    assert!(
        output.status.success(),
        "git-op {command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init(root: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("run git init");
    assert!(status.success());
}

#[test]
fn install_and_uninstall_are_local_by_default() {
    let root = TempDir::new().expect("create repository");
    let home = TempDir::new().expect("create home");
    init(root.path());
    let hook = root.path().join(".git/hooks/reference-transaction");

    run("install", root.path(), home.path());
    assert!(hook.is_file());
    assert!(
        !home
            .path()
            .join(".config/git/templates/hooks/reference-transaction")
            .exists()
    );

    run("uninstall", root.path(), home.path());
    assert!(!hook.exists());
}

#[test]
fn global_is_opt_in() {
    let root = TempDir::new().expect("create working directory");
    let home = TempDir::new().expect("create home");
    let template_hook = home
        .path()
        .join(".config/git/templates/hooks/reference-transaction");

    run("install --global", root.path(), home.path());
    assert!(template_hook.is_file());
    assert!(
        !root
            .path()
            .join(".git/hooks/reference-transaction")
            .exists()
    );

    run("uninstall --global", root.path(), home.path());
    assert!(!template_hook.exists());
    assert!(!home.path().join(".config/git/op-config").exists());
    assert!(!home.path().join(".config/git/templates").exists());
}

/// A template directory the user configured is never seeded or deleted, even
/// when its path matches the default one git-op would claim.
#[test]
fn user_configured_template_directory_is_never_seeded_or_deleted() {
    let root = TempDir::new().expect("create working directory");
    let home = TempDir::new().expect("create home");
    let templates = home.path().join(".config/git/templates");
    std::fs::create_dir_all(templates.join("hooks")).expect("create templates");
    std::fs::write(templates.join("custom"), b"custom\n").expect("write custom file");
    let set_template = Command::new("git")
        .args(["config", "--global", "init.templateDir"])
        .arg(&templates)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("configure template directory");
    assert!(set_template.success());
    let hook = templates.join("hooks/reference-transaction");

    run("install --global", root.path(), home.path());
    assert!(hook.is_file());
    assert!(
        !templates.join("description").exists(),
        "a user-configured template directory must not gain stock files"
    );
    assert!(
        !home.path().join(".config/git/op-config").exists(),
        "a user-configured template directory must not be claimed"
    );

    run("uninstall --global", root.path(), home.path());
    assert!(!hook.exists());
    assert!(
        templates.is_dir() && templates.join("custom").is_file(),
        "a user-configured template directory must survive uninstall"
    );
    let still_configured = Command::new("git")
        .args(["config", "--global", "--get", "init.templateDir"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("read configured template directory");
    assert!(still_configured.status.success());
    assert_eq!(
        String::from_utf8_lossy(&still_configured.stdout).trim(),
        templates.to_str().expect("UTF-8 path")
    );
}

/// The installed pre-push hook refuses a pushed operation ref and lets every
/// other ref through, resolving git-op from `PATH` as Git would.
#[cfg(unix)]
#[test]
fn installed_pre_push_hook_rejects_the_operation_ref() {
    let root = TempDir::new().expect("create repository");
    let home = TempDir::new().expect("create home");
    init(root.path());
    run("install", root.path(), home.path());
    let hook = root.path().join(".git/hooks/pre-push");
    assert!(hook.is_file(), "install must write the pre-push hook");

    // The hook reaches git-op through PATH, so put the binary under test first.
    let binary_dir = binary().parent().expect("binary has a parent directory");
    let search_path = std::env::join_paths(std::iter::once(binary_dir.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("join search path");

    let run_hook = |input: &str| {
        let mut child = Command::new("/bin/sh")
            .arg(&hook)
            .args(["origin", "url"])
            .env("PATH", &search_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pre-push hook");
        child
            .stdin
            .take()
            .expect("open pre-push stdin")
            .write_all(input.as_bytes())
            .expect("write pre-push input");
        child.wait_with_output().expect("wait for pre-push hook")
    };

    for rejected in [
        format!(
            "{OP_REF} 1111111111111111111111111111111111111111 refs/heads/op 2222222222222222222222222222222222222222\n"
        ),
        format!(
            "refs/heads/main 1111111111111111111111111111111111111111 {OP_REF} 2222222222222222222222222222222222222222\n"
        ),
    ] {
        let output = run_hook(&rejected);
        assert!(
            !output.status.success(),
            "{} must be rejected",
            rejected.trim()
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            format!(
                "git-op: Pushing {OP_REF} is not supported: operation references must remain local"
            )
        );
    }

    let allowed = run_hook(
        "refs/heads/main 1111111111111111111111111111111111111111 refs/heads/main 2222222222222222222222222222222222222222\n",
    );
    assert!(allowed.status.success(), "an ordinary ref must be allowed");
    assert!(allowed.stderr.is_empty());
}

/// The installed pre-push hook blocks a push only when `git-op` rejects it, so
/// a `git-op` on `PATH` that predates the `pre-push` subcommand does not fail
/// every push.
#[cfg(unix)]
#[test]
fn installed_pre_push_hook_fails_open_for_an_older_git_op() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = TempDir::new().expect("create repository");
    let home = TempDir::new().expect("create home");
    let stub = TempDir::new().expect("create stub directory");
    init(root.path());
    run("install", root.path(), home.path());
    let hook = root.path().join(".git/hooks/pre-push");
    assert!(hook.is_file(), "install must write the pre-push hook");

    let stub_binary = stub.path().join("git-op");
    let write_stub = |body: &str| {
        std::fs::write(&stub_binary, body).expect("write stub");
        std::fs::set_permissions(&stub_binary, std::fs::Permissions::from_mode(0o755))
            .expect("make stub executable");
    };

    let run_hook = |input: &str| {
        let mut child = Command::new("/bin/sh")
            .arg(&hook)
            .args(["origin", "url"])
            .env("PATH", stub.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pre-push hook");
        child
            .stdin
            .take()
            .expect("open pre-push stdin")
            .write_all(input.as_bytes())
            .expect("write pre-push input");
        child.wait_with_output().expect("wait for pre-push hook")
    };

    let rejected = format!(
        "{OP_REF} 1111111111111111111111111111111111111111 refs/heads/op 2222222222222222222222222222222222222222\n"
    );

    // An older binary reports the unknown subcommand with clap's usage status,
    // which must read as "cannot judge" rather than "reject".
    write_stub("#!/bin/sh\necho \"error: unrecognized subcommand 'pre-push'\" >&2\nexit 2\n");
    let older = run_hook(&rejected);
    assert!(
        older.status.success(),
        "an older git-op must not block the push"
    );

    // A rejection still blocks the push.
    write_stub("#!/bin/sh\necho 'git-op: rejected' >&2\nexit 1\n");
    let blocked = run_hook(&rejected);
    assert!(
        !blocked.status.success(),
        "a rejection must still block the push"
    );
}

/// A pre-push hook that git-op's command was pasted into, as the install error
/// message instructs, keeps running its own commands after git-op allows the
/// push.
#[cfg(unix)]
#[test]
fn merged_pre_push_hook_keeps_running_after_git_op_allows() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = TempDir::new().expect("create repository");
    let home = TempDir::new().expect("create home");
    init(root.path());
    run("install", root.path(), home.path());

    let installed = std::fs::read_to_string(root.path().join(".git/hooks/pre-push"))
        .expect("read installed pre-push hook");
    let git_op_line = installed
        .lines()
        .find(|line| line.contains("git-op pre-push"))
        .expect("the installed hook carries git-op's command");

    let merged = root.path().join("merged-pre-push");
    std::fs::write(
        &merged,
        format!("#!/bin/sh\n{git_op_line}\necho reached-the-end\n"),
    )
    .expect("write merged hook");
    std::fs::set_permissions(&merged, std::fs::Permissions::from_mode(0o755))
        .expect("make merged hook executable");

    let binary_dir = binary().parent().expect("binary has a parent directory");
    let search_path = std::env::join_paths(std::iter::once(binary_dir.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("join search path");

    let mut child = Command::new("/bin/sh")
        .arg(&merged)
        .args(["origin", "url"])
        .env("PATH", &search_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn merged hook");
    child
        .stdin
        .take()
        .expect("open merged hook stdin")
        .write_all(
            b"refs/heads/main 1111111111111111111111111111111111111111 refs/heads/main 2222222222222222222222222222222222222222\n",
        )
        .expect("write merged hook input");
    let output = child.wait_with_output().expect("wait for merged hook");
    assert!(
        output.status.success(),
        "an allowed push must leave the hook successful"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "reached-the-end",
        "commands after git-op's line must still run"
    );
}
