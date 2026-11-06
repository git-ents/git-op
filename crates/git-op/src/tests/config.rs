use super::*;
use std::{
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_TEMP_HOOKS_DIR: AtomicUsize = AtomicUsize::new(0);

/// A reference-transaction hook body written by an earlier git-op version that
/// this version still recognizes from its command line.
const HISTORICAL_HOOK_BODY: &str = "#!/bin/sh\nexec git op reference-transaction \"$@\"\n";

/// A `reference-transaction` body an earlier git-op version wrote, which this
/// version recognizes only as a whole.
const ORIGINAL_HOOK_BODY: &str = "#!/bin/sh\nexec op reference-transaction \"$@\"\n";

/// Verify that the body git-op claims as a whole is the one an earlier version
/// wrote, so the constant and the literal cannot drift apart.
#[test]
fn legacy_bodies_are_owned_as_a_whole() {
    assert_eq!(LEGACY_REFERENCE_TRANSACTION_BODIES.len(), 1);
    assert!(LEGACY_REFERENCE_TRANSACTION_BODIES.contains(&ORIGINAL_HOOK_BODY));
}

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
    assert_eq!(
        rewrite_owned_lines(HOOK_BODY, HOOK_LINE, is_git_op_line).as_deref(),
        Some(HOOK_BODY)
    );
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
        rewrite_owned_lines(existing, HOOK_LINE, is_git_op_line).as_deref(),
        Some(expected.as_str())
    );
    assert!(!is_git_op_line("echo git-op-helper"));
    assert!(!is_git_op_line("echo git-op reference-transaction"));
    assert!(!is_git_op_line("echo \"git-op reference-transaction\""));
}

/// Verify that the wrapped, qualified, and assigned spellings a hand-merged
/// hook may use are recognized, while commands that merely mention git-op are
/// not.
#[test]
fn classifies_wrapped_and_qualified_git_op_commands() {
    for owned in [
        "command git-op reference-transaction \"$@\"",
        "/usr/local/bin/git-op reference-transaction \"$@\"",
        "GIT_DIR=. git-op reference-transaction \"$@\"",
        "env GIT_DIR=. git-op reference-transaction \"$@\"",
        "exec /usr/bin/git op reference-transaction \"$@\"",
        "if git-op reference-transaction \"$@\"; then :; fi",
        "git op \"$hook\" \"$@\"",
    ] {
        assert!(is_git_op_line(owned), "{owned} must be recognized");
    }
    for foreign in [
        "xargs git-op reference-transaction \"$@\"",
        "sh -c 'git-op reference-transaction'",
        "# git-op reference-transaction \"$@\"",
        "git op \"$@\"",
        "git op \"$1\"",
    ] {
        assert!(!is_git_op_line(foreign), "{foreign} must not be recognized");
    }
    assert!(is_pre_push_line("exec /usr/bin/git-op pre-push \"$@\""));
    assert!(!is_pre_push_line("xargs git-op pre-push \"$@\""));
    assert!(is_git_op_line("git op \"$hook\""));
    assert!(
        !is_pre_push_line("git op \"$hook\""),
        "the generic delegation was only ever a reference-transaction spelling"
    );
}

/// Verify that a fresh install writes an executable hook with the current body.
#[test]
fn fresh_install_writes_executable_hook() {
    let hooks = TemporaryHooksDir::new();
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("install hook");
    let body = fs::read(hooks.hook_path()).expect("read installed hook");
    assert_eq!(body, HOOK_BODY.as_bytes());
    #[cfg(unix)]
    assert!(is_executable(&hooks.hook_path()));
}

/// Verify that installing over the current body is idempotent.
#[test]
fn install_over_current_body_is_idempotent() {
    let hooks = TemporaryHooksDir::new();
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("install hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks.hook_path(), fs::Permissions::from_mode(0o644))
            .expect("remove execute permissions");
    }
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("reinstall hook");
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
        install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("install hook");
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
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("install hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook after install");
    let expected = format!("#!/bin/sh\n{HOOK_LINE}echo something-else\n");
    assert_eq!(body, expected);
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("reinstall hook");
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
    let error = install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction)
        .expect_err("existing hook must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.hook_path()));
    assert_eq!(
        error.to_string(),
        format!(
            "refusing to modify hook {}: it is not git-op's; merge git-op's command into it manually",
            hooks.hook_path().display()
        )
    );
    assert_eq!(
        fs::read_to_string(hooks.hook_path()).expect("read existing hook"),
        existing
    );
}

