use super::*;
use crate::OP_REF;
use std::{
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_HOOKS_DIR: AtomicUsize = AtomicUsize::new(0);

/// A reference-transaction hook body written by an earlier git-op version.
const HISTORICAL_HOOK_BODY: &str = "#!/bin/sh\nexec git op reference-transaction \"$@\"\n";
const ORIGINAL_HOOK_BODY: &str = "#!/bin/sh\nexec op reference-transaction \"$@\"\n";

/// Own a temporary hooks directory and remove it when the test finishes.
struct TemporaryHooksDir {
    path: PathBuf,
}

impl TemporaryHooksDir {
    fn new() -> Self {
        let path = loop {
            let sequence = NEXT_TEMP_HOOKS_DIR.fetch_add(1, Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "git-op-hooks-test-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create temporary hooks directory: {error}"),
            }
        };
        Self { path }
    }

    fn hook_path(&self) -> PathBuf {
        self.path.join(REFERENCE_TRANSACTION_HOOK_NAME)
    }

    fn pre_push_path(&self) -> PathBuf {
        self.path.join(PRE_PUSH_HOOK_NAME)
    }
}

impl Drop for TemporaryHooksDir {
    /// Remove the temporary hooks directory after the test completes.
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .expect("read hook metadata")
        .permissions()
        .mode()
        & 0o111
        != 0
}

/// Verify that the current body survives a rewrite unchanged.
#[test]
fn hook_body_is_recognized_unchanged() {
    assert_eq!(rewrite_managed_lines(HOOK_BODY).as_deref(), Some(HOOK_BODY));
    assert_eq!(
        rewrite_owned_lines(PRE_PUSH_BODY, PRE_PUSH_LINE, is_pre_push_line).as_deref(),
        Some(PRE_PUSH_BODY)
    );
}

#[test]
fn rewrite_removes_every_git_op_line() {
    let existing = concat!(
        "#!/bin/sh\n",
        "echo before\n",
        "git-op reference-transaction \"$@\"\n",
        "# git-op in a comment\n",
        "git op reference-transaction \"$@\"\n",
        "echo after\n",
    );
    let expected = format!(
        "#!/bin/sh\n{}{}{}{}",
        "echo before\n", HOOK_LINE, "# git-op in a comment\n", "echo after\n",
    );
    assert_eq!(
        rewrite_managed_lines(existing).as_deref(),
        Some(expected.as_str())
    );
    assert!(!is_git_op_line("echo git-op-helper"));
    assert!(!is_git_op_line("echo git-op reference-transaction"));
    assert!(!is_git_op_line("echo \"git-op reference-transaction\""));
}

/// Verify that a fresh install writes an executable hook with the current body.
#[test]
fn fresh_install_writes_executable_hook() {
    let hooks = TemporaryHooksDir::new();
    install_reference_transaction_hook(&hooks.path).expect("install hook");
    let body = fs::read(hooks.hook_path()).expect("read installed hook");
    assert_eq!(body, HOOK_BODY.as_bytes());
    #[cfg(unix)]
    assert!(is_executable(&hooks.hook_path()));
}

/// Verify that installing over the current body is idempotent.
#[test]
fn install_over_current_body_is_idempotent() {
    let hooks = TemporaryHooksDir::new();
    install_reference_transaction_hook(&hooks.path).expect("install hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks.hook_path(), fs::Permissions::from_mode(0o644))
            .expect("remove execute permissions");
    }
    install_reference_transaction_hook(&hooks.path).expect("reinstall hook");
    let body = fs::read(hooks.hook_path()).expect("read installed hook");
    assert_eq!(body, HOOK_BODY.as_bytes());
    #[cfg(unix)]
    assert!(is_executable(&hooks.hook_path()));
}

/// Verify that a hook written by an older git-op version is upgraded in place.
#[test]
fn install_over_historical_body_upgrades_in_place() {
    for historical in [HISTORICAL_HOOK_BODY, ORIGINAL_HOOK_BODY] {
        let hooks = TemporaryHooksDir::new();
        fs::write(hooks.hook_path(), historical).expect("write historical hook");
        install_reference_transaction_hook(&hooks.path).expect("install hook");
        let body = fs::read(hooks.hook_path()).expect("read installed hook");
        assert_eq!(body, HOOK_BODY.as_bytes());
    }
}

