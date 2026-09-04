//! Implementations of `git-op` command-line operations.

use std::io::{self, IsTerminal, Read, Write};

use git_op::ReferenceTransactionPhase;
use gix::prelude::Write as GixWrite;

use crate::cli::Command;

/// Execute the selected command-line operation.
pub(crate) fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::ReferenceTransaction { phase } => reference_transaction(&phase),
        Command::Install { global } => install(global),
        Command::Uninstall { global } => uninstall(global),
        Command::Snap => snap_command(),
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
        Command::Show { oid } => show_command(oid.as_deref()),
    }
}

/// Reject writes while the worktree is dirty.
///
/// Installing from a dirty worktree is refused so the hook reflects a
/// deliberate repository state rather than one mid-change.
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
#[expect(
    clippy::result_large_err,
    reason = "gix defines and returns this discovery error directly"
)]
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
fn install(global: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !global {
        let repo = open_repository()?;
        ensure_clean(&repo)?;
        git_op::install_local(&repo)?;
        println!("Installed git-op in this repository");
    } else {
        if let Ok(repo) = open_repository() {
            ensure_clean(&repo)?;
        }
        git_op::install_global()?;
        println!("Installed git-op globally for newly initialized repositories");
    }
    Ok(())
}

/// Remove the hook from the current repository or Git's global template.
fn uninstall(global: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !global {
        let repo = open_repository()?;
        git_op::uninstall_local(&repo)?;
        println!("Uninstalled git-op from this repository");
    } else {
        git_op::uninstall_global()?;
        println!("Uninstalled git-op globally");
    }
    Ok(())
}

/// Record any repository state not yet on the operation log, reporting the
/// outcome of the request.
fn snap_command() -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    println!("{}", snap_outcome_summary(&git_op::snap(&repo)?));
    Ok(())
}

/// Describe a snap outcome.
fn snap_outcome_summary(outcome: &git_op::SnapOutcome) -> String {
    match outcome {
        git_op::SnapOutcome::Recorded(snapped) => snap_summary(*snapped),
        git_op::SnapOutcome::Detached => "HEAD is detached; no snapshot recorded".to_owned(),
    }
}

/// Describe a recorded snap, attributing a snapshot to this invocation only
/// when this call actually appended it.
fn snap_summary(snapped: git_op::SnapResult) -> String {
    match snapped {
        git_op::SnapResult::Appended(operation) => {
            format!("Recorded snapshot {}", short(&operation))
        }
        git_op::SnapResult::Current(operation) => {
            format!("Operation log is current ({})", short(&operation))
        }
    }
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
    let result = git_op::restore_action(&repo, oid)?;
    print_action_result(
        &action_header("Restored to operation", &result.target, None),
        &result,
    );
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
    println!("{}  {}", short(&tip), commit.message()?.summary());
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
        println!(
            "Would undo operation {} (restoring {})",
            short(&plan.target),
            short(&plan.restored)
        );
        return Ok(());
    }
    let result = git_op::undo_action(&repo)?;
    print_action_result(
        &action_header("Undid operation", &result.target, Some(&result.restored)),
        &result,
    );
    Ok(())
}

fn redo_command(dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let plan = git_op::plan_redo(&repo)?;
    if dry_run {
        println!("Would redo operation {}", short(&plan.target));
        return Ok(());
    }
    let result = git_op::redo_action(&repo)?;
    print_action_result(
        &action_header("Redid operation", &result.target, None),
        &result,
    );
    Ok(())
}

