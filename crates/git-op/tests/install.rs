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

/// A template directory the user configured belongs to them: git-op installs
/// its hook there without adding stock files, and uninstall removes only the
/// hook, never the directory — even when its path matches the default one
/// git-op would claim.
#[test]
fn user_configured_template_directory_is_never_seeded_or_deleted() {
    let root = TempDir::new().expect("create working directory");
    let home = TempDir::new().expect("create home");
    // The user configures exactly the path git-op would claim by default.
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