/// Verify that a merged hook keeps its own commands through an upgrade.
#[test]
fn install_over_merged_hook_upgrades_git_op_lines_only() {
    let hooks = TemporaryHooksDir::new();
    let merged = "#!/bin/sh\nexec git op reference-transaction \"$@\"\necho something-else\n";
    fs::write(hooks.hook_path(), merged).expect("write merged hook");
    install_reference_transaction_hook(&hooks.path).expect("install hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook after install");
    let expected = format!("#!/bin/sh\n{HOOK_LINE}echo something-else\n");
    assert_eq!(body, expected);
    install_reference_transaction_hook(&hooks.path).expect("reinstall hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook after reinstall");
    assert_eq!(
        body, expected,
        "reinstalling must not duplicate the git-op line"
    );
}

/// Verify that a hook invoking no git-op command is refused on install.
#[test]
fn install_over_existing_hook_returns_hook_exists() {
    let hooks = TemporaryHooksDir::new();
    let existing = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.hook_path(), existing).expect("write existing hook");
    let error = install_reference_transaction_hook(&hooks.path)
        .expect_err("existing hook must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.hook_path()));
    assert_eq!(
        error.to_string(),
        format!(
            "refusing to modify existing hook {}; merge hook commands manually",
            hooks.hook_path().display()
        )
    );
    assert_eq!(
        fs::read_to_string(hooks.hook_path()).expect("read existing hook"),
        existing
    );
}

#[cfg(unix)]
#[test]
fn pre_push_rejects_operation_ref_and_allows_other_refs() {
    let hooks = TemporaryHooksDir::new();
    install_pre_push_hook(&hooks.path).expect("install pre-push hook");

    let run = |input: &str| {
        let mut command = Command::new("/bin/sh");
        command
            .arg(hooks.path.join(PRE_PUSH_HOOK_NAME))
            .arg("origin")
            .arg("url")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().expect("spawn pre-push hook");
        child
            .stdin
            .take()
            .expect("open pre-push stdin")
            .write_all(input.as_bytes())
            .expect("write pre-push input");
        child.wait_with_output().expect("wait for pre-push hook")
    };
    let rejected = run(&format!(
        "{OP_REF} 1111111111111111111111111111111111111111 refs/heads/op 2222222222222222222222222222222222222222\n",
    ));
    let rejected_remote = run(&format!(
        "refs/heads/main 1111111111111111111111111111111111111111 {OP_REF} 2222222222222222222222222222222222222222\n",
    ));
    assert!(!rejected.status.success());
    assert!(!rejected_remote.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rejected.stderr).trim(),
        &format!(
            "git-op: Pushing {OP_REF} is not supported: operation references must remain local"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&rejected_remote.stderr).trim(),
        &format!(
            "git-op: Pushing {OP_REF} is not supported: operation references must remain local"
        )
    );

    let allowed = run(
        "refs/heads/main 1111111111111111111111111111111111111111 refs/heads/main 2222222222222222222222222222222222222222\n",
    );
    assert!(allowed.status.success());
    assert!(allowed.stderr.is_empty());
}

/// Verify that pre-push install writes an executable hook and is idempotent.
#[test]
fn pre_push_install_is_fresh_then_idempotent() {
    let hooks = TemporaryHooksDir::new();
    install_pre_push_hook(&hooks.path).expect("install pre-push hook");
    let body = fs::read(hooks.pre_push_path()).expect("read installed hook");
    assert_eq!(body, PRE_PUSH_BODY.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks.pre_push_path(), fs::Permissions::from_mode(0o644))
            .expect("remove execute permissions");
    }
    install_pre_push_hook(&hooks.path).expect("reinstall pre-push hook");
    let body = fs::read(hooks.pre_push_path()).expect("read reinstalled hook");
    assert_eq!(body, PRE_PUSH_BODY.as_bytes());
    #[cfg(unix)]
    assert!(is_executable(&hooks.pre_push_path()));
}

/// Verify that a pre-push conflict leaves the reference hook untouched.
#[test]
fn pre_push_conflict_prevents_partial_install() {
    let hooks = TemporaryHooksDir::new();
    let existing = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.pre_push_path(), existing).expect("write existing hook");
    let error = install_hooks(hooks.path.clone()).expect_err("existing hook must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.pre_push_path()));
    assert!(hooks.hook_path().exists());
    assert_eq!(
        fs::read_to_string(hooks.pre_push_path()).expect("read existing hook"),
        existing
    );
}

/// Verify that the legacy shell pre-push body upgrades to the command hook.
#[test]
fn pre_push_legacy_body_upgrades() {
    let hooks = TemporaryHooksDir::new();
    fs::write(hooks.pre_push_path(), LEGACY_PRE_PUSH_BODY).expect("write legacy hook");
    install_pre_push_hook(&hooks.path).expect("upgrade pre-push hook");
    assert_eq!(
        fs::read_to_string(hooks.pre_push_path()).expect("read upgraded hook"),
        PRE_PUSH_BODY
    );
}

