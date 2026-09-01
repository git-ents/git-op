//! Git configuration, paths, and hook installation.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use crate::Error;

const HOOK_NAME: &str = "reference-transaction";

/// Hook block written by fresh installs and hook upgrades.
///
/// The hook fails open: when the `git-op` executable is missing it exits 0 so
/// a lost operation log never blocks ref updates, warning only on the
/// `committed` phase so ordinary transactions stay quiet.
macro_rules! hook_line {
    () => {
        "command -v git-op >/dev/null 2>&1 || { [ \"$1\" = committed ] && printf >&2 \"\\n%s\\n\\n    %s\\n\" \"This repository has an operation log enabled via Git Op, but the 'git-op' executable could not be found, so this operation was not recorded. If you no longer wish for operations to be logged, run: git op uninstall --local, or delete the following file.\" \"$0\"; exit 0; }; git-op reference-transaction \"$@\"\n"
    };
}

const HOOK_LINE: &str = hook_line!();

const HOOK_BODY: &str = concat!("#!/bin/sh\n", hook_line!());

/// Install the `reference-transaction` hook in this repository.
///
/// The git-op block is rewritten in place, whether written by an older
/// version of git-op or merged into a hook of your own. A hook that does not
/// invoke git-op is never overwritten.
///
/// # Examples
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!(
///     "git-op-install-local-{}-{unique}",
///     std::process::id()
/// ));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// git_op::install_local(&repo).expect("install reference-transaction hook");
/// let hook = repo.git_dir().join("hooks/reference-transaction");
/// let body = std::fs::read_to_string(hook).expect("read installed hook");
/// assert!(body.contains("git-op reference-transaction"));
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn install_local(repo: &gix::Repository) -> Result<(), Error> {
    install_hook(git_path(repo, "hooks")?)
}

/// Install the hook in Git's configured global template directory.
///
/// When `init.templateDir` is already configured, the hook is installed there
/// and the directory gains any stock template files it is missing, so
/// repositories initialized from it keep the files a default `git init`
/// provides. Otherwise git-op claims a default template directory by writing
/// `init.templateDir` into its own global config file and referencing that
/// file from an `[include]` block appended to the global configuration;
/// existing settings are never rewritten. The global configuration and
/// template directory are process-wide, so prefer
/// [`install_local`](crate::install_local) when isolation matters.
pub fn install_global() -> Result<(), Error> {
    let template = resolve_global_template()?;
    seed_stock_templates(&template)?;
    install_hook(template.join("hooks"))
}

/// Resolve the global template directory, claiming a default one through an
/// `[include]`d git-op config when Git has none configured.
///
/// This is intentionally private because resolving or initializing the global
/// template path is an implementation detail of [`install_global`].
fn resolve_global_template() -> Result<PathBuf, Error> {
    if let Some(template) = git_config_global_template()? {
        return Ok(template);
    }

    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .ok_or_else(|| Error::message("neither XDG_CONFIG_HOME nor HOME is set"))?;
    let template = default_template_dir(&config_home);
    let op_config = config_home.join("git").join(OP_CONFIG_NAME);
    write_op_config(&op_config, &template)?;
    ensure_include(&["--global"], &op_config)?;
    Ok(template)
}

/// The default global template directory git-op claims when Git has none.
fn default_template_dir(config_home: &Path) -> PathBuf {
    config_home.join("git/templates")
}

/// The name of the git-op-owned global config file holding `init.templateDir`.
const OP_CONFIG_NAME: &str = "op-config";

/// Write `template` as the `init.templateDir` value of the git-op-owned
/// config file at `op_config`.
///
/// The file is written with `git config --file`, so Git handles the quoting
/// of unusual paths. A previous install's file is updated in place.
fn write_op_config(op_config: &Path, template: &Path) -> Result<(), Error> {
    if let Some(parent) = op_config.parent() {
        fs::create_dir_all(parent).map_err(Error::git)?;
    }
    let status = Command::new("git")
        .args(["config", "--file"])
        .arg(op_config)
        .args(["init.templateDir"])
        .arg(template)
        .status()
        .map_err(Error::git)?;
    if !status.success() {
        return Err(Error::message(format!(
            "git config --file {} failed with {status}",
            op_config.display()
        )));
    }
    Ok(())
}

/// Append an `[include] path` entry for `op_config` to the config selected by
/// `scope` unless it already references it, reporting whether an entry was added.
///
/// `scope` selects the configuration: `["--global"]` for the user's global
/// config, or `["--file", path]` for tests. Existing content is never
/// rewritten; git appends the entry, creating the file when absent.
fn ensure_include(scope: &[&str], op_config: &Path) -> Result<bool, Error> {
    let present = Command::new("git")
        .arg("config")
        .args(scope)
        .args(["--get-all", "--fixed-value", "include.path"])
        .arg(op_config)
        .status()
        .map_err(Error::git)?
        .success();
    if present {
        return Ok(false);
    }
    let status = Command::new("git")
        .arg("config")
        .args(scope)
        .arg("include.path")
        .arg(op_config)
        .status()
        .map_err(Error::git)?;
    if !status.success() {
        return Err(Error::message(format!(
            "git config include.path failed with {status}"
        )));
    }
    Ok(true)
}

/// Copy Git's stock template files into `template`, never overwriting.
///
/// A template directory carrying only git-op's hook would otherwise make
/// `git init` produce repositories missing the stock `description`,
/// `info/exclude`, and sample hooks. Files already present are left
/// untouched, so reinstalls never clobber customizations.
fn seed_stock_templates(template: &Path) -> Result<(), Error> {
    fs::create_dir_all(template.join("hooks")).map_err(Error::git)?;
    let Some(stock) = stock_template_dir() else {
        return Ok(());
    };
    copy_absent(&stock, template)
}

/// Locate Git's installed stock template directory, if present.
///
/// Stock templates live at `<prefix>/share/git-core/templates`, two levels
/// above the `<prefix>/libexec/git-core` directory `git --exec-path` reports.
/// An unusual installation layout yields `None`, and installing proceeds
/// without stock content rather than failing.
fn stock_template_dir() -> Option<PathBuf> {
    let output = Command::new("git").arg("--exec-path").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let exec_path = PathBuf::from(std::str::from_utf8(&output.stdout).ok()?.trim());
    let stock = exec_path
        .parent()?
        .parent()?
        .join("share/git-core/templates");
    stock.is_dir().then_some(stock)
}

/// Copy `source` into `target`, creating directories and skipping existing files.
fn copy_absent(source: &Path, target: &Path) -> Result<(), Error> {
    if source.is_dir() {
        fs::create_dir_all(target).map_err(Error::git)?;
        for entry in fs::read_dir(source).map_err(Error::git)? {
            let entry = entry.map_err(Error::git)?;
            copy_absent(&entry.path(), &target.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if !target.exists() {
        fs::copy(source, target).map_err(Error::git)?;
    }
    Ok(())
}

/// Resolve a path using Git's repository-aware path rules.
///
/// Using `git rev-parse --git-path` preserves Git's behavior for alternate
/// object layouts and configured hook paths instead of assuming a `.git`
/// directory below the worktree.
fn git_path(repo: &gix::Repository, name: &str) -> Result<PathBuf, Error> {
    let output = Command::new("git")
        .current_dir(repo.workdir().unwrap_or(repo.git_dir()))
        .env("GIT_DIR", repo.git_dir())
        .args(["rev-parse", "--path-format=absolute", "--git-path", name])
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Err(Error::message(format!(
            "git rev-parse --git-path {name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let path = std::str::from_utf8(&output.stdout)
        .map_err(|error| Error::message(error.to_string()))?
        .trim();
    Ok(PathBuf::from(path))
}

/// Return whether Git is configured to sign commits in this repository.
///
/// An unset `commit.gpgSign` setting is treated as disabled. Invalid boolean
/// values and configuration-command failures are returned as errors.
pub(crate) fn commit_signing_enabled(repo: &gix::Repository) -> Result<bool, Error> {
    let output = Command::new("git")
        .current_dir(repo.current_dir())
        .args(["config", "--bool", "--get", "commit.gpgSign"])
        .output()
        .map_err(Error::git)?;
    if output.status.success() {
        return match std::str::from_utf8(&output.stdout)
            .map_err(|error| Error::message(error.to_string()))?
            .trim()
        {
            "true" => Ok(true),
            "false" => Ok(false),
            value => Err(Error::message(format!(
                "git config returned invalid commit.gpgSign value {value:?}"
            ))),
        };
    }
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    Err(Error::message(format!(
        "git config --bool --get commit.gpgSign failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Read the global template directory configured for Git.
///
/// Git's exit status distinguishes an absent setting (`None`) from a command
/// failure (`Err`). `--includes` is required because `git config` skips
/// include directives when a specific file such as the global config is
/// selected, and git-op's own `init.templateDir` is set through one.
fn git_config_global_template() -> Result<Option<PathBuf>, Error> {
    let output = Command::new("git")
        .args([
            "config",
            "--global",
            "--path",
            "--includes",
            "--get",
            "init.templateDir",
        ])
        .output()
        .map_err(Error::git)?;
    if output.status.success() {
        let value = std::str::from_utf8(&output.stdout)
            .map_err(|error| Error::message(error.to_string()))?
            .trim();
        return Ok((!value.is_empty()).then(|| PathBuf::from(value)));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(Error::message(format!(
        "git config --global --get init.templateDir failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Return whether `line` is a non-comment line owned by git-op.
fn invokes_git_op(line: &str) -> bool {
    let line = line.trim();
    !line.starts_with('#')
        && (line.contains("git-op")
            || line.contains("git op reference-transaction")
            || line.contains("git op \"$hook\""))
}

/// Rewrite git-op's block in `existing`, or `None` if no owned line exists.
///
/// [`HOOK_LINE`] replaces the first owned line and all further owned lines are
/// dropped; every other line, including its terminator, is preserved byte for
/// byte.
fn rewrite_managed_line(existing: &str) -> Option<String> {
    let mut rewritten = String::with_capacity(existing.len() + HOOK_LINE.len());
    let mut spliced = false;
    for line in existing.split_inclusive('\n') {
        if invokes_git_op(line.trim_end_matches(['\n', '\r'])) {
            if !spliced {
                rewritten.push_str(HOOK_LINE);
                spliced = true;
            }
        } else {
            rewritten.push_str(line);
        }
    }
    spliced.then_some(rewritten)
}

/// Bring an existing hook up to date with the current [`HOOK_LINE`].
///
/// A hook already carrying that line is left untouched; one that invokes no
/// git-op hook, or that is not valid UTF-8 to scan, is refused.
fn upgrade_hook(hooks: &Path, path: &Path, existing: &[u8]) -> Result<(), Error> {
    let existing =
        std::str::from_utf8(existing).map_err(|_| Error::HookExists(path.to_path_buf()))?;
    let rewritten =
        rewrite_managed_line(existing).ok_or_else(|| Error::HookExists(path.to_path_buf()))?;
    if rewritten == existing {
        return Ok(());
    }
    replace_hook(hooks, path, &rewritten)
}

/// Install the hook without replacing an unrelated existing hook.
///
/// The create-new open protects the fresh-install path from replacing a hook
/// created concurrently by another process. Upgrading an existing hook instead
/// writes the new body to a sibling temporary file and renames it into place,
/// so a process dying mid-write can never leave a truncated hook for Git to
/// execute.
fn install_hook(hooks: impl AsRef<Path>) -> Result<(), Error> {
    let hooks = hooks.as_ref();
    fs::create_dir_all(hooks).map_err(Error::git)?;
    let path = hooks.join(HOOK_NAME);
    match fs::read(&path) {
        Ok(existing) => return upgrade_hook(hooks, &path, &existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::git(error)),
    }
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::HookExists(path));
        }
        Err(error) => return Err(Error::git(error)),
    };
    file.write_all(HOOK_BODY.as_bytes()).map_err(Error::git)?;
    file.sync_all().map_err(Error::git)?;
    drop(file);
    make_executable(&path)
}

/// Atomically replace `path` with `body`.
///
/// The body is written to a uniquely-named temporary file in `hooks` first,
/// then renamed over `path`, so the live hook is never truncated in place.
fn replace_hook(hooks: &Path, path: &Path, body: &str) -> Result<(), Error> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| Error::message(error.to_string()))?
        .as_nanos();
    let tmp_path = hooks.join(format!(".{HOOK_NAME}.tmp-{}-{unique}", std::process::id()));
    write_tmp_hook(&tmp_path, path, body).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })
}

/// Write `body` to `tmp_path` and rename it over `path`.
fn write_tmp_hook(tmp_path: &Path, path: &Path, body: &str) -> Result<(), Error> {
    let mut file = fs::File::create_new(tmp_path).map_err(Error::git)?;
    file.write_all(body.as_bytes()).map_err(Error::git)?;
    file.sync_all().map_err(Error::git)?;
    drop(file);
    make_executable(tmp_path)?;
    fs::rename(tmp_path, path).map_err(Error::git)
}

/// Ensure the hook has executable permissions on supported platforms.
///
/// On non-Unix platforms the file is already executable according to the
/// platform's normal file semantics, so this function is a no-op.
fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).map_err(Error::git)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(Error::git)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
            let _ = fs::remove_dir_all(&self.path);
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

    /// Verify that a genuinely foreign hook is refused and left untouched.
    #[test]
    fn install_over_foreign_hook_is_refused() {
        let hooks = TemporaryHooksDir::new();
        let foreign = "#!/bin/sh\necho something-else\n";
        fs::write(hooks.hook_path(), foreign).expect("write foreign hook");
        let error = install_hook(&hooks.path).expect_err("foreign hook must be refused");
        assert!(matches!(error, Error::HookExists(_)));
        let body = fs::read(hooks.hook_path()).expect("read hook after refused install");
        assert_eq!(body, foreign.as_bytes());
    }

    /// Verify that a missing git-op binary fails open: the hook exits 0 so ref
    /// updates proceed, warning only on the committed phase.
    #[cfg(unix)]
    #[test]
    fn missing_binary_fails_open_with_warning_only_on_committed_phase() {
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
            assert_eq!(code, Some(0), "{phase} phase must fail open");
            assert_eq!(stderr, "", "{phase} phase must stay silent");
        }
        let (code, stderr) = run("committed");
        assert_eq!(code, Some(0), "committed phase must fail open");
        assert!(
            stderr.contains("could not be found"),
            "committed phase should warn: {stderr}"
        );
        assert!(
            stderr.contains("git op uninstall --local"),
            "warning should mention uninstalling: {stderr}"
        );
    }

    /// Create a unique temporary directory and remove it when the returned
    /// guard drops.
    struct TemporaryDir {
        path: PathBuf,
    }

    impl TemporaryDir {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMP_HOOKS_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("git-op-{label}-{}-{sequence}", std::process::id()));
            fs::create_dir(&path).expect("create temporary directory");
            Self { path }
        }
    }

    impl Drop for TemporaryDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Verify that stock template content fills a template directory without
    /// overwriting files that are already there, and that unrelated files
    /// survive.
    #[test]
    fn copy_absent_populates_missing_files_and_keeps_existing_ones() {
        let root = TemporaryDir::new("copy-absent");
        let stock = root.path.join("stock");
        fs::create_dir_all(stock.join("hooks")).expect("create stock hooks");
        fs::create_dir_all(stock.join("info")).expect("create stock info");
        fs::write(stock.join("description"), b"stock description\n").expect("write description");
        fs::write(stock.join("info/exclude"), b"stock exclude\n").expect("write exclude");
        fs::write(stock.join("hooks/pre-commit.sample"), b"sample").expect("write sample");

        let template = root.path.join("template");
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
        let root = TemporaryDir::new("include");
        let template = root.path.join("git/templates");
        let op_config = root.path.join("git/op-config");
        let config = root.path.join("gitconfig");

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
}
