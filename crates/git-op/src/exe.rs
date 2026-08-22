//! Implementations of `git-op` command-line operations.

use std::io::{self, Read};

use crate::cli::Command;

/// Execute the selected command-line operation.
pub(crate) fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::ReferenceTransaction { phase } => reference_transaction(&phase),
        Command::Install { local } => install(local),
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