/// Verify that an existing pre-push hook is never replaced.
#[test]
fn pre_push_install_over_existing_hook_returns_hook_exists() {
    let hooks = TemporaryHooksDir::new();
    let existing = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.pre_push_path(), existing).expect("write existing hook");
    let error = install_pre_push_hook(&hooks.path).expect_err("existing hook must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.pre_push_path()));
    assert_eq!(
        fs::read_to_string(hooks.pre_push_path()).expect("read existing hook"),
        existing
    );
}

/// Verify that pre-push uninstall removes matching hooks and spares foreign ones.
#[test]
fn pre_push_uninstall_removes_matching_hook_and_spares_foreign_ones() {
    let hooks = TemporaryHooksDir::new();
    install_pre_push_hook(&hooks.path).expect("install pre-push hook");
    uninstall_pre_push_hook(&hooks.path).expect("uninstall pre-push hook");
    assert!(!hooks.pre_push_path().exists());
    uninstall_pre_push_hook(&hooks.path).expect("repeat uninstall is a no-op");

    let legacy = LEGACY_PRE_PUSH_BODY;
    fs::write(hooks.pre_push_path(), legacy).expect("write legacy hook");
    uninstall_pre_push_hook(&hooks.path).expect("uninstall legacy pre-push hook");
    assert!(!hooks.pre_push_path().exists());

    let foreign = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.pre_push_path(), foreign).expect("write foreign hook");
    uninstall_pre_push_hook(&hooks.path).expect("uninstall over foreign hook");
    assert_eq!(
        fs::read_to_string(hooks.pre_push_path()).expect("read foreign hook"),
        foreign,
        "a foreign pre-push hook must be left untouched"
    );
}

/// Verify that uninstalling removes the current hook and is safe to repeat.
#[test]
fn uninstall_removes_current_hook_and_repeats_safely() {
    let hooks = TemporaryHooksDir::new();
    install_reference_transaction_hook(&hooks.path).expect("install hook");
    uninstall_reference_transaction_hook(&hooks.path).expect("uninstall hook");
    assert!(!hooks.hook_path().exists());
    uninstall_reference_transaction_hook(&hooks.path).expect("repeat uninstall is a no-op");
}

/// Verify that uninstalling removes hooks from earlier git-op versions.
#[test]
fn uninstall_removes_historical_hook() {
    for historical in [HISTORICAL_HOOK_BODY, ORIGINAL_HOOK_BODY] {
        let hooks = TemporaryHooksDir::new();
        fs::write(hooks.hook_path(), historical).expect("write historical hook");
        uninstall_reference_transaction_hook(&hooks.path).expect("uninstall hook");
        assert!(!hooks.hook_path().exists());
    }
}

/// Verify that uninstalling a merged hook strips only git-op's lines.
#[test]
fn uninstall_strips_git_op_lines_from_merged_hook() {
    let hooks = TemporaryHooksDir::new();
    let merged = concat!(
        "#!/bin/sh\n",
        "echo before\n",
        "exec git op reference-transaction \"$@\"\n",
        "echo after\n",
    );
    fs::write(hooks.hook_path(), merged).expect("write merged hook");
    uninstall_reference_transaction_hook(&hooks.path).expect("uninstall hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read stripped hook");
    assert_eq!(body, "#!/bin/sh\necho before\necho after\n");

    uninstall_reference_transaction_hook(&hooks.path).expect("repeat uninstall");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook again");
    assert_eq!(
        body, "#!/bin/sh\necho before\necho after\n",
        "a hook with no git-op lines left must be untouched"
    );
}

/// Verify that a hook invoking no git-op command is never deleted.
#[test]
fn uninstall_leaves_foreign_hook_untouched() {
    let hooks = TemporaryHooksDir::new();
    let foreign = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.hook_path(), foreign).expect("write foreign hook");
    uninstall_reference_transaction_hook(&hooks.path).expect("uninstall hook");
    assert_eq!(
        fs::read_to_string(hooks.hook_path()).expect("read foreign hook"),
        foreign
    );
}

