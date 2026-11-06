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

/// Hook line written by fresh installs and hook upgrades of the `pre-push`
/// guard.
///
/// When the `git-op` executable is missing the hook exits 0, so a lost guard
/// never blocks a push.
///
/// `git-op pre-push` exits 1 only to reject a push; any other status means it
/// could not judge, as when the `git-op` found on `PATH` predates the
/// subcommand and exits with a usage error. Blocking only on status 1 keeps a
/// stale binary from failing every push.
///
/// Only a rejection ends the hook, so a hook whose owner pasted this line in,
/// as the install error message instructs, still runs its own commands.
macro_rules! pre_push_line {
    () => {
        "command -v git-op >/dev/null 2>&1 || exit 0; git-op pre-push \"$@\"; git_op_status=$?; if [ \"$git_op_status\" -eq 1 ]; then exit 1; fi\n"
    };
}

const HOOK_LINE: &str = hook_line!();
const PRE_PUSH_LINE: &str = pre_push_line!();

const HOOK_BODY: &str = concat!("#!/bin/sh\n", hook_line!());
const PRE_PUSH_BODY: &str = concat!("#!/bin/sh\n", pre_push_line!());

/// Complete `reference-transaction` bodies an earlier git-op version wrote
/// whose command line this version does not otherwise recognize.
///
/// A file matching one of these byte for byte is still owned, so it upgrades
/// and uninstalls in place. The commands are not matched line by line, because
/// a bare `op` is too generic a name to claim: only a body git-op actually
/// wrote is recognized, never an unrelated hook that happens to run something
/// named `op` with the same argument.
const LEGACY_REFERENCE_TRANSACTION_BODIES: &[&str] =
    &["#!/bin/sh\nexec op reference-transaction \"$@\"\n"];

/// A hook git-op installs and owns.
#[derive(Clone, Copy)]
enum ManagedHook {
    /// Records a snapshot for every reference transaction.
    ReferenceTransaction,
    /// Refuses pushes of the operation ref.
    PrePush,
}

impl ManagedHook {
    /// Every hook git-op installs.
    const ALL: [Self; 2] = [Self::ReferenceTransaction, Self::PrePush];

    /// The file name Git looks for.
    fn name(self) -> &'static str {
        match self {
            Self::ReferenceTransaction => REFERENCE_TRANSACTION_HOOK_NAME,
            Self::PrePush => PRE_PUSH_HOOK_NAME,
        }
    }

    /// The complete body a fresh install writes.
    fn body(self) -> &'static str {
        match self {
            Self::ReferenceTransaction => HOOK_BODY,
            Self::PrePush => PRE_PUSH_BODY,
        }
    }

    /// The command line an upgrade rewrites in place.
    fn line(self) -> &'static str {
        match self {
            Self::ReferenceTransaction => HOOK_LINE,
            Self::PrePush => PRE_PUSH_LINE,
        }
    }

    /// Whether `line` invokes this hook's git-op command in a spelling git-op
    /// has shipped, so hooks from earlier versions upgrade and uninstall in
    /// place.
    fn owns_line(self, line: &str) -> bool {
        match self {
            Self::ReferenceTransaction => is_git_op_line(line),
            Self::PrePush => is_pre_push_line(line),
        }
    }

    /// Whole-file bodies an earlier git-op version wrote, owned only when a
    /// hook matches one exactly.
    fn legacy_bodies(self) -> &'static [&'static str] {
        match self {
            Self::ReferenceTransaction => LEGACY_REFERENCE_TRANSACTION_BODIES,
            Self::PrePush => &[],
        }
    }
}

/// Install the `reference-transaction` and `pre-push` hooks in this repository.
///
/// An existing hook carrying a recognizable git-op command, or matching a body
/// an earlier version wrote, is upgraded in place, preserving other commands.
/// A hook git-op does not own is refused with [`Error::HookExists`] and left
/// untouched, but the hooks are installed independently, so one conflict never
/// withholds the other.
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
/// Every command git-op owns, from this version or an earlier one, is removed,
/// as is a whole body an earlier version wrote; the file is deleted only when
/// nothing of yours remains, so uninstalling is safe to repeat and never
/// touches a hook git-op did not write.
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

