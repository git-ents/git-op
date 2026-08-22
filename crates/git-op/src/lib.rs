//! An operation log for a repository's local refs and Git metadata.
//!
//! Snapshots are committed to [`OP_REF`]. The operation ref, remote refs, and
//! pseudo references such as `HEAD` are excluded from snapshots.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use facet::Facet;
use facet_git_tree::{RawBlob, RawTree};
use gix::objs::{Kind, Write};
use gix::{self, bstr::ByteSlice};
use gix_refstore::{ApplyError, Committer, GixRefStore, ObjectId, RefEdit, RefName, RefStore};

/// The ref containing the latest repository-state snapshot.
pub const OP_REF: &str = "refs/op";

const HOOK_NAME: &str = "reference-transaction";
const HOOK_BODY: &str = "#!/bin/sh\nexec git-op reference-transaction \"$@\"\n";
const MAX_APPEND_ATTEMPTS: usize = 128;

/// The state of repository metadata captured by an operation-log commit.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct RepositoryState {
    /// Local refs, represented as a ref-namespace tree whose leaves contain
    /// Git ref-file contents.
    pub r#refs: RawTree,
    /// The bytes of the repository's `config` file, when it exists.
    pub config: Option<RawBlob>,
    /// The bytes of the repository's `description` file, when it exists.
    pub description: Option<RawBlob>,
}

/// Errors produced while capturing, storing, or installing repository state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying repository or filesystem operation failed.
    #[error("repository operation failed: {0}")]
    Git(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// A reference name could not be represented in the snapshot tree.
    #[error("invalid reference name {0}")]
    InvalidRef(String),
    /// Two refs would require both a tree and a blob with the same tree name.
    #[error("reference names conflict in the snapshot: {first} and {second}")]
    ConflictingRefs { first: String, second: String },
    /// A compare-and-swap could not be applied after repeated retries.
    #[error("reference update lost a race after {0} attempts")]
    LostRace(usize),
    /// Facet tree serialization failed.
    #[error("facet tree serialization failed: {0}")]
    Serialize(#[source] facet_git_tree::SerializeError),
    /// Facet tree deserialization failed.
    #[error("facet tree deserialization failed: {0}")]
    Deserialize(#[source] facet_git_tree::DeserializeError),
    /// A reference-transaction hook received an unsupported phase.
    #[error("unsupported reference-transaction phase {0}")]
    InvalidPhase(String),
    /// The operation ref is not a direct reference to a commit.
    #[error("{OP_REF} must directly reference a commit")]
    InvalidOperationRef,
    /// A reference-transaction hook received malformed input.
    #[error("invalid reference-transaction input: {0}")]
    InvalidHookInput(String),
    /// Installing the hook would overwrite a hook not installed by this crate.
    #[error("refusing to overwrite existing hook {0}")]
    HookExists(PathBuf),
}

impl Error {
    fn git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Git(error.into())
    }

    fn message(message: impl Into<String>) -> Self {
        Self::git(std::io::Error::other(message.into()))
    }
}

fn is_captured_ref(name: &[u8]) -> bool {
    name.starts_with(b"refs/") && name != OP_REF.as_bytes() && !name.starts_with(b"refs/remotes/")
}

fn ref_contents(reference: &gix::Reference<'_>) -> Vec<u8> {
    match reference.target() {
        gix::refs::TargetRef::Object(id) => {
            let mut contents = id.to_hex().to_string().into_bytes();
            contents.push(b'\n');
            contents
        }
        gix::refs::TargetRef::Symbolic(name) => {
            let mut contents = b"ref: ".to_vec();
            contents.extend_from_slice(name.as_bstr().as_bytes());
            contents.push(b'\n');
            contents
        }
    }
}

fn validate_ref_paths(refs: &BTreeMap<String, Vec<u8>>) -> Result<(), Error> {
    let mut previous: Option<&str> = None;
    for name in refs.keys() {
        let relative = name
            .strip_prefix("refs/")
            .filter(|name| !name.is_empty() && !name.ends_with('/'))
            .ok_or_else(|| Error::InvalidRef(name.clone()))?;
        if relative.split('/').any(|part| part.is_empty()) {
            return Err(Error::InvalidRef(name.clone()));
        }
        if let Some(previous) = previous
            && name.starts_with(previous)
            && name.as_bytes().get(previous.len()) == Some(&b'/')
        {
            return Err(Error::ConflictingRefs {
                first: previous.to_owned(),
                second: name.clone(),
            });
        }
        previous = Some(name.as_str());
    }
    Ok(())
}

fn write_ref_tree(
    repo: &gix::Repository,
    refs: &BTreeMap<String, Vec<u8>>,
) -> Result<ObjectId, Error> {
    validate_ref_paths(refs)?;
    let mut root = RefNode::default();
    for (name, bytes) in refs {
        let relative = name.strip_prefix("refs/").expect("validated ref prefix");
        let oid = repo.write_buf(Kind::Blob, bytes).map_err(Error::git)?;
        root.insert(relative, oid, name)?;
    }
    root.write(repo)
}

#[derive(Default)]
struct RefNode {
    children: BTreeMap<String, RefNode>,
    leaves: BTreeMap<String, ObjectId>,
}

impl RefNode {
    fn insert(&mut self, relative: &str, oid: ObjectId, full_name: &str) -> Result<(), Error> {
        let (head, tail) = relative
            .split_once('/')
            .map_or((relative, None), |(head, tail)| (head, Some(tail)));
        match tail {
            None => {
                if self.children.contains_key(head)
                    || self.leaves.insert(head.to_owned(), oid).is_some()
                {
                    return Err(Error::ConflictingRefs {
                        first: full_name.to_owned(),
                        second: format!("refs/{relative}"),
                    });
                }
            }
            Some(tail) => {
                if self.leaves.contains_key(head) {
                    return Err(Error::ConflictingRefs {
                        first: format!("refs/{head}"),
                        second: full_name.to_owned(),
                    });
                }
                self.children
                    .entry(head.to_owned())
                    .or_default()
                    .insert(tail, oid, full_name)?;
            }
        }
        Ok(())
    }

    fn write(&self, repo: &gix::Repository) -> Result<ObjectId, Error> {
        let mut entries = Vec::with_capacity(self.children.len() + self.leaves.len());
        for (name, child) in &self.children {
            entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryMode::from(gix::objs::tree::EntryKind::Tree),
                filename: name.clone().into(),
                oid: child.write(repo)?,
            });
        }
        for (name, oid) in &self.leaves {
            entries.push(gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryMode::from(gix::objs::tree::EntryKind::Blob),
                filename: name.clone().into(),
                oid: *oid,
            });
        }
        entries.sort();
        repo.write(&gix::objs::Tree { entries }).map_err(Error::git)
    }
}