/// Verify that a hook that cannot be read as text is refused on install, so
/// git-op never overwrites content it cannot classify.
#[test]
fn unreadable_hook_is_refused_on_install() {
    let hooks = TemporaryHooksDir::new();
    let unreadable = b"#!/bin/sh\n\xff\xfe\n";
    fs::write(hooks.hook_path(), unreadable).expect("write unreadable hook");
    let error = install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction)
        .expect_err("an unreadable hook must be refused");
    assert!(matches!(&error, Error::HookUnreadable(path) if path == &hooks.hook_path()));
    assert_eq!(
        fs::read(hooks.hook_path()).expect("read unreadable hook"),
        unreadable
    );
}

/// Verify that uninstalling leaves a hook it cannot read as text exactly as
/// found, since git-op writes only text and so cannot have written it, while
/// still removing the other, readable hook.
#[test]
fn uninstall_spares_an_unreadable_hook_and_still_removes_the_other() {
    let hooks = TemporaryHooksDir::new();
    let unreadable = b"#!/bin/sh\n\xff\xfe\n";
    fs::write(hooks.hook_path(), unreadable).expect("write unreadable hook");
    install_managed_hook(&hooks.path, ManagedHook::PrePush).expect("install pre-push hook");
    uninstall_hooks(hooks.path.clone()).expect("uninstall must not fail on an unreadable hook");
    assert_eq!(
        fs::read(hooks.hook_path()).expect("read unreadable hook"),
        unreadable,
        "an unreadable hook must be left untouched"
    );
    assert!(
        !hooks.pre_push_path().exists(),
        "the readable hook must still be removed"
    );
}

/// Verify that pre-push install writes an executable hook and is idempotent.
#[test]
fn pre_push_install_is_fresh_then_idempotent() {
    let hooks = TemporaryHooksDir::new();
    install_managed_hook(&hooks.path, ManagedHook::PrePush).expect("install pre-push hook");
    let body = fs::read(hooks.pre_push_path()).expect("read installed hook");
    assert_eq!(body, PRE_PUSH_BODY.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(hooks.pre_push_path(), fs::Permissions::from_mode(0o644))
            .expect("remove execute permissions");
    }
    install_managed_hook(&hooks.path, ManagedHook::PrePush).expect("reinstall pre-push hook");
    let body = fs::read(hooks.pre_push_path()).expect("read reinstalled hook");
    assert_eq!(body, PRE_PUSH_BODY.as_bytes());
    #[cfg(unix)]
    assert!(is_executable(&hooks.pre_push_path()));
}

/// Verify that a pre-push conflict is reported without withholding the
/// reference hook.
#[test]
fn pre_push_conflict_still_installs_the_reference_hook() {
    let hooks = TemporaryHooksDir::new();
    let existing = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.pre_push_path(), existing).expect("write existing hook");
    let error = install_hooks(hooks.path.clone()).expect_err("existing hook must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.pre_push_path()));
    assert_eq!(
        fs::read(hooks.hook_path()).expect("read installed hook"),
        HOOK_BODY.as_bytes(),
        "a pre-push conflict must not withhold the reference hook"
    );
    assert_eq!(
        fs::read_to_string(hooks.pre_push_path()).expect("read existing hook"),
        existing
    );
}

/// Verify that a reference hook conflict is reported without withholding the
/// pre-push hook.
#[test]
fn reference_conflict_still_installs_the_pre_push_hook() {
    let hooks = TemporaryHooksDir::new();
    let existing = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.hook_path(), existing).expect("write existing hook");
    let error = install_hooks(hooks.path.clone()).expect_err("existing hook must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.hook_path()));
    assert_eq!(
        fs::read(hooks.pre_push_path()).expect("read installed hook"),
        PRE_PUSH_BODY.as_bytes(),
        "a reference hook conflict must not withhold the pre-push hook"
    );
    assert_eq!(
        fs::read_to_string(hooks.hook_path()).expect("read existing hook"),
        existing
    );
}

