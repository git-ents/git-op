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
        Command::Restore { oid } => restore(&oid),
        Command::Undo { oid } => undo(oid.as_deref()),
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
    git_op::reference_transaction(&repo, phase, &input)?;
    Ok(())
}

/// Install the hook in the current repository or Git's global template.
fn install(local: bool) -> Result<(), Box<dyn std::error::Error>> {
    if local {
        let repo = open_repository()?;
        git_op::install_local(&repo)?;
    } else {
        git_op::install_global()?;
    }
    Ok(())
}

/// Restore one operation-log commit into the current repository.
fn restore(oid: &str) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let oid = git_op::resolve_operation(&repo, oid)?;
    git_op::restore(&repo, oid)?;
    Ok(())
}

/// Restore the state before an operation-log commit.
fn undo(specification: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let oid = specification
        .map(|specification| git_op::resolve_operation(&repo, specification))
        .transpose()?;
    git_op::undo_at(&repo, oid)?;
    Ok(())
}