fn read_repository_file(repo: &gix::Repository, name: &str) -> Result<Option<Vec<u8>>, Error> {
    let path = repo.common_dir().join(name);
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::git(error)),
    }
}

fn operation_parent(repo: &gix::Repository, name: &RefName) -> Result<Option<ObjectId>, Error> {
    let Some(reference) = repo.try_find_reference(name.as_str()).map_err(Error::git)? else {
        return Ok(None);
    };
    let gix::refs::TargetRef::Object(parent) = reference.target() else {
        return Err(Error::InvalidOperationRef);
    };
    let parent = parent.to_owned();
    repo.find_commit(parent)
        .map_err(|_| Error::InvalidOperationRef)?;
    Ok(Some(parent))
}

/// Capture all included refs and repository metadata files from `repo`.
pub fn capture(repo: &gix::Repository) -> Result<RepositoryState, Error> {
    let mut ref_values = BTreeMap::new();
    for reference in repo
        .references()
        .map_err(Error::git)?
        .all()
        .map_err(Error::git)?
    {
        let reference = reference.map_err(Error::git)?;
        let raw_name = reference.name().as_bstr();
        if !is_captured_ref(raw_name.as_bytes()) {
            continue;
        }
        let name = raw_name
            .to_str()
            .map_err(|_| Error::InvalidRef(raw_name.to_string()))?;
        ref_values.insert(name.to_owned(), ref_contents(&reference));
    }

    let ref_tree = write_ref_tree(repo, &ref_values)?;
    let config = read_repository_file(repo, "config")?;
    let description = read_repository_file(repo, "description")?;
    let config_oid = config
        .as_deref()
        .map(|bytes| repo.write_buf(Kind::Blob, bytes))
        .transpose()
        .map_err(Error::git)?;
    let description_oid = description
        .as_deref()
        .map(|bytes| repo.write_buf(Kind::Blob, bytes))
        .transpose()
        .map_err(Error::git)?;

    Ok(RepositoryState {
        r#refs: RawTree::new(ref_tree),
        config: config_oid.map(RawBlob::new),
        description: description_oid.map(RawBlob::new),
    })
}

