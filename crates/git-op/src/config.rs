//! Git configuration, paths, and hook installation.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use crate::Error;

const HOOK_NAME: &str = "reference-transaction";
const HOOK_BODY: &str = "#!/bin/sh\nexec git-op reference-transaction \"$@\"\n";

/// Install the `reference-transaction` hook in this repository.
///
/// Existing hooks created by `git-op` are updated idempotently. An unrelated
/// hook is never overwritten.
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

/// Install the hook without replacing an unrelated existing hook.
///
/// The create-new open protects the final installation step from replacing a
/// hook created concurrently by another process.
fn install_hook(hooks: impl AsRef<Path>) -> Result<(), Error> {
    let hooks = hooks.as_ref();
    fs::create_dir_all(hooks).map_err(Error::git)?;
    let path = hooks.join(HOOK_NAME);
    match fs::read(&path) {
        Ok(existing) if existing == HOOK_BODY.as_bytes() => {
            make_executable(&path)?;
            return Ok(());
        }
        Ok(_) => return Err(Error::HookExists(path)),
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
