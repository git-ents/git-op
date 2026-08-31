//! Implementations of `git-op` command-line operations.

use std::io::{self, IsTerminal, Read, Write};

use git_op::ReferenceTransactionPhase;

use crate::cli::Command;

/// Execute the selected command-line operation.
pub(crate) fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::ReferenceTransaction { phase } => reference_transaction(&phase),
        Command::Install { local } => install(local),
        Command::Log {
            max_count,
            reverse,
            verbose,
            no_pager,
            oneline,
            json,
        } => crate::log::run(max_count, reverse, verbose, no_pager, oneline, json),
        Command::Restore { oid, dry_run } => restore_command(oid.as_deref(), dry_run),
        Command::Undo { dry_run } => undo_command(dry_run),
        Command::Redo { dry_run } => redo_command(dry_run),
    }
}

fn ensure_clean(repo: &gix::Repository) -> Result<(), Box<dyn std::error::Error>> {
    let Some(workdir) = repo.workdir() else {
        return Ok(());
    };
    let output = std::process::Command::new("git")
        .current_dir(workdir)
        .env("GIT_DIR", repo.git_dir())
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .output()?;
    if !output.status.success() {
        return Err(format!("git status failed with {}", output.status).into());
    }
    if output.stdout.is_empty() {
        Ok(())
    } else {
        Err("working tree is dirty; commit or restore before changing repository state".into())
    }
}

/// Open the repository Git selected for this invocation.
///
/// Discovery honors `GIT_DIR` and related environment variables Git sets
/// when it launches `git-op` (directly or as a hook), rather than always
/// resolving from the process's current directory. Without this, a hook
/// invoked with `PWD` in one repository and `GIT_DIR` pointing at another
/// (as happens during `git clone`) would silently operate on the wrong
/// repository.
#[allow(clippy::result_large_err)]
pub(crate) fn open_repository() -> Result<gix::Repository, gix::discover::Error> {
    gix::discover_with_environment_overrides(".")
}

/// Process a reference-transaction hook invocation from standard input.
///
/// An undiscoverable repository, as during `git init`, is a clean no-op.
fn reference_transaction(phase: &str) -> Result<(), Box<dyn std::error::Error>> {
    let phase = ReferenceTransactionPhase::try_from(phase)?;
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let Ok(repo) = open_repository() else {
        return Ok(());
    };
    git_op::reference_transaction(&repo, phase, &input)?;
    Ok(())
}

/// Install the hook in the current repository or Git's global template.
fn install(local: bool) -> Result<(), Box<dyn std::error::Error>> {
    if local {
        let repo = open_repository()?;
        ensure_clean(&repo)?;
        git_op::install_local(&repo)?;
    } else {
        if let Ok(repo) = open_repository() {
            ensure_clean(&repo)?;
        }
        git_op::install_global()?;
    }
    Ok(())
}

fn restore_command(
    specification: Option<&str>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let specification = match specification {
        Some(specification) => specification.to_owned(),
        None if io::stdin().is_terminal() => select_restore_operation(&repo)?,
        None => return Err("restore requires an operation in non-interactive mode".into()),
    };
    let oid = git_op::resolve_operation(&repo, &specification)?;
    if dry_run {
        println!("Would restore to operation {}", short(&oid));
        return Ok(());
    }
    ensure_clean(&repo)?;
    let result = git_op::restore_action(&repo, oid)?;
    print_action_result("Restored to operation", &result);
    Ok(())
}

fn select_restore_operation(repo: &gix::Repository) -> Result<String, Box<dyn std::error::Error>> {
    let Some(reference) = repo.try_find_reference(git_op::OP_REF)? else {
        return Err("no operation snapshots recorded".into());
    };
    let tip = reference
        .target()
        .try_id()
        .ok_or(git_op::Error::InvalidOperationRef)?
        .to_owned();
    let commit = repo.find_commit(tip)?;
    println!("Restore repository to the state after this operation:");
    println!("{}  {}", tip, commit.message()?.summary());
    print!("Operation ID (empty selects latest): ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    Ok(if input.is_empty() {
        tip.to_string()
    } else {
        input.to_owned()
    })
}

