use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP_HOOKS_DIR: AtomicUsize = AtomicUsize::new(0);

/// Historical hook body shipped before [`HOOK_LINE`] existed.
const HISTORICAL_HOOK_BODY: &str = "#!/bin/sh\nexec git op reference-transaction \"$@\"\n";

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
        self.path.join(HOOK_NAME)
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

/// Verify that a hook this version writes is still recognized, and left
/// unchanged, by the next upgrade.
#[test]
fn hook_body_is_recognized_unchanged() {
    assert_eq!(rewrite_managed_line(HOOK_BODY).as_deref(), Some(HOOK_BODY));
}

#[test]
fn rewrite_removes_every_git_op_line() {
    let existing = concat!(
        "#!/bin/sh\n",
        "echo before\n",
        "git-op old-hook-line\n",
        "# git-op in a comment\n",
        "git op reference-transaction \"$@\"\n",
        "echo after\n",
    );
    let expected = format!(
        "#!/bin/sh\n{}{}{}{}",
        "echo before\n", HOOK_LINE, "# git-op in a comment\n", "echo after\n",
    );
    assert_eq!(
        rewrite_managed_line(existing).as_deref(),
        Some(expected.as_str())
    );
}

/// Verify that a fresh install writes an executable hook with the current body.
#[test]
fn fresh_install_writes_executable_hook() {
    let hooks = TemporaryHooksDir::new();
    install_hook(&hooks.path).expect("install hook");
    let body = fs::read(hooks.hook_path()).expect("read installed hook");
    assert_eq!(body, HOOK_BODY.as_bytes());
    #[cfg(unix)]
    assert!(is_executable(&hooks.hook_path()));
}

/// Verify that installing over the current body is idempotent.
#[test]
fn install_over_current_body_is_idempotent() {
    let hooks = TemporaryHooksDir::new();
    install_hook(&hooks.path).expect("install hook");
    install_hook(&hooks.path).expect("reinstall hook");
    let body = fs::read(hooks.hook_path()).expect("read installed hook");
    assert_eq!(body, HOOK_BODY.as_bytes());
}

/// Verify that a hook written by an older git-op version is upgraded in place.
#[test]
fn install_over_historical_body_upgrades_in_place() {
    let hooks = TemporaryHooksDir::new();
    fs::write(hooks.hook_path(), HISTORICAL_HOOK_BODY).expect("write historical hook");
    install_hook(&hooks.path).expect("install hook");
    let body = fs::read(hooks.hook_path()).expect("read installed hook");
    assert_eq!(body, HOOK_BODY.as_bytes());
}

/// Verify that a hook of your own gains the git-op line and keeps its
/// own commands.
#[test]
fn install_over_foreign_hook_splices_the_git_op_line() {
    let hooks = TemporaryHooksDir::new();
    let foreign = "#!/bin/sh\necho something-else\n";
    fs::write(hooks.hook_path(), foreign).expect("write foreign hook");
    install_hook(&hooks.path).expect("install hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook after install");
    let expected = format!("#!/bin/sh\n{HOOK_LINE}echo something-else\n");
    assert_eq!(body, expected);
    install_hook(&hooks.path).expect("reinstall hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook after reinstall");
    assert_eq!(
        body, expected,
        "reinstalling must not duplicate the git-op line"
    );
}

/// Verify that a hook without a shebang is spliced at the start.
#[test]
fn install_over_shebangless_hook_splices_at_the_start() {
    let hooks = TemporaryHooksDir::new();
    fs::write(hooks.hook_path(), "echo something-else\n").expect("write foreign hook");
    install_hook(&hooks.path).expect("install hook");
    let body = fs::read_to_string(hooks.hook_path()).expect("read hook after install");
    assert_eq!(body, format!("{HOOK_LINE}echo something-else\n"));
}

/// Verify that a missing git-op binary never blocks ref updates: the hook
/// exits 0, warning only on the committed phase.
#[cfg(unix)]
#[test]
fn missing_binary_exits_zero_and_warns_only_on_committed_phase() {
    let hooks = TemporaryHooksDir::new();
    install_hook(&hooks.path).expect("install hook");
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