/// Serialize a repository state into the repository's object database.
pub fn serialize(repo: &gix::Repository, state: &RepositoryState) -> Result<ObjectId, Error> {
    facet_git_tree::serialize_into(state, repo).map_err(Error::Serialize)
}

/// Append a snapshot commit and CAS-advance [`OP_REF`].
pub fn append(repo: &gix::Repository, message: &str) -> Result<ObjectId, Error> {
    let refs = GixRefStore::new(repo);
    let name = RefName::new(OP_REF).map_err(|_| Error::InvalidRef(OP_REF.to_owned()))?;

    for _ in 0..MAX_APPEND_ATTEMPTS {
        let parent = operation_parent(repo, &name)?;
        let state = capture(repo)?;
        let tree = serialize(repo, &state)?;
        let commit = gix::objs::Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: refs.author().map_err(Error::git)?,
            committer: refs.signature().map_err(Error::git)?,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let commit_id = repo.write(&commit).map_err(Error::git)?;
        let edit = match parent {
            Some(expected) => RefEdit::Update {
                name: name.clone(),
                expected,
                new: commit_id,
            },
            None => RefEdit::Create {
                name: name.clone(),
                new: commit_id,
            },
        };
        match refs.apply(edit) {
            Ok(()) => return Ok(commit_id),
            Err(ApplyError::LostRace { .. }) => continue,
            Err(ApplyError::Backend(error)) => return Err(Error::git(error)),
        }
    }
    Err(Error::LostRace(MAX_APPEND_ATTEMPTS))
}

/// Read a repository state from an operation-log commit.
pub fn read(repo: &gix::Repository, commit: ObjectId) -> Result<RepositoryState, Error> {
    let commit = repo.find_commit(commit).map_err(Error::git)?;
    let tree = commit.tree_id().map_err(Error::git)?.detach();
    facet_git_tree::deserialize(&tree, repo).map_err(Error::Deserialize)
}

/// Install the `reference-transaction` hook in this repository.
pub fn install_local(repo: &gix::Repository) -> Result<(), Error> {
    install_hook(git_path(repo, "hooks")?)
}

/// Install the hook in Git's configured global template directory, creating a
/// default directory setting when `init.templateDir` is not configured.
pub fn install_global() -> Result<(), Error> {
    let template = match git_config_global_template()? {
        Some(path) => path,
        None => {
            let config_home = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
                .ok_or_else(|| Error::message("neither XDG_CONFIG_HOME nor HOME is set"))?;
            let template = config_home.join("git/templates");
            let status = Command::new("git")
                .args(["config", "--global", "init.templateDir"])
                .arg(&template)
                .status()
                .map_err(Error::git)?;
            if !status.success() {
                return Err(Error::message(format!(
                    "git config --global failed with {status}"
                )));
            }
            template
        }
    };
    install_hook(template.join("hooks"))
}

