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
macro_rules! hook_line {
    () => {
        "if command -v git-op >/dev/null 2>&1; then git-op reference-transaction \"$@\"; fi\n"
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
/// This updates Git's global `init.templateDir` setting if it is not already
/// configured. The global configuration and template directory are process-wide,
/// so prefer [`install_local`](crate::install_local) when isolation matters.
pub fn install_global() -> Result<(), Error> {
    install_hook(global_template_dir()?.join("hooks"))
}

/// Return the configured global template directory, initializing the default when absent.
///
/// This is intentionally private because resolving or initializing the global
/// template path is an implementation detail of [`install_global`].
fn global_template_dir() -> Result<PathBuf, Error> {
    if let Some(template) = git_config_global_template()? {
        return Ok(template);
    }

    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .ok_or_else(|| Error::message("neither XDG_CONFIG_HOME nor HOME is set"))?;
    let template = config_home.join("git/templates");
    let status = Command::new("git")
        .args(["config", "--global", "init.templateDir"])
        .arg(&template)
        .status()
        .map_err(Error::git)?;
    if !status.success() {
        return Err(Error::message(format!(
            "git config --global failed with {status}"
        )));
    }
    Ok(template)
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
/// failure (`Err`).
fn git_config_global_template() -> Result<Option<PathBuf>, Error> {
    let output = Command::new("git")
        .args(["config", "--global", "--path", "--get", "init.templateDir"])
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
        && (line.contains("git-op") || line.contains("git op reference-transaction"))
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
}