/// Decode one operation-log entry: its trailers, the refs it changed, and the
/// metadata diffs it carries.
fn show_command(specification: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repository()?;
    let oid = match specification {
        Some(specification) => git_op::resolve_operation(&repo, specification)?,
        None => {
            let Some(tip) = repo.try_find_reference(git_op::OP_REF)? else {
                return Err("no operation snapshots recorded".into());
            };
            tip.id().detach()
        }
    };
    let commit = repo.find_commit(oid)?;
    let message = commit.message_raw_sloppy();
    let summary = message.split(|&byte| byte == b'\n').next().unwrap_or(&[]);
    println!("{} {}", short(&oid), String::from_utf8_lossy(summary));
    for line in message.split(|&byte| byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"Invoked-by: ") {
            println!("Invoked by: {}", String::from_utf8_lossy(value));
        } else if let Some(value) = line.strip_prefix(b"Git-op: ") {
            println!("Action: {}", String::from_utf8_lossy(value));
        }
    }

    let state = git_op::read(&repo, oid)?;
    let previous = match git_op::parent_operation(&repo, oid)? {
        Some(parent) => Some(git_op::read(&repo, parent)?),
        None => None,
    };

    let changed_refs = match git_op::ref_changes(&repo, oid)? {
        Some(refs) => refs,
        None => git_op::captured_refs(&repo, oid)?,
    };
    if !changed_refs.is_empty() {
        println!("refs:");
        let width = changed_refs
            .iter()
            .map(|change| change.name.chars().count())
            .max()
            .unwrap_or(0);
        for change in &changed_refs {
            let (before, after) = ref_targets(&change.kind);
            println!("  {:width$}  {} → {}", change.name, before, after);
        }
    }

    show_metadata_diff(
        &repo,
        "config",
        previous
            .as_ref()
            .and_then(|state| state.config)
            .map(|blob| blob.oid()),
        state.config.map(|blob| blob.oid()),
    )?;
    show_metadata_diff(
        &repo,
        "description",
        previous
            .as_ref()
            .and_then(|state| state.description)
            .map(|blob| blob.oid()),
        state.description.map(|blob| blob.oid()),
    )?;
    Ok(())
}

/// Render a captured ref transition as its before and after targets.
fn ref_targets(kind: &git_op::RefChangeKind) -> (String, String) {
    match kind {
        git_op::RefChangeKind::Created(new) => ("(new)".to_owned(), target_summary(new)),
        git_op::RefChangeKind::Deleted(old) => (target_summary(old), "(deleted)".to_owned()),
        git_op::RefChangeKind::Updated { old, new } => (target_summary(old), target_summary(new)),
    }
}

/// Print the unified diff of one metadata file between two snapshot states.
///
/// A side a snapshot does not capture diffs against the empty blob, so an
/// initial snapshot shows its file as all additions and a dropped file as all
/// deletions.
fn show_metadata_diff(
    repo: &gix::Repository,
    label: &str,
    before: Option<gix::ObjectId>,
    after: Option<gix::ObjectId>,
) -> Result<(), Box<dyn std::error::Error>> {
    if before == after {
        return Ok(());
    }
    let empty_blob = repo
        .write_buf(gix::objs::Kind::Blob, b"")
        .map_err(|error| error.to_string())?;
    let output = std::process::Command::new("git")
        .current_dir(repo.workdir().unwrap_or(repo.git_dir()))
        .env("GIT_DIR", repo.git_dir())
        .args([
            "diff",
            &before
                .map(|oid| oid.to_string())
                .unwrap_or_else(|| empty_blob.to_string()),
            &after
                .map(|oid| oid.to_string())
                .unwrap_or_else(|| empty_blob.to_string()),
            "--",
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!("git diff failed with {}", output.status).into());
    }
    println!("{label}:");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn print_action_result(header: &str, result: &git_op::ActionResult) {
    println!("{}", action_summary(header, result));
}

/// Compose the header describing an action's outcome, leading with the
/// operation the action acted on so dry-run and real-run agree, and naming
/// the snapshot the repository was left at when the two differ.
fn action_header(verb: &str, target: &gix::ObjectId, restored: Option<&gix::ObjectId>) -> String {
    match restored {
        Some(restored) => format!("{verb} {} (restored {})", short(target), short(restored)),
        None => format!("{verb} {}", short(target)),
    }
}

fn action_summary(header: &str, result: &git_op::ActionResult) -> String {
    let mut summary = header.to_owned();
    if result.changes.is_empty() {
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
    oid.to_string().chars().take(7).collect()
}

#[cfg(test)]
#[path = "tests/exe.rs"]
mod tests;
