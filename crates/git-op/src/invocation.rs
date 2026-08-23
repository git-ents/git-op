use std::{path::Path, process::Command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InvokedBy(pub(crate) String);

impl InvokedBy {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn detect() -> Option<InvokedBy> {
    #[cfg(unix)]
    {
        detect_unix()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
fn detect_unix() -> Option<InvokedBy> {
    let mut pid = process(std::process::id())?.parent;
    for _ in 0..1024 {
        let process = process(pid)?;
        if let Some(command) = git_command(&process.command) {
            return Some(InvokedBy(command));
        }
        if process.parent == 0 || process.parent == pid {
            return None;
        }
        pid = process.parent;
    }
    None
}

#[cfg(unix)]
struct Process {
    parent: u32,
    command: String,
}

#[cfg(unix)]
fn process(pid: u32) -> Option<Process> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "ppid=", "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8(output.stdout).ok()?;
    let mut fields = line.trim().splitn(2, char::is_whitespace);
    let parent = fields.next()?.trim().parse().ok()?;
    let command = fields.next()?.trim().to_owned();
    Some(Process { parent, command })
}

fn git_command(command: &str) -> Option<String> {
    let mut arguments = command.split_whitespace();
    let executable = arguments.next()?;
    let name = Path::new(executable).file_name()?.to_str()?;
    if name == "git" {
        return Some(format!("git {}", git_subcommand(arguments)?));
    }
    name.strip_prefix("git-")
        .filter(|command| !command.is_empty() && name != "git-op")
        .map(|command| format!("git {command}"))
}

fn git_subcommand<'a>(mut arguments: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    const OPTIONS_WITH_VALUES: &[&str] = &[
        "-C",
        "-c",
        "--config-env",
        "--exec-path",
        "--git-dir",
        "--namespace",
        "--super-prefix",
        "--work-tree",
    ];

    while let Some(argument) = arguments.next() {
        if argument == "--" {
            return arguments.next();
        }
        if OPTIONS_WITH_VALUES.contains(&argument) {
            arguments.next()?;
        } else if !argument.starts_with('-') {
            return Some(argument);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::git_command;

    #[test]
    fn identifies_git_subcommands() {
        assert_eq!(
            git_command("/usr/bin/git commit -m message"),
            Some("git commit".to_owned())
        );
        assert_eq!(
            git_command("/usr/libexec/git-core/git-commit"),
            Some("git commit".to_owned())
        );
    }

    #[test]
    fn skips_git_global_options() {
        assert_eq!(
            git_command("git -C /tmp/repo -c user.name=test commit"),
            Some("git commit".to_owned())
        );
    }

    #[test]
    fn ignores_non_git_processes() {
        assert_eq!(
            git_command("/bin/sh .git/hooks/reference-transaction"),
            None
        );
        assert_eq!(git_command("git-op reference-transaction committed"), None);
    }
}
