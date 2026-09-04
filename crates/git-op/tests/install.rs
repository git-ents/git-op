use std::{path::Path, process::Command};

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
