//! Implementations of `git-op` command-line operations.

use std::io::{self, Read};

use crate::cli::Command;

/// Execute the selected command-line operation.
pub(crate) fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::ReferenceTransaction { phase } => reference_transaction(&phase),
        Command::Install { local } => install(local),
        Command::Log { args } => log(args),
        Command::Restore { oid } => restore(&oid),
        Command::Undo => undo(),
    }
}

/// Process a reference-transaction hook invocation from standard input.
fn reference_transaction(phase: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    let repo = gix::discover(".")?;
    git_op::reference_transaction(&repo, phase, &input)?;
    Ok(())
}

/// Install the hook in the current repository or Git's global template.
fn install(local: bool) -> Result<(), Box<dyn std::error::Error>> {
    if local {
        let repo = gix::discover(".")?;
        git_op::install_local(&repo)?;
    } else {
        git_op::install_global()?;
    }
    Ok(())
}

/// Show operation-log commits by delegating formatting and filtering to Git.
fn log(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::discover(".")?;
    if repo.try_find_reference(git_op::OP_REF)?.is_none() {
        println!("No operation snapshots recorded.");
        return Ok(());
    }
    let forbidden = ["--all", "--branches", "--tags", "--remotes"];
    if let Some(argument) = args
        .iter()
        .find(|argument| forbidden.iter().any(|option| *argument == option))
    {
        return Err(format!(
            "git op log does not accept {argument}; it would leave the operation log"
        )
        .into());
    }
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo.current_dir())
        .env("GIT_DIR", repo.git_dir())
        .arg("log")
        .arg(git_op::OP_REF)
        .args(args);
    let status = command.status()?;
    if !status.success() {
        return Err(format!("git log failed with {status}").into());
    }
    Ok(())
}

/// Restore one operation-log commit into the current repository.
fn restore(oid: &str) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::discover(".")?;
    let oid = git_op::resolve_operation(&repo, oid)?;
    git_op::restore(&repo, oid)?;
    Ok(())
}

/// Restore the state before the latest operation-log commit.
fn undo() -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::discover(".")?;
    git_op::undo(&repo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn log_starts_with_operation_ref() {
        let mut command = std::process::Command::new("git");
        command.arg("log").arg(git_op::OP_REF).args(["--oneline"]);

        let args = command
            .get_args()
            .map(|arg| arg.to_str().expect("test arguments are UTF-8"))
            .collect::<Vec<_>>();
        assert_eq!(args, ["log", "refs/op", "--oneline"]);
    }
}
