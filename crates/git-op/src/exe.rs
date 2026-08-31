//! Implementations of `git-op` command-line operations.

use std::io::{self, Read};

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
        Command::Undo { oid, dry_run } => undo_command(oid.as_deref(), dry_run),
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
/// Git can invoke this hook while `git init` is still creating the
/// repository, before there is anything to discover; that is treated as a
/// clean no-op rather than an error.
fn reference_transaction(phase: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let Ok(repo) = open_repository() else {
        return Ok(());
    };
    if phase == "committed" {
        ensure_clean(&repo)?;
    }
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
    let Some(specification) = specification else {
        return Err(
            "restore requires an operation; interactive selection is not implemented".into(),
        );
    };
    let oid = git_op::resolve_operation(&repo, specification)?;
    if dry_run {
        println!("Would restore to operation {}", short(&oid));
        return Ok(());
    }
    ensure_clean(&repo)?;
    let result = git_op::restore_action(&repo, oid)?;
    println!("Restored to operation {}", short(&result.restored));
    Ok(())
}

fn undo_command(
    specification: Option<&str>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if specification.is_some() {
        return Err("undo does not take an operation\nhint: use `git op restore <operation>`\nhint: reproduce the former behavior with `git op restore <operation>^`".into());
    }
    let repo = open_repository()?;
    let plan = git_op::plan_undo(&repo)?;
    if dry_run {
        println!("Would undo operation {}", short(&plan.target));
        return Ok(());
    }
    ensure_clean(&repo)?;
    let result = git_op::undo_action(&repo)?;
    println!("Undid operation {}", short(&result.target));
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
    println!("Redid operation {}", short(&result.target));
    Ok(())
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

    use super::ensure_clean;

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