fn git_path(repo: &gix::Repository, name: &str) -> Result<PathBuf, Error> {
    let output = Command::new("git")
        .current_dir(repo.current_dir())
        .args(["rev-parse", "--path-format=absolute", "--git-path", name])
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Err(Error::message(format!(
            "git rev-parse --git-path {name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let path = std::str::from_utf8(&output.stdout)
        .map_err(|error| Error::message(error.to_string()))?
        .trim();
    Ok(PathBuf::from(path))
}

fn git_config_global_template() -> Result<Option<PathBuf>, Error> {
    let output = Command::new("git")
        .args(["config", "--global", "--path", "--get", "init.templateDir"])
        .output()
        .map_err(Error::git)?;
    if output.status.success() {
        let value = std::str::from_utf8(&output.stdout)
            .map_err(|error| Error::message(error.to_string()))?
            .trim();
        return Ok((!value.is_empty()).then(|| PathBuf::from(value)));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(Error::message(format!(
        "git config --global --get init.templateDir failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Process one `reference-transaction` hook invocation.
///
/// Only the committed phase creates a snapshot. Transactions that contain only
/// excluded refs are ignored. Updating [`OP_REF`] alone does not trigger a
/// snapshot, which prevents the operation-log update from recursively invoking
/// itself when Git runs hooks for ref transactions.
pub fn reference_transaction(
    repo: &gix::Repository,
    phase: &str,
    input: &[u8],
) -> Result<(), Error> {
    match phase {
        "prepared" | "aborted" => Ok(()),
        "committed" => {
            if transaction_changes_captured_refs(input)? {
                append(repo, "reference-transaction")?;
            }
            Ok(())
        }
        phase => Err(Error::InvalidPhase(phase.to_owned())),
    }
}

fn transaction_changes_captured_refs(input: &[u8]) -> Result<bool, Error> {
    let mut captured = false;
    let mut saw_line = false;
    for line in input.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        saw_line = true;
        let fields: Vec<_> = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect();
        if fields.len() != 3 {
            return Err(Error::InvalidHookInput(
                String::from_utf8_lossy(line).into_owned(),
            ));
        }
        captured |= is_captured_ref(fields[2]);
    }
    if !saw_line {
        return Err(Error::InvalidHookInput("transaction is empty".to_owned()));
    }
    Ok(captured)
}

fn install_hook(hooks: impl AsRef<Path>) -> Result<(), Error> {
    let hooks = hooks.as_ref();
    fs::create_dir_all(hooks).map_err(Error::git)?;
    let path = hooks.join(HOOK_NAME);
    match fs::read(&path) {
        Ok(existing) if existing == HOOK_BODY.as_bytes() => {
            make_executable(&path)?;
            return Ok(());
        }
        Ok(_) => return Err(Error::HookExists(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::git(error)),
    }
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::HookExists(path));
        }
        Err(error) => return Err(Error::git(error)),
    };
    file.write_all(HOOK_BODY.as_bytes()).map_err(Error::git)?;
    file.sync_all().map_err(Error::git)?;
    drop(file);
    make_executable(&path)
}

fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).map_err(Error::git)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(Error::git)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_filter_excludes_operation_and_remote_refs() {
        assert!(is_captured_ref(b"refs/heads/main"));
        assert!(is_captured_ref(b"refs/tags/v1"));
        assert!(!is_captured_ref(OP_REF.as_bytes()));
        assert!(!is_captured_ref(b"refs/remotes/origin/main"));
        assert!(!is_captured_ref(b"HEAD"));
    }

    #[test]
    fn ref_tree_rejects_file_directory_conflicts() {
        let refs = BTreeMap::from([
            ("refs/heads/main".to_owned(), Vec::new()),
            ("refs/heads/main/topic".to_owned(), Vec::new()),
        ]);
        assert!(matches!(
            validate_ref_paths(&refs),
            Err(Error::ConflictingRefs { .. })
        ));
    }

    #[test]
    fn hook_parser_classifies_only_captured_refs() {
        let oid = b"0000000000000000000000000000000000000000";
        assert!(
            transaction_changes_captured_refs(
                &[oid.as_slice(), oid.as_slice(), b"refs/heads/main", b"\n"].concat()
            )
            .is_err()
        );
        assert!(
            transaction_changes_captured_refs(
                &[
                    oid.as_slice(),
                    b" ",
                    b"1111111111111111111111111111111111111111",
                    b" ",
                    b"refs/heads/main\n"
                ]
                .concat()
            )
            .expect("valid transaction")
        );
        assert!(
            !transaction_changes_captured_refs(
                &[
                    oid.as_slice(),
                    b" ",
                    b"1111111111111111111111111111111111111111",
                    b" ",
                    b"refs/remotes/origin/main\n"
                ]
                .concat()
            )
            .expect("valid transaction")
        );
        assert!(
            !transaction_changes_captured_refs(
                &[
                    oid.as_slice(),
                    b" ",
                    b"1111111111111111111111111111111111111111",
                    b" ",
                    OP_REF.as_bytes(),
                    b"\n"
                ]
                .concat()
            )
            .expect("valid transaction")
        );
    }

    #[test]
    fn hook_parser_rejects_empty_transactions() {
        assert!(matches!(
            transaction_changes_captured_refs(b"\n"),
            Err(Error::InvalidHookInput(_))
        ));
    }
}