/// Remove both hooks from one hooks directory, each independently so that a
/// failure in one does not withhold the other. The first failure is returned.
fn uninstall_hooks(hooks: PathBuf) -> Result<(), Error> {
    let mut failure = None;
    for hook in ManagedHook::ALL {
        if let Err(error) = uninstall_managed_hook(&hooks, hook) {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
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

/// Install both hooks, each independently so that a conflict in one does not
/// withhold the other.
///
/// A hook git-op does not own is refused with [`Error::HookExists`] and left
/// untouched, and one that cannot be read as text is refused with
/// [`Error::HookUnreadable`]. The hooks are installed independently, so one
/// conflict never withholds the other; the first is returned once both have
/// been attempted.
fn install_hooks(hooks: PathBuf) -> Result<(), Error> {
    let mut conflict = None;
    for hook in ManagedHook::ALL {
        if let Err(error) = install_managed_hook(&hooks, hook) {
            conflict.get_or_insert(error);
        }
    }
    conflict.map_or(Ok(()), Err)
}

/// Publish `body` as a new hook named `name`, reporting [`AlreadyExists`] when
/// a concurrent install creates it first.
///
/// The body reaches the hook as a fully written, executable temporary file
/// hard-linked under the final name: a crash before the link can never leave a
/// truncated hook for Git to execute.
///
/// [`AlreadyExists`] is reported when the hook name is taken, and also when a
/// temporary file left by an interrupted install still occupies the temporary
/// name; the caller distinguishes them by the hook's presence.
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
        set_executable(&tmp_path)?;
        fs::hard_link(&tmp_path, hooks.join(name))?;
        fs::remove_file(&tmp_path)
    })();
    if result.is_err() {
        drop(fs::remove_file(&tmp_path));
    }
    result
}

