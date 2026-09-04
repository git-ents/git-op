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

#[test]
fn excludes_arguments_that_may_contain_secrets() {
    const SECRET: &str = "super-secret-token";
    let commands = [
        format!("git -c http.extraheader=Authorization:{SECRET} commit"),
        format!("git commit https://user:{SECRET}@example.com/repository"),
        format!("git commit --password={SECRET}"),
    ];

    for command in commands {
        let invoked_by = git_command(&command).expect("identify git command");
        assert_eq!(invoked_by, "git commit");
        assert!(!invoked_by.contains(SECRET));
    }
}