/// Verify that an existing pre-push hook is never replaced, including a
/// standalone shell guard, which carries no git-op command line to adopt.
#[test]
fn pre_push_install_over_existing_hook_returns_hook_exists() {
    let standalone = concat!(
        "#!/bin/sh\n",
        "while read -r local_ref local_oid remote_ref remote_oid; do\n",
        "    if [ \"$local_ref\" = \"refs/op\" ]; then printf '%s\\n' \"refs/op is local\" >&2; exit 1; fi\n",
        "done\n",
    );
    for existing in ["#!/bin/sh\necho something-else\n", standalone] {
        let hooks = TemporaryHooksDir::new();
        fs::write(hooks.pre_push_path(), existing).expect("write existing hook");
        let error = install_managed_hook(&hooks.path, ManagedHook::PrePush)
            .expect_err("existing hook must be rejected");
        assert!(matches!(&error, Error::HookExists(path) if path == &hooks.pre_push_path()));
        assert_eq!(
            fs::read_to_string(hooks.pre_push_path()).expect("read existing hook"),
            existing
        );
    }
}

/// Verify that pre-push uninstall removes matching hooks and spares foreign ones.
#[test]
fn pre_push_uninstall_removes_matching_hook_and_spares_foreign_ones() {
    let hooks = TemporaryHooksDir::new();
    install_managed_hook(&hooks.path, ManagedHook::PrePush).expect("install pre-push hook");
    uninstall_managed_hook(&hooks.path, ManagedHook::PrePush).expect("uninstall pre-push hook");
    assert!(!hooks.pre_push_path().exists());
    uninstall_managed_hook(&hooks.path, ManagedHook::PrePush).expect("repeat uninstall is a no-op");

    let standalone = "#!/bin/sh\nwhile read -r local_ref; do exit 1; done\n";
    fs::write(hooks.pre_push_path(), standalone).expect("write standalone hook");
    uninstall_managed_hook(&hooks.path, ManagedHook::PrePush)
        .expect("uninstall over standalone hook");
    assert_eq!(
        fs::read_to_string(hooks.pre_push_path()).expect("read standalone hook"),
        standalone,
        "a hook carrying no git-op command line must be left untouched"
    );

    let foreign = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.pre_push_path(), foreign).expect("write foreign hook");
    uninstall_managed_hook(&hooks.path, ManagedHook::PrePush).expect("uninstall over foreign hook");
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
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("install hook");
    uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("uninstall hook");
    assert!(!hooks.hook_path().exists());
    uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction)
        .expect("repeat uninstall is a no-op");
}

/// Verify that a hook that merely runs a program named `op` is not claimed:
/// only a body git-op previously wrote verbatim is owned.
#[test]
fn foreign_bare_op_command_is_not_owned() {
    let hooks = TemporaryHooksDir::new();
    let foreign = "#!/bin/sh\necho before\nexec op reference-transaction \"$@\"\n";
    fs::write(hooks.hook_path(), foreign).expect("write foreign hook");
    let error = install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction)
        .expect_err("a hook git-op did not write must be rejected");
    assert!(matches!(&error, Error::HookExists(path) if path == &hooks.hook_path()));
    uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("uninstall hook");
    assert_eq!(
        fs::read_to_string(hooks.hook_path()).expect("read foreign hook"),
        foreign,
        "a hook git-op did not write must be left untouched"
    );
}

/// Verify that uninstalling removes hooks from earlier git-op versions.
#[test]
fn uninstall_removes_historical_hook() {
    for historical in [HISTORICAL_HOOK_BODY, ORIGINAL_HOOK_BODY] {
        let hooks = TemporaryHooksDir::new();
        fs::write(hooks.hook_path(), historical).expect("write historical hook");
        uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction)
            .expect("uninstall hook");
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
    uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("uninstall hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read stripped hook");
    assert_eq!(body, "#!/bin/sh\necho before\necho after\n");

    uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction)
        .expect("repeat uninstall");
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
    uninstall_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("uninstall hook");
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
    install_managed_hook(&hooks.path, ManagedHook::ReferenceTransaction).expect("install hook");
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