fn undo_command(dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let plan = git_op::plan_undo(&repo)?;
    if dry_run {
        println!("Would undo operation {}", short(&plan.target));
        return Ok(());
    }
    ensure_clean(&repo)?;
    let result = git_op::undo_action(&repo)?;
    print_action_result("Undid operation", &result);
    Ok(())
}

fn redo_command(dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let plan = git_op::plan_redo(&repo)?;
    if dry_run {
        println!("Would redo operation {}", short(&plan.target));
        return Ok(());
    }
    ensure_clean(&repo)?;
    let result = git_op::redo_action(&repo)?;
    print_action_result("Redid operation", &result);
    Ok(())
}

fn print_action_result(prefix: &str, result: &git_op::ActionResult) {
    println!("{}", action_summary(prefix, result));
}

fn action_summary(prefix: &str, result: &git_op::ActionResult) -> String {
    let mut summary = format!("{prefix} {}", short(&result.restored));
    if !result.changed {
        summary.push_str("; no updates");
        return summary;
    }
    for change in result
        .changes
        .iter()
        .find_map(|change| match change {
            git_op::Changes::Refs(refs) => Some(refs),
            _ => None,
        })
        .into_iter()
        .flatten()
    {
        let target = match &change.kind {
            git_op::RefChangeKind::Created(target)
            | git_op::RefChangeKind::Updated { new: target, .. } => target_summary(target),
            git_op::RefChangeKind::Deleted(_) => "deleted".to_owned(),
        };
        summary.push_str(&format!("\n{} -> {target}", change.name));
    }
    for name in git_op::Changes::file_names(&result.changes) {
        summary.push_str(&format!("\n{name} updated"));
    }
    summary
}

fn target_summary(target: &gix::refs::Target) -> String {
    match target {
        gix::refs::Target::Object(oid) => short(oid),
        gix::refs::Target::Symbolic(name) => format!("ref: {name}"),
    }
}

fn short(oid: &gix::ObjectId) -> String {
    oid.to_string().chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{action_summary, ensure_clean};

    fn repository() -> (gix::Repository, std::path::PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("git-op-exe-{}-{unique}", std::process::id()));
        let repo = gix::init(&path).expect("initialize repository");
        (repo, path)
    }

    #[test]
    fn action_summary_lists_ref_updates() {
        let old = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .expect("parse old object ID");
        let new = gix::ObjectId::from_hex(b"2222222222222222222222222222222222222222")
            .expect("parse new object ID");
        let result = git_op::ActionResult {
            operation: old,
            target: old,
            restored: new,
            changes: vec![git_op::Changes::Refs(vec![git_op::RefChange {
                name: "refs/heads/main".to_owned(),
                kind: git_op::RefChangeKind::Updated {
                    old: gix::refs::Target::Object(old),
                    new: gix::refs::Target::Object(new),
                },
            }])],
            changed: true,
        };
        assert_eq!(
            action_summary("Restored to operation", &result),
            "Restored to operation 222222222222\nrefs/heads/main -> 222222222222"
        );
    }

    #[test]
    fn action_summary_reports_noop() {
        let oid = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .expect("parse object ID");
        let result = git_op::ActionResult {
            operation: oid,
            target: oid,
            restored: oid,
            changes: Vec::new(),
            changed: false,
        };
        assert_eq!(
            action_summary("Restored to operation", &result),
            "Restored to operation 111111111111; no updates"
        );
    }

    #[test]
    fn clean_worktree_is_allowed() {
        let (repo, path) = repository();
        ensure_clean(&repo).expect("clean worktree should be allowed");
        fs::remove_dir_all(path).expect("remove temporary repository");
    }

    #[test]
    fn dirty_worktree_is_rejected() {
        let (repo, path) = repository();
        fs::write(path.join("untracked"), b"change").expect("write untracked file");
        let error = ensure_clean(&repo).expect_err("dirty worktree should be rejected");
        assert_eq!(
            error.to_string(),
            "working tree is dirty; commit or restore before changing repository state"
        );
        fs::remove_dir_all(path).expect("remove temporary repository");
    }
}