/// Replace the first git-op-owned line with the current [`HOOK_LINE`], drop
/// any further ones, and keep every other line, terminator included, byte by
/// byte, or `None` when no owned line exists.
fn rewrite_owned_lines(existing: &str, line: &str, owned: impl Fn(&str) -> bool) -> Option<String> {
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

/// Whether `line` invokes the `reference-transaction` hook command in any
/// spelling git-op has shipped, so hooks from earlier versions upgrade and
/// uninstall in place.
fn is_git_op_line(line: &str) -> bool {
    line_runs_hook_command(line, "reference-transaction")
        || any_segment_runs(line, segment_runs_generic_hook_command)
}

/// Whether `line` invokes the `pre-push` guard command in any spelling git-op
/// has shipped.
///
/// The generic `git op "$hook"` delegation is not one of them: git-op only ever
/// installed it for the `reference-transaction` hook, so the guard never claims
/// it.
fn is_pre_push_line(line: &str) -> bool {
    line_runs_hook_command(line, "pre-push")
}

/// Whether any shell segment on `line` satisfies `predicate`.
///
/// The whole line is judged, not a text prefix: a comment line is ignored, and
/// each command a `;`, `|`, `&`, or brace separator splits off is tested on its
/// own.
fn any_segment_runs(line: &str, predicate: impl Fn(&str) -> bool) -> bool {
    let line = line.trim();
    !line.starts_with('#')
        && shell_segments(line)
            .iter()
            .any(|segment| predicate(segment))
}

/// Whether any command on `line` runs git-op's hook for `subcommand`.
///
/// Matching is by command words rather than by a text prefix, so the wrapper
/// commands, environment assignments, and absolute paths a hand-merged hook
/// may carry are recognized: `command git-op reference-transaction "$@"`,
/// `GIT_DIR=. git-op pre-push "$@"`, and `/usr/bin/git-op pre-push "$@"` all
/// count. Wrapper *arguments* other than options and assignments, as in
/// `nice -n 5 git-op …`, are not modeled.
fn line_runs_hook_command(line: &str, subcommand: &str) -> bool {
    any_segment_runs(line, |segment| {
        segment_runs_hook_command(segment, subcommand)
    })
}

/// Whether one shell segment runs git-op's hook for `subcommand`.
fn segment_runs_hook_command(segment: &str, subcommand: &str) -> bool {
    let words = shell_words(segment);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let Some((&program, arguments)) = command_words(&words).split_first() else {
        return false;
    };
    match base_name(program) {
        "git-op" => matches!(arguments, [first, ..] if *first == subcommand),
        "git" => matches!(arguments, [first, second, ..]
            if *first == "op" && *second == subcommand),
        _ => false,
    }
}

/// Whether one shell segment runs the generic delegation an earlier git-op
/// wrote for the `reference-transaction` hook, whose hook name sat in the
/// `$hook` variable: `git op "$hook" "$@"`.
///
/// Only that exact variable is claimed, so an unrelated `git op "$@"` or
/// `git op "$1"` is left alone, and only the `reference-transaction` hook
/// matches it, since that is the only hook git-op ever installed this way.
fn segment_runs_generic_hook_command(segment: &str) -> bool {
    let words = shell_words(segment);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let Some((&program, arguments)) = command_words(&words).split_first() else {
        return false;
    };
    base_name(program) == "git"
        && matches!(arguments, [first, second, ..] if *first == "op" && *second == "$hook")
}

/// Commands that run another command in place, so the command they precede is
/// still the one being run.
const COMMAND_WRAPPERS: &[&str] = &["command", "env", "exec", "nohup", "sudo"];

/// Shell reserved words that may precede the command word.
const SHELL_KEYWORDS: &[&str] = &["!", "do", "elif", "else", "if", "then", "until", "while"];

/// Drop the leading wrappers, options, and environment assignments from
/// `words`, leaving the command they run.
fn command_words<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    let mut index = 0;
    while let Some(&word) = words.get(index) {
        if COMMAND_WRAPPERS.contains(&base_name(word)) || SHELL_KEYWORDS.contains(&base_name(word))
        {
            index += 1;
            while let Some(&option) = words.get(index) {
                if option.starts_with('-') || is_assignment(option) {
                    index += 1;
                } else {
                    break;
                }
            }
        } else if is_assignment(word) {
            index += 1;
        } else {
            break;
        }
    }
    words.get(index..).unwrap_or_default()
}

