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
    /// Show the recorded operation-log snapshots, most recent first.
    Log {
        /// Limit output to the first N snapshots.
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
        /// Show the oldest snapshots first.
        #[arg(long)]
        reverse: bool,
        /// Show each snapshot as one line: abbreviated id and message summary.
        #[arg(long, conflicts_with = "json")]
        oneline: bool,
        /// Show each snapshot as one JSON object per line (JSON Lines).
        #[arg(long)]
        json: bool,
    },
    /// Restore refs, repository config, and description from an operation snapshot.
    ///
    /// The working tree and index are not changed.
    Restore {
        /// The operation commit to restore.
        oid: String,
    },
    /// Restore the state before the latest operation-log commit.
    ///
    /// The working tree and index are not changed.
    Undo,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn log_parses_max_count_and_reverse() {
        let cli = Cli::try_parse_from(["git-op", "log", "-n", "2", "--reverse"])
            .expect("parse log with max-count and reverse");
        let Command::Log {
            max_count,
            reverse,
            oneline,
            json,
        } = cli.command
        else {
            panic!("expected the log command");
        };
        assert_eq!(max_count, Some(2));
        assert!(reverse);
        assert!(!oneline);
        assert!(!json);
    }

    #[test]
    fn log_rejects_oneline_and_json_together() {
        let error = Cli::try_parse_from(["git-op", "log", "--oneline", "--json"])
            .expect_err("--oneline and --json must conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn log_rejects_unknown_git_log_flags() {
        let error = Cli::try_parse_from(["git-op", "log", "--all"])
            .expect_err("--all is not a git-op log flag");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn log_rejects_revision_arguments() {
        let error = Cli::try_parse_from(["git-op", "log", "HEAD~1..HEAD"])
            .expect_err("git-op log takes no positional revision argument");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
