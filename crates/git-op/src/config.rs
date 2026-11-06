//! Git configuration, paths, and hook installation.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::Error;

const REFERENCE_TRANSACTION_HOOK_NAME: &str = "reference-transaction";
const PRE_PUSH_HOOK_NAME: &str = "pre-push";

static NEXT_TEMP_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Hook block written by fresh installs and hook upgrades.
///
/// When the `git-op` executable is missing, the hook exits 0 so a lost
/// operation log never blocks ref updates, warning only on the `committed`
/// phase so ordinary transactions stay quiet.
macro_rules! hook_line {
    () => {
        "command -v git-op >/dev/null 2>&1 || { [ \"$1\" = committed ] && printf >&2 \"\\n%s\\n\\n    %s\\n\" \"This repository has an operation log enabled via Git Op, but the 'git-op' executable could not be found, so this operation was not recorded. If you no longer wish for operations to be logged, run: git-op uninstall, or delete the following file.\" \"$0\"; exit 0; }; git-op reference-transaction \"$@\"\n"
    };
}

const HOOK_LINE: &str = hook_line!();
const PRE_PUSH_LINE: &str =
    "command -v git-op >/dev/null 2>&1 || exit 0; exec git-op pre-push \"$@\"\n";

const HOOK_BODY: &str = concat!("#!/bin/sh\n", hook_line!());
const PRE_PUSH_BODY: &str =
    "#!/bin/sh\ncommand -v git-op >/dev/null 2>&1 || exit 0; exec git-op pre-push \"$@\"\n";

/// Install the `reference-transaction` and `pre-push` hooks in this repository.
///
/// Existing hooks carrying recognizable git-op command lines are upgraded in
/// place, preserving other commands; unowned hooks are refused with
/// [`Error::HookExists`] for manual merging. Both hooks are classified before
/// either is written, so a refusal leaves no partial install behind.
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
    install_hooks(git_path(repo, "hooks")?)
}

/// Install the hook in Git's configured global template directory.
///
/// An already-configured directory gains only the hook; otherwise git-op
/// claims a default directory, seeding it with Git's stock templates, via
/// an `[include]`d config file that never rewrites existing settings. The
/// global configuration and template directory are process-wide, so prefer
/// [`install_local`](crate::install_local) when isolation matters.
pub fn install_global() -> Result<(), Error> {
    match git_config_global_template()? {
        Some(template) => install_hooks(template.join("hooks")),
        None => {
            let config_home = config_home()?;
            let template = default_template_dir(&config_home);
            let op_config = config_home.join("git").join(OP_CONFIG_NAME);
            write_op_config(&op_config, &template)?;
            ensure_include(&["--global"], &op_config)?;
            seed_stock_templates(&template)?;
            install_hooks(template.join("hooks"))
        }
    }
}

/// Resolve the global template directory, claiming a default one through an
/// `[include]`d git-op config when Git has none configured.
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

/// The XDG config home, or an error when neither it nor HOME is set.
fn config_home() -> Result<PathBuf, Error> {
    config_home_optional().ok_or_else(|| Error::message("neither XDG_CONFIG_HOME nor HOME is set"))
}

/// The XDG config home, or `None` when neither it nor HOME is set.
fn config_home_optional() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
}

/// Remove the `reference-transaction` and `pre-push` hooks from this repository.
///
/// Every line git-op owns, from this version or an earlier one, is removed;
/// the file is deleted only when nothing of yours remains, so uninstalling
/// is safe to repeat and never touches a hook git-op did not write.
pub fn uninstall_local(repo: &gix::Repository) -> Result<(), Error> {
    uninstall_hooks(git_path(repo, "hooks")?)
}

/// Remove the hook from Git's global template directory and every template
/// directory, config file, and `[include]` entry git-op installed.
///
/// Only directories git-op claimed are deleted; a user-configured one keeps
/// everything except git-op's own hook. The global configuration and
/// template directory are process-wide, so prefer
/// [`uninstall_local`](crate::uninstall_local) when isolation matters.
pub fn uninstall_global() -> Result<(), Error> {
    if let Some(template) = git_config_global_template()? {
        uninstall_hooks(template.join("hooks"))?;
    }
    if let Some(config_home) = config_home_optional() {
        if let Some(claimed) = claimed_template(&config_home)? {
            uninstall_hooks(claimed.join("hooks"))?;
            remove_unmodified_template(&claimed)?;
        }
        remove_global_claim(&config_home)?;
    }
    Ok(())
}