/// The final path component of a command word, so `/usr/bin/git-op` and
/// `git-op` compare equal.
fn base_name(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

/// Whether `word` is a shell environment assignment such as `GIT_DIR=.`.
fn is_assignment(word: &str) -> bool {
    let Some((name, _value)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Split a shell segment into words, honoring quotes and backslash escapes and
/// discarding the quoting characters, so a command can be compared by its
/// words rather than by a prefix of the raw text.
fn shell_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = None;
    let mut escaped = false;
    for character in segment.chars() {
        if escaped {
            escaped = false;
            in_word = true;
            word.push(character);
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
            in_word = true;
        } else if quote == Some(character) {
            quote = None;
            in_word = true;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
            in_word = true;
        } else if quote.is_none() && character.is_whitespace() {
            if in_word {
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
        } else {
            in_word = true;
            word.push(character);
        }
    }
    if in_word {
        words.push(word);
    }
    words
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

/// Remove every line `hook` claims from its file, deleting the file when
/// nothing but a shebang remains. A body an earlier git-op version wrote
/// verbatim is removed whole.
///
/// A hook git-op does not own is left exactly as found, as is one that cannot
/// be read as text: git-op writes only text, so such a hook cannot be its.
fn uninstall_managed_hook(hooks: &Path, hook: ManagedHook) -> Result<(), Error> {
    let path = hooks.join(hook.name());
    let existing = match fs::read(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::git(error)),
    };
    let Ok(existing_text) = std::str::from_utf8(&existing) else {
        // git-op writes only text, so a hook that is not text cannot be its;
        // leave it exactly as found.
        return Ok(());
    };
    if hook.legacy_bodies().contains(&existing_text) {
        return fs::remove_file(path).map_err(Error::git);
    }
    let stripped: String = existing_text
        .split_inclusive('\n')
        .filter(|line| !hook.owns_line(line.trim_end_matches(['\n', '\r'])))
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
    replace_hook(hooks, hook.name(), &stripped, &existing)
}

/// Replace the hook named `name` with `body` through a temporary file renamed
/// into place, so Git only ever sees a complete hook.
///
/// The replacement is abandoned with [`Error::HookExists`] when the hook no
/// longer reads `expected`. The read, comparison, and rename are separate
/// steps, so the check narrows but cannot close the window in which a writer
/// that arrives between them is overwritten; the writers git-op can lose to
/// are other git-op installs of the same hook. An unowned hook is never a
/// target, so a hook git-op did not write is not at risk.
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
    /// Classify the hook `hook` describes without changing it, refusing one
    /// that invokes no git-op command with [`Error::HookExists`].
    fn of(hooks: &Path, hook: ManagedHook) -> Result<Self, Error> {
        let path = hooks.join(hook.name());
        let existing = match fs::read(&path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Create),
            Err(error) => return Err(Error::git(error)),
        };
        let Ok(text) = std::str::from_utf8(&existing) else {
            return Err(Error::HookUnreadable(path));
        };
        if hook.legacy_bodies().contains(&text) {
            return Ok(Self::Replace {
                contents: hook.body().to_owned(),
                expected: existing,
            });
        }
        match rewrite_owned_lines(text, hook.line(), |line| hook.owns_line(line)) {
            None => Err(Error::HookExists(path)),
            Some(contents) if contents == text => Ok(Self::Current),
            Some(contents) => Ok(Self::Replace {
                contents,
                expected: existing,
            }),
        }
    }
}

/// Install git-op's hook, upgrading one git-op already owns and refusing one
/// it does not.
fn install_managed_hook(hooks: &Path, hook: ManagedHook) -> Result<(), Error> {
    fs::create_dir_all(hooks).map_err(Error::git)?;
    match HookPlan::of(hooks, hook)? {
        HookPlan::Create => match write_new_hook(hooks, hook.name(), hook.body()) {
            // A concurrent install created the hook between the read above and
            // the write: adopt what it left rather than failing this install.
            //
            // `AlreadyExists` also reports a leftover temporary file, which
            // leaves no hook behind; requiring the hook to exist tells the two
            // apart, so a stale file is reported instead of silently skipping
            // the install.
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && hooks.join(hook.name()).exists() =>
            {
                apply_hook_plan(hooks, hook, HookPlan::of(hooks, hook)?)
            }
            Ok(()) => Ok(()),
            Err(error) => Err(Error::git(error)),
        },
        plan => apply_hook_plan(hooks, hook, plan),
    }
}

/// Bring the hook `hook` describes to the state `plan` describes.
///
/// A [`HookPlan::Create`] reaching here means a hook another install created
/// has since been removed, which leaves nothing to do.
fn apply_hook_plan(hooks: &Path, hook: ManagedHook, plan: HookPlan) -> Result<(), Error> {
    match plan {
        HookPlan::Create => Ok(()),
        HookPlan::Current => make_executable(&hooks.join(hook.name())),
        HookPlan::Replace { contents, expected } => {
            replace_hook(hooks, hook.name(), &contents, &expected)
        }
    }
}

/// Ensure the hook has executable permissions on supported platforms.
fn make_executable(path: &Path) -> Result<(), Error> {
    set_executable(path).map_err(Error::git)
}

/// Give `path` the executable permissions a hook needs.
///
/// On non-Unix platforms the file is already executable according to the
/// platform's normal file semantics, so this is a no-op.
fn set_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
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