/// Verify that a missing git-op binary never blocks ref updates: the hook
/// exits 0, warning only on the committed phase.
#[cfg(unix)]
#[test]
fn missing_binary_exits_zero_and_warns_only_on_committed_phase() {
    let hooks = TemporaryHooksDir::new();
    install_reference_transaction_hook(&hooks.path).expect("install hook");
    let run = |phase: &str| {
        let output = Command::new("/bin/sh")
            .arg(hooks.hook_path())
            .arg(phase)
            .env("PATH", "")
            .output()
            .expect("run hook without git-op on PATH");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    for phase in ["preparing", "prepared", "aborted"] {
        let (code, stderr) = run(phase);
        assert_eq!(code, Some(0), "{phase} phase must exit 0");
        assert_eq!(stderr, "", "{phase} phase must stay silent");
    }
    let (code, stderr) = run("committed");
    assert_eq!(code, Some(0), "committed phase must exit 0");
    assert!(
        stderr.contains("could not be found"),
        "committed phase should warn: {stderr}"
    );
    assert!(
        stderr.contains("git-op uninstall"),
        "warning should mention uninstalling: {stderr}"
    );
}

/// Verify that stock template content fills a template directory without
/// overwriting files that are already there, and that unrelated files
/// survive.
#[test]
fn copy_absent_populates_missing_files_and_keeps_existing_ones() {
    let root = tempfile::TempDir::new().expect("create temporary directory");
    let stock = root.path().join("stock");
    fs::create_dir_all(stock.join("hooks")).expect("create stock hooks");
    fs::create_dir_all(stock.join("info")).expect("create stock info");
    fs::write(stock.join("description"), b"stock description\n").expect("write description");
    fs::write(stock.join("info/exclude"), b"stock exclude\n").expect("write exclude");
    fs::write(stock.join("hooks/pre-commit.sample"), b"sample").expect("write sample");

    let template = root.path().join("template");
    fs::create_dir_all(template.join("hooks")).expect("create template hooks");
    fs::write(template.join("description"), b"custom description\n")
        .expect("write custom description");
    fs::write(template.join("hooks/post-commit"), b"custom hook").expect("write custom hook");

    copy_absent(&stock, &template).expect("copy stock templates");

    assert_eq!(
        fs::read(template.join("description")).expect("read description"),
        b"custom description\n",
        "existing files must not be overwritten"
    );
    assert_eq!(
        fs::read(template.join("info/exclude")).expect("read exclude"),
        b"stock exclude\n"
    );
    assert_eq!(
        fs::read(template.join("hooks/pre-commit.sample")).expect("read sample"),
        b"sample"
    );
    assert_eq!(
        fs::read(template.join("hooks/post-commit")).expect("read custom hook"),
        b"custom hook",
        "unrelated template files must survive"
    );

    copy_absent(&stock, &template).expect("re-copy stock templates");
    assert_eq!(
        fs::read(template.join("description")).expect("read description again"),
        b"custom description\n",
        "copying must be idempotent"
    );
}

/// Verify that the git-op-owned config file records the template directory
/// and that the appended `[include]` block makes it effective exactly once.
#[test]
fn include_block_activates_the_op_template_directory() {
    let root = tempfile::TempDir::new().expect("create temporary directory");
    let template = root.path().join("git/templates");
    let op_config = root.path().join("git/op-config");
    let config = root.path().join("gitconfig");

    write_op_config(&op_config, &template).expect("write op config");
    let configured: String = String::from_utf8(
        Command::new("git")
            .args(["config", "--file"])
            .arg(&op_config)
            .args(["--get", "init.templateDir"])
            .output()
            .expect("read op config")
            .stdout,
    )
    .expect("op config is UTF-8");
    assert_eq!(configured.trim(), template.to_str().expect("UTF-8 path"));

    let config_scope = ["--file", config.to_str().expect("UTF-8 path")];
    assert!(ensure_include(&config_scope, &op_config).expect("append include"));
    assert!(
        !ensure_include(&config_scope, &op_config).expect("re-append include"),
        "a second install must not duplicate the include entry"
    );

    let values: String = String::from_utf8(
        Command::new("git")
            .arg("config")
            .args(config_scope)
            .args(["--get-all", "include.path"])
            .output()
            .expect("read include entries")
            .stdout,
    )
    .expect("include entries are UTF-8");
    assert_eq!(
        values.lines().collect::<Vec<_>>(),
        vec![op_config.to_str().expect("UTF-8 path")],
        "exactly one include entry must reference the op config"
    );

    // `--file` skips include directives unless `--includes` is passed.
    let resolved: String = String::from_utf8(
        Command::new("git")
            .arg("config")
            .args(config_scope)
            .args(["--includes", "--get", "init.templateDir"])
            .output()
            .expect("resolve included value")
            .stdout,
    )
    .expect("resolved value is UTF-8");
    assert_eq!(resolved.trim(), template.to_str().expect("UTF-8 path"));
}