/// The template directory git-op's own config file claims, if it exists.
fn claimed_template(config_home: &Path) -> Result<Option<PathBuf>, Error> {
    let op_config = config_home.join("git").join(OP_CONFIG_NAME);
    if !op_config.is_file() {
        return Ok(None);
    }
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(&op_config)
        .args(["--get", "init.templateDir"])
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    )))
}

/// Remove both hooks from one hooks directory.
fn uninstall_hooks(hooks: PathBuf) -> Result<(), Error> {
    uninstall_reference_transaction_hook(&hooks)?;
    uninstall_pre_push_hook(&hooks)
}

/// Remove git-op's `reference-transaction` hook from `hooks`.
fn uninstall_reference_transaction_hook(hooks: &Path) -> Result<(), Error> {
    uninstall_managed_hook(hooks, REFERENCE_TRANSACTION_HOOK_NAME, is_git_op_line)
}

/// Remove the claimed template directory when git-op's install created it.
///
/// The directory goes away only once its hook is gone and no file outside
/// git-op's and Git's stock set is present; anything else means the
/// directory holds content git-op did not put there.
fn remove_unmodified_template(template: &Path) -> Result<(), Error> {
    let hook = template.join("hooks");
    if hook.join(REFERENCE_TRANSACTION_HOOK_NAME).exists()
        || hook.join(PRE_PUSH_HOOK_NAME).exists()
        || !template_is_unmodified(template)
    {
        return Ok(());
    }
    match fs::remove_dir_all(template) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::git(error)),
    }
}

/// Whether every file under `template` is one git-op's install could have
/// written: the stock `description`, `info/exclude`, sample hooks, or
/// git-op's own hook.
fn template_is_unmodified(template: &Path) -> bool {
    fn walk(dir: &Path, prefix: &str) -> bool {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            let relative = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            if path.is_dir() {
                if !walk(&path, &relative) {
                    return false;
                }
            } else if relative != "description"
                && relative != "info/exclude"
                && relative != format!("hooks/{REFERENCE_TRANSACTION_HOOK_NAME}")
                && relative != format!("hooks/{PRE_PUSH_HOOK_NAME}")
                && !(relative.starts_with("hooks/") && name.ends_with(".sample"))
            {
                return false;
            }
        }
        true
    }
    walk(template, "")
}

/// Remove the `[include]` entry and config file git-op added to claim the
/// default template directory, when present.
///
/// The include entry is unset first so Git never observes a dangling include.
/// Exit status 5 means no matching entry remains, which is the desired state.
fn remove_global_claim(config_home: &Path) -> Result<(), Error> {
    let op_config = config_home.join("git").join(OP_CONFIG_NAME);
    let status = Command::new("git")
        .args([
            "config",
            "--global",
            "--unset-all",
            "--fixed-value",
            "include.path",
        ])
        .arg(&op_config)
        .status()
        .map_err(Error::git)?;
    if !status.success() && status.code() != Some(5) {
        return Err(Error::message(format!(
            "git config --unset-all include.path failed with {status}"
        )));
    }
    match fs::remove_file(op_config) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::git(error)),
    }
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

/// Install both hooks, classifying each first so a hook git-op does not own
/// is refused before either hook is written.
fn install_hooks(hooks: PathBuf) -> Result<(), Error> {
    ensure_installable(
        &hooks,
        REFERENCE_TRANSACTION_HOOK_NAME,
        HOOK_LINE,
        is_git_op_line,
    )?;
    ensure_installable(&hooks, PRE_PUSH_HOOK_NAME, PRE_PUSH_LINE, is_pre_push_line)?;
    install_reference_transaction_hook(&hooks)?;
    install_pre_push_hook(&hooks)
}

/// Refuse an install for a hook git-op does not own, naming the hook's path,
/// without changing anything on disk.
fn ensure_installable(
    hooks: &Path,
    name: &str,
    line: &str,
    owned: fn(&str) -> bool,
) -> Result<(), Error> {
    HookPlan::of(hooks, name, line, owned).map(drop)
}

