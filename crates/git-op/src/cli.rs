//! Command-line parsing for `git-op`.

use clap::{Parser, Subcommand};

/// Command-line arguments accepted by `git-op`.
#[derive(Debug, Parser)]
#[command(
    name = "git-op",
    version,
    about = "Record Git repository metadata snapshots"
)]
pub(crate) struct Cli {
    /// The operation selected by the caller.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Operations supported by `git-op`.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Process a Git reference-transaction hook invocation.
    #[command(hide = true)]
    ReferenceTransaction {
        /// The phase supplied by Git: prepared, committed, or aborted.
        phase: String,
    },
    /// Install the reference-transaction hook globally or in one repository.
    Install {
        /// Install only in the current repository instead of Git's global template.
        #[arg(long)]
        local: bool,
    },
    /// Show the operation-log commits using Git's log command.
    Log {
        /// Additional arguments passed to `git log` after `refs/op`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Restore the repository metadata captured by an operation commit.
    Restore {
        /// The operation commit to restore.
        oid: String,
    },
    /// Restore the state before the latest operation-log commit.
    Undo,
}