fn install_pre_push_hook(hooks: &Path) -> Result<(), Error> {
    install_managed_hook(
        hooks,
        PRE_PUSH_HOOK_NAME,
        PRE_PUSH_BODY,
        PRE_PUSH_LINE,
        is_pre_push_line,
    )
}

fn uninstall_pre_push_hook(hooks: &Path) -> Result<(), Error> {
    uninstall_managed_hook(hooks, PRE_PUSH_HOOK_NAME, is_pre_push_line)
}

/// Install or upgrade the `reference-transaction` hook in `hooks`.
fn install_reference_transaction_hook(hooks: &Path) -> Result<(), Error> {
    install_managed_hook(
        hooks,
        REFERENCE_TRANSACTION_HOOK_NAME,
        HOOK_BODY,
        HOOK_LINE,
        is_git_op_line,
    )
}

/// Publish `body` as a new hook named `name`, reporting [`AlreadyExists`] when
/// a concurrent install creates it first.
///
/// The body reaches the hook as a fully written, executable temporary file
/// hard-linked under the final name: a crash before the link can never leave a
/// truncated hook for Git to execute.
///
/// [`AlreadyExists`]: std::io::ErrorKind::AlreadyExists
fn write_new_hook(hooks: &Path, name: &str, body: &str) -> std::io::Result<()> {
    let sequence = NEXT_TEMP_HOOK.fetch_add(1, Ordering::Relaxed);
    let tmp_path = hooks.join(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create_new(&tmp_path)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        drop(file);
        make_executable_io(&tmp_path)?;
        fs::hard_link(&tmp_path, hooks.join(name))?;
        fs::remove_file(&tmp_path)
    })();
    if result.is_err() {
        drop(fs::remove_file(&tmp_path));
    }
    result
}

fn make_executable_io(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

/// Replace the first git-op-owned line with the current [`HOOK_LINE`], drop
/// any further ones, and keep every other line, terminator included, byte by
/// byte, or `None` when no owned line exists.
fn rewrite_owned_lines(existing: &str, line: &str, owned: fn(&str) -> bool) -> Option<String> {
    let mut rewritten = String::with_capacity(existing.len() + line.len());
    let mut replaced = false;
    for existing_line in existing.split_inclusive('\n') {
        if owned(existing_line.trim_end_matches(['\n', '\r'])) {
            if !replaced {
                rewritten.push_str(line);
                replaced = true;
            }
        } else {
            rewritten.push_str(existing_line);
        }
    }
    replaced.then_some(rewritten)
}

/// Whether `line` invokes a git-op hook command in any spelling git-op has
/// shipped, so hooks from earlier versions upgrade and uninstall in place.
fn is_git_op_line(line: &str) -> bool {
    is_git_command_line(line, "git-op reference-transaction")
        || is_git_command_line(line, "git op reference-transaction")
        || is_git_command_line(line, "op reference-transaction")
        || is_git_command_line(line, "git op \"$hook\"")
}

fn is_git_command_line(line: &str, command: &str) -> bool {
    let line = line.trim();
    !line.starts_with('#')
        && shell_segments(line).iter().any(|segment| {
            let segment = segment.trim();
            let segment = ["exec ", "then ", "do ", "if "]
                .into_iter()
                .find_map(|prefix| segment.strip_prefix(prefix))
                .unwrap_or(segment);
            segment == command
                || segment
                    .strip_prefix(command)
                    .is_some_and(|rest| rest.starts_with([' ', '\t']))
        })
}

fn shell_segments(line: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut segment = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            escaped = false;
            segment.push(character);
        } else if character == '\\' {
            escaped = true;
            segment.push(character);
        } else if quote == Some(character) {
            quote = None;
            segment.push(character);
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
            segment.push(character);
        } else if quote.is_none() && matches!(character, ';' | '|' | '&' | '{' | '}') {
            segments.push(std::mem::take(&mut segment));
        } else {
            segment.push(character);
        }
    }
    segments.push(segment);
    segments
}

fn is_pre_push_line(line: &str) -> bool {
    is_git_command_line(line, "git-op pre-push")
}

/// Remove every line `owned` claims from the hook at `hooks`/`name`, deleting
/// the file when nothing but a shebang remains, and leaving unscannable
/// content or a hook git-op does not own exactly as found.
fn uninstall_managed_hook(hooks: &Path, name: &str, owned: fn(&str) -> bool) -> Result<(), Error> {
    let path = hooks.join(name);
    let existing = match fs::read(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::git(error)),
    };
    let Ok(existing_text) = std::str::from_utf8(&existing) else {
        return Ok(());
    };
    let stripped: String = existing_text
        .split_inclusive('\n')
        .filter(|line| !owned(line.trim_end_matches(['\n', '\r'])))
        .collect();
    if stripped == existing_text {
        return Ok(());
    }
    if stripped
        .lines()
        .all(|line| line.trim().is_empty() || line.starts_with("#!"))
    {
        return fs::remove_file(path).map_err(Error::git);
    }
    replace_hook(hooks, name, &stripped, &existing)
}

/// Atomically replace the hook named `name` with `body` via a temporary file,
/// so the live hook is never truncated in place.
///
/// The replacement is abandoned with [`Error::HookExists`] when the hook no
/// longer reads `expected`, so a concurrent edit is never overwritten.
fn replace_hook(hooks: &Path, name: &str, body: &str, expected: &[u8]) -> Result<(), Error> {
    let path = hooks.join(name);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| Error::message(error.to_string()))?
        .as_nanos();
    let tmp_path = hooks.join(format!(".{name}.tmp-{}-{unique}", std::process::id()));
    let current = fs::read(&path).map_err(Error::git)?;
    if current != expected {
        return Err(Error::HookExists(path));
    }
    write_tmp_hook(&tmp_path, &path, body).inspect_err(|_error| {
        drop(fs::remove_file(&tmp_path));
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

/// How an install treats whatever hook already occupies the target path.
enum HookPlan {
    /// No hook yet: publish the hook body under a new name.
    Create,
    /// Already git-op's current hook: at most the executable bit needs repair.
    Current,
    /// Rewrite the hook to `contents`, provided it still reads `expected`.
    Replace { contents: String, expected: Vec<u8> },
}

impl HookPlan {
    /// Classify the hook at `hooks`/`name` without changing it, refusing one
    /// that invokes no git-op command with [`Error::HookExists`].
    fn of(hooks: &Path, name: &str, line: &str, owned: fn(&str) -> bool) -> Result<Self, Error> {
        let path = hooks.join(name);
        let existing = match fs::read(&path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Create),
            Err(error) => return Err(Error::git(error)),
        };
        let Ok(text) = std::str::from_utf8(&existing) else {
            return Err(Error::HookExists(path));
        };
        match rewrite_owned_lines(text, line, owned) {
            None => Err(Error::HookExists(path)),
            Some(contents) if contents == text => Ok(Self::Current),
            Some(contents) => Ok(Self::Replace {
                contents,
                expected: existing,
            }),
        }
    }
}

/// Install git-op's hook at `hooks`/`name`, upgrading one git-op already owns
/// and refusing one it does not.
fn install_managed_hook(
    hooks: &Path,
    name: &str,
    body: &str,
    line: &str,
    owned: fn(&str) -> bool,
) -> Result<(), Error> {
    fs::create_dir_all(hooks).map_err(Error::git)?;
    match HookPlan::of(hooks, name, line, owned)? {
        HookPlan::Create => match write_new_hook(hooks, name, body) {
            Ok(()) => Ok(()),
            // A concurrent install created the hook between the read above and
            // the write: adopt what it left rather than failing this install.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                apply_hook_plan(hooks, name, HookPlan::of(hooks, name, line, owned)?)
            }
            Err(error) => Err(Error::git(error)),
        },
        plan => apply_hook_plan(hooks, name, plan),
    }
}

/// Bring the hook at `hooks`/`name` to the state `plan` describes.
///
/// A [`HookPlan::Create`] reaching here means a hook another install created
/// has since been removed, which leaves nothing to do.
fn apply_hook_plan(hooks: &Path, name: &str, plan: HookPlan) -> Result<(), Error> {
    match plan {
        HookPlan::Create => Ok(()),
        HookPlan::Current => make_executable(&hooks.join(name)),
        HookPlan::Replace { contents, expected } => replace_hook(hooks, name, &contents, &expected),
    }
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
#[expect(
    clippy::panic,
    reason = "tests use panics and direct indexing to express failed expectations"
)]
#[path = "tests/config.rs"]
mod tests;
