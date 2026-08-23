//! An operation log for a repository's local refs and Git metadata.
//!
//! Snapshots are committed to [`OP_REF`]. The operation ref, remote refs, and
//! pseudo references such as `HEAD` are excluded from snapshots.

use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};

use facet::Facet;
use facet_git_tree::{RawBlob, RawTree};
use gix::objs::{Kind, Write};
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit as GixRefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix::{self, bstr::ByteSlice};
use gix_refstore::{ApplyError, Committer, GixRefStore, ObjectId, RefEdit, RefName, RefStore};

mod config;
mod invocation;

use config::commit_signing_enabled;
pub use config::{install_global, install_local};
use invocation::InvokedBy;

/// The ref containing the latest repository-state snapshot.
pub const OP_REF: &str = "refs/op";

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

/// Optional commit metadata for [`append_with_options`].
#[derive(Debug, Clone, Default)]
pub struct AppendOptions {
    /// The author signature, or the repository-configured author when absent.
    pub author: Option<gix::actor::Signature>,
    /// The committer signature, or the repository-configured committer when absent.
    pub committer: Option<gix::actor::Signature>,
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
    /// The operation log has no earlier snapshot to restore.
    #[error("the operation log has no earlier snapshot to restore")]
    NothingToUndo,
    /// A snapshot contains a reference or metadata value that cannot be restored.
    #[error("invalid snapshot content: {0}")]
    InvalidSnapshot(String),
}

impl Error {
    /// Convert an error into the crate's repository-operation error variant.
    fn git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Git(error.into())
    }

    /// Create a repository-operation error from a diagnostic message.
    fn message(message: impl Into<String>) -> Self {
        Self::git(std::io::Error::other(message.into()))
    }
}

/// Return whether a ref name belongs in a captured repository snapshot.
fn is_captured_ref(name: &[u8]) -> bool {
    name.starts_with(b"refs/") && name != OP_REF.as_bytes() && !name.starts_with(b"refs/remotes/")
}

/// Render a reference target in Git's loose-ref file format.
///
/// Object references are written as an object ID followed by a newline, while
/// symbolic references use Git's `ref: ` prefix. The resulting bytes can be
/// stored as a blob without converting Git's byte-oriented ref contents to
/// UTF-8.
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

/// Validate that captured ref names form a representable tree namespace.
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

/// Write captured ref contents as a nested Git tree.
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
    /// Insert one ref leaf, rejecting file/directory collisions.
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

    /// Recursively write this namespace node as a Git tree.
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

/// Read a repository metadata file, preserving its bytes when present.
///
/// The file is resolved relative to the repository's common directory. Missing
/// files return `Ok(None)`; existing files are returned byte-for-byte in
/// `Ok(Some(_))`.
///
/// The capture example exercises this behavior for both metadata files: their
/// contents remain raw bytes in the resulting Git blobs.
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!(
///     "git-op-read-repository-file-{}-{unique}",
///     std::process::id()
/// ));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// std::fs::write(repo.common_dir().join("description"), b"raw bytes\\xff\\n")
///     .expect("write description");
/// let state = git_op::capture(&repo).expect("capture repository state");
/// let description = repo
///     .find_blob(state.description.expect("description was captured").oid())
///     .expect("read description blob");
/// assert_eq!(description.data, b"raw bytes\\xff\\n");
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
fn read_repository_file(repo: &gix::Repository, name: &str) -> Result<Option<Vec<u8>>, Error> {
    let path = repo.common_dir().join(name);
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::git(error)),
    }
}

/// Resolve and validate the current operation-log ref target, if any.
///
/// A missing operation ref is represented as `None`. Existing targets must be
/// direct references to commits; symbolic targets and references to other
/// object kinds are rejected before a new snapshot is built. The public
/// [`append`] and [`append_with_options`] functions use this check before
/// constructing each candidate commit.
fn operation_target(repo: &gix::Repository, name: &RefName) -> Result<Option<ObjectId>, Error> {
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
///
/// # Examples
///
/// This example verifies that repository files are stored as raw Git blobs and
/// can be read back without interpreting their contents.
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-capture-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// std::fs::write(repo.common_dir().join("config"), b"[core]\n\tbare = false\n")
///     .expect("write config");
/// std::fs::write(repo.common_dir().join("description"), b"example repository\n")
///     .expect("write description");
///
/// let state = git_op::capture(&repo).expect("capture repository state");
/// let config = repo
///     .find_blob(state.config.expect("config was created").oid())
///     .expect("read config blob");
/// let description = repo
///     .find_blob(state.description.expect("description was created").oid())
///     .expect("read description blob");
/// assert_eq!(config.data, b"[core]\n\tbare = false\n");
/// assert_eq!(description.data, b"example repository\n");
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
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

/// Serialize a repository state into a Git tree object.
///
/// # Examples
///
/// A captured state can be encoded directly into the repository's object
/// database and yields a tree object suitable for a snapshot commit.
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-serialize-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// let state = git_op::capture(&repo).expect("capture repository state");
/// let tree = git_op::serialize(&repo, &state).expect("serialize repository state");
/// assert_eq!(
///     repo.find_header(tree).expect("read serialized tree").kind(),
///     gix::objs::Kind::Tree,
/// );
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn serialize(repo: &gix::Repository, state: &RepositoryState) -> Result<ObjectId, Error> {
    facet_git_tree::serialize_into(state, repo).map_err(Error::Serialize)
}

/// Append a snapshot commit and advance [`OP_REF`] with compare-and-swap.
///
/// # Examples
///
/// Appending to a repository creates a commit whose tree contains the
/// captured state and publishes its object ID through [`OP_REF`].
///
/// ```
/// use gix::bstr::ByteSlice;
///
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-append-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// let commit = git_op::append(&repo, "initial snapshot").expect("append snapshot");
/// assert_eq!(
///     repo.find_commit(commit)
///         .expect("read snapshot commit")
///         .message()
///         .expect("parse snapshot message")
///         .summary()
///         .as_bytes(),
///     b"initial snapshot",
/// );
/// assert_eq!(
///     repo.find_reference(git_op::OP_REF)
///         .expect("read operation ref")
///         .target()
///         .try_id(),
///     Some(commit.as_ref()),
/// );
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn append(repo: &gix::Repository, message: &str) -> Result<ObjectId, Error> {
    append_with_options(repo, message, AppendOptions::default())
}

/// Append a snapshot commit with explicitly selected commit metadata.
///
/// Missing author or committer signatures are loaded from the repository's
/// configured identity. The operation ref is advanced with compare-and-swap.
///
/// # Examples
///
/// A caller can provide deterministic signatures when importing or replaying
/// snapshots.
///
/// ```
/// use gix::bstr::ByteSlice;
///
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-append-options-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// let signature = gix::actor::Signature {
///     name: "snapshot importer".into(),
///     email: "importer@example.com".into(),
///     time: gix::date::Time { seconds: 1_700_000_000, offset: 0 },
/// };
/// let commit = git_op::append_with_options(
///     &repo,
///     "imported snapshot",
///     git_op::AppendOptions {
///         author: Some(signature.clone()),
///         committer: Some(signature),
///     },
/// )
/// .expect("append imported snapshot");
/// let commit = repo.find_commit(commit).expect("read snapshot commit");
/// assert_eq!(
///     commit
///         .author()
///         .expect("read author")
///         .email,
///     "importer@example.com".as_bytes().as_bstr(),
/// );
/// assert_eq!(
///     commit
///         .committer()
///         .expect("read committer")
///         .email,
///     "importer@example.com".as_bytes().as_bstr(),
/// );
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn append_with_options(
    repo: &gix::Repository,
    message: &str,
    options: AppendOptions,
) -> Result<ObjectId, Error> {
    append_internal(repo, CommitMessage::Explicit(message), options)
}

fn append_internal(
    repo: &gix::Repository,
    requested_message: CommitMessage<'_>,
    options: AppendOptions,
) -> Result<ObjectId, Error> {
    let refs = GixRefStore::new(repo);
    let name = RefName::new(OP_REF).map_err(|_| Error::InvalidRef(OP_REF.to_owned()))?;
    let signing = commit_signing_enabled(repo)?;
    let invoked_by = matches!(requested_message, CommitMessage::Generated)
        .then(invocation::detect)
        .flatten();
    let author = match options.author {
        Some(author) => author,
        None => refs.author().map_err(Error::git)?,
    };
    let committer = match options.committer {
        Some(committer) => committer,
        None => refs.signature().map_err(Error::git)?,
    };

    for _ in 0..MAX_APPEND_ATTEMPTS {
        let parent = operation_target(repo, &name)?;
        let state = capture(repo)?;
        let message = match (requested_message, parent) {
            (CommitMessage::Explicit(message), _) => message.to_owned(),
            (CommitMessage::Generated, None) => "op: capture initial repository state".to_owned(),
            (CommitMessage::Generated, Some(parent)) => {
                let changed =
                    SnapshotEntries::from_state(&state).diff(&SnapshotEntries::read(repo, parent)?);
                let Some(message) = snapshot_message(changed) else {
                    return Ok(parent);
                };
                message
            }
        };
        let tree = serialize(repo, &state)?;
        let commit_id = write_commit(
            repo,
            tree,
            parent,
            &message,
            invoked_by.as_ref(),
            &author,
            &committer,
            signing,
        )?;
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

/// Write a snapshot commit through Git, optionally letting Git sign it.
///
/// `git commit-tree` is used rather than constructing the commit object
/// directly so Git's configured signing implementation can add a `gpgsig`
/// header. It does not invoke commit hooks.
#[allow(clippy::too_many_arguments)]
fn write_commit(
    repo: &gix::Repository,
    tree: ObjectId,
    parent: Option<ObjectId>,
    message: &str,
    invoked_by: Option<&InvokedBy>,
    author: &gix::actor::Signature,
    committer: &gix::actor::Signature,
    signing: bool,
) -> Result<ObjectId, Error> {
    let mut command = Command::new("git");
    command
        .current_dir(repo.current_dir())
        .arg("commit-tree")
        .arg(tree.to_hex().to_string());
    if let Some(parent) = parent {
        command.args(["-p", &parent.to_hex().to_string()]);
    }
    command.arg("-m").arg(message);
    if let Some(invoked_by) = invoked_by {
        command
            .arg("-m")
            .arg(format!("Invoked-by: {}", invoked_by.as_str()));
    }
    if signing {
        command.arg("-S");
    }
    let author = format_signature(author)?;
    let committer = format_signature(committer)?;
    let output = command
        .env("GIT_DIR", repo.git_dir())
        .env("GIT_AUTHOR_NAME", author.name)
        .env("GIT_AUTHOR_EMAIL", author.email)
        .env("GIT_AUTHOR_DATE", author.time)
        .env("GIT_COMMITTER_NAME", committer.name)
        .env("GIT_COMMITTER_EMAIL", committer.email)
        .env("GIT_COMMITTER_DATE", committer.time)
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Err(Error::message(format!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    ObjectId::from_hex(
        std::str::from_utf8(&output.stdout)
            .map_err(|error| Error::message(error.to_string()))?
            .trim()
            .as_bytes(),
    )
    .map_err(|error| {
        Error::message(format!(
            "git commit-tree returned invalid object ID: {error}"
        ))
    })
}

/// Format a Git signature for the commit environment.
///
/// Git reads names and email addresses from environment variables when
/// creating a commit. Newlines are rejected because they could inject extra
/// commit-header content.
fn format_signature(signature: &gix::actor::Signature) -> Result<FormattedSignature, Error> {
    if signature.name.contains(&b'\n') || signature.email.contains(&b'\n') {
        return Err(Error::message("commit signature contains a newline"));
    }
    Ok(FormattedSignature {
        name: signature.name.to_string(),
        email: signature.email.to_string(),
        time: signature.time.to_string(),
    })
}

struct FormattedSignature {
    name: String,
    email: String,
    time: String,
}

#[derive(Clone, Copy)]
enum CommitMessage<'a> {
    Explicit(&'a str),
    Generated,
}

/// Deserialize a repository state from a snapshot commit.
///
/// # Examples
///
/// A snapshot commit can be decoded into the same state representation that
/// was captured before it was serialized.
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-read-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// std::fs::write(repo.common_dir().join("description"), b"snapshot example\n")
///     .expect("write description");
/// let commit = git_op::append(&repo, "capture metadata").expect("append snapshot");
/// let state = git_op::read(&repo, commit).expect("read snapshot");
/// let description = repo
///     .find_blob(state.description.expect("description was captured").oid())
///     .expect("read description blob");
/// assert_eq!(description.data, b"snapshot example\n");
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn read(repo: &gix::Repository, commit: ObjectId) -> Result<RepositoryState, Error> {
    let commit = repo.find_commit(commit).map_err(Error::git)?;
    let tree = commit.tree_id().map_err(Error::git)?.detach();
    facet_git_tree::deserialize(&tree, repo).map_err(Error::Deserialize)
}

/// The parts of the captured state that a snapshot changed relative to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    /// Whether the captured refs tree changed.
    pub r#refs: bool,
    /// Whether the captured `config` file changed.
    pub config: bool,
    /// Whether the captured `description` file changed.
    pub description: bool,
}

impl Changes {
    /// The names of the changed parts, in capture order.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.r#refs {
            names.push("refs");
        }
        names.extend(self.file_names());
        names
    }

    /// The names of the changed metadata files, in capture order.
    pub fn file_names(&self) -> Vec<&'static str> {
        [(self.config, "config"), (self.description, "description")]
            .into_iter()
            .filter_map(|(changed, name)| changed.then_some(name))
            .collect()
    }
}

/// One captured ref changed by a snapshot, relative to its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    /// The full ref name, including the `refs/` prefix.
    pub name: String,
    /// The transition the ref made.
    pub kind: RefChangeKind,
}

/// How a captured ref's target changed between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefChangeKind {
    /// The ref exists only in the newer snapshot.
    Created(Target),
    /// The ref exists only in the older snapshot.
    Deleted(Target),
    /// The ref exists in both snapshots with different targets.
    Updated {
        /// The target in the older snapshot.
        old: Target,
        /// The target in the newer snapshot.
        new: Target,
    },
}

/// The object IDs of a snapshot commit's top-level `refs`, `config`, and
/// `description` tree entries.
///
/// Comparing only these entries, rather than the fully deserialized
/// [`RepositoryState`], avoids walking the entire captured refs tree just to
/// detect whether a snapshot changed anything.
struct SnapshotEntries {
    r#refs: ObjectId,
    config: Option<ObjectId>,
    description: Option<ObjectId>,
}

impl SnapshotEntries {
    /// Read the top-level tree entries of a snapshot commit.
    fn read(repo: &gix::Repository, commit: ObjectId) -> Result<Self, Error> {
        let tree_id = repo
            .find_commit(commit)
            .map_err(Error::git)?
            .tree_id()
            .map_err(Error::git)?
            .detach();
        let tree = repo.find_tree(tree_id).map_err(Error::git)?;
        let mut refs = None;
        let mut config = None;
        let mut description = None;
        for entry in tree.decode().map_err(Error::git)?.entries {
            match entry.filename.to_str().ok() {
                Some("refs") => refs = Some(entry.oid.to_owned()),
                Some("config") => {
                    config = Some(Self::read_optional_blob(repo, entry.oid.to_owned())?)
                }
                Some("description") => {
                    description = Some(Self::read_optional_blob(repo, entry.oid.to_owned())?);
                }
                _ => {}
            }
        }
        let r#refs = refs.ok_or_else(|| {
            Error::InvalidSnapshot(format!("{commit} is missing its refs tree entry"))
        })?;
        Ok(Self {
            r#refs,
            config: config.flatten(),
            description: description.flatten(),
        })
    }

    /// Read the blob wrapped by a `facet_git_tree` `Option<RawBlob>` field.
    ///
    /// `facet_git_tree` encodes an absent value as an empty tree and a
    /// present value as a tree with a single `some` entry, rather than
    /// storing a blob directly under the field name.
    fn read_optional_blob(
        repo: &gix::Repository,
        tree: ObjectId,
    ) -> Result<Option<ObjectId>, Error> {
        let tree = repo.find_tree(tree).map_err(Error::git)?;
        for entry in tree.decode().map_err(Error::git)?.entries {
            if entry.filename.to_str().ok() == Some("some") {
                return Ok(Some(entry.oid.to_owned()));
            }
        }
        Ok(None)
    }

    /// The entries a pending state would serialize to, without reading anything.
    fn from_state(state: &RepositoryState) -> Self {
        Self {
            r#refs: state.r#refs.oid(),
            config: state.config.map(|blob| blob.oid()),
            description: state.description.map(|blob| blob.oid()),
        }
    }

    /// The changes `self` represents relative to `previous`.
    fn diff(&self, previous: &Self) -> Changes {
        Changes {
            r#refs: self.r#refs != previous.r#refs,
            config: self.config != previous.config,
            description: self.description != previous.description,
        }
    }
}

/// The parts changed by `commit` relative to its parent, or `None` when
/// `commit` is the initial snapshot and has no parent to compare against.
///
/// # Examples
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-changes-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
///
/// let initial = git_op::append(&repo, "initial snapshot").expect("append initial snapshot");
/// assert_eq!(git_op::changes(&repo, initial).expect("compute changes"), None);
///
/// std::fs::write(repo.common_dir().join("description"), b"example\n")
///     .expect("write description");
/// let update = git_op::append(&repo, "update snapshot").expect("append updated snapshot");
/// let changed = git_op::changes(&repo, update)
///     .expect("compute changes")
///     .expect("updated snapshot has a parent");
/// assert_eq!(changed.names(), vec!["description"]);
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn changes(repo: &gix::Repository, commit: ObjectId) -> Result<Option<Changes>, Error> {
    let Some(parent) = parent_snapshot(repo, commit)? else {
        return Ok(None);
    };
    let current = SnapshotEntries::read(repo, commit)?;
    let previous = SnapshotEntries::read(repo, parent)?;
    Ok(Some(current.diff(&previous)))
}

/// The first parent of a snapshot commit, or `None` for the initial snapshot.
fn parent_snapshot(repo: &gix::Repository, commit: ObjectId) -> Result<Option<ObjectId>, Error> {
    Ok(repo
        .find_commit(commit)
        .map_err(Error::git)?
        .parent_ids()
        .next()
        .map(|id| id.detach()))
}

/// The individual refs `commit` changed relative to its parent, ordered by
/// name, or `None` when `commit` is the initial snapshot and has no parent to
/// compare against.
///
/// # Examples
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!("git-op-ref-changes-{}-{unique}", std::process::id()));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// let empty_tree = repo.write_object(&gix::objs::Tree::empty()).expect("write empty tree");
/// let target = repo
///     .commit("refs/heads/main", "example", empty_tree, gix::commit::NO_PARENT_IDS)
///     .expect("commit to main")
///     .detach();
///
/// let initial = git_op::append(&repo, "initial snapshot").expect("append initial snapshot");
/// assert_eq!(git_op::ref_changes(&repo, initial).expect("compute ref changes"), None);
///
/// repo.reference("refs/heads/topic", target, gix::refs::transaction::PreviousValue::MustNotExist, "branch")
///     .expect("create topic");
/// let update = git_op::append(&repo, "create topic").expect("append updated snapshot");
/// let changed = git_op::ref_changes(&repo, update)
///     .expect("compute ref changes")
///     .expect("updated snapshot has a parent");
/// assert_eq!(
///     changed,
///     vec![git_op::RefChange {
///         name: "refs/heads/topic".to_owned(),
///         kind: git_op::RefChangeKind::Created(gix::refs::Target::Object(target)),
///     }],
/// );
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn ref_changes(
    repo: &gix::Repository,
    commit: ObjectId,
) -> Result<Option<Vec<RefChange>>, Error> {
    let Some(parent) = parent_snapshot(repo, commit)? else {
        return Ok(None);
    };
    let current = SnapshotEntries::read(repo, commit)?;
    let previous = SnapshotEntries::read(repo, parent)?;
    let mut changes = Vec::new();
    diff_ref_trees(repo, previous.r#refs, current.r#refs, "refs", &mut changes)?;
    Ok(Some(changes))
}

/// Compare two captured ref trees, appending one [`RefChange`] per ref whose
/// target differs.
///
/// Subtrees with equal object IDs are identical by construction, so the walk
/// never descends into unchanged parts of the ref namespace.
fn diff_ref_trees(
    repo: &gix::Repository,
    previous: ObjectId,
    current: ObjectId,
    prefix: &str,
    changes: &mut Vec<RefChange>,
) -> Result<(), Error> {
    use gix::objs::tree::EntryKind;

    let previous = ref_tree_entries(repo, previous)?;
    let current = ref_tree_entries(repo, current)?;
    let names: std::collections::BTreeSet<&String> =
        previous.keys().chain(current.keys()).collect();
    for name in names {
        let path = format!("{prefix}/{name}");
        match (previous.get(name), current.get(name)) {
            (Some(previous), Some(current)) if previous == current => {}
            (Some((EntryKind::Tree, previous)), Some((EntryKind::Tree, current))) => {
                diff_ref_trees(repo, *previous, *current, &path, changes)?;
            }
            (Some((EntryKind::Blob, previous)), Some((EntryKind::Blob, current))) => {
                changes.push(RefChange {
                    name: path,
                    kind: RefChangeKind::Updated {
                        old: read_ref_target(repo, *previous)?,
                        new: read_ref_target(repo, *current)?,
                    },
                });
            }
            (previous, current) => {
                for (name, target) in ref_tree_targets(repo, previous, &path)? {
                    changes.push(RefChange {
                        name,
                        kind: RefChangeKind::Deleted(target),
                    });
                }
                for (name, target) in ref_tree_targets(repo, current, &path)? {
                    changes.push(RefChange {
                        name,
                        kind: RefChangeKind::Created(target),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The immediate entries of a captured ref tree, keyed by path component.
fn ref_tree_entries(
    repo: &gix::Repository,
    tree: ObjectId,
) -> Result<BTreeMap<String, (gix::objs::tree::EntryKind, ObjectId)>, Error> {
    let tree = repo.find_tree(tree).map_err(Error::git)?;
    let mut entries = BTreeMap::new();
    for entry in tree.decode().map_err(Error::git)?.entries {
        let name = entry
            .filename
            .to_str()
            .map_err(|_| Error::InvalidSnapshot("non-UTF-8 reference path".to_owned()))?;
        let kind = match entry.mode.kind() {
            kind @ (gix::objs::tree::EntryKind::Tree | gix::objs::tree::EntryKind::Blob) => kind,
            kind => {
                return Err(Error::InvalidSnapshot(format!(
                    "ref tree contains {kind:?}"
                )));
            }
        };
        entries.insert(name.to_owned(), (kind, entry.oid.to_owned()));
    }
    Ok(entries)
}

/// Every ref reachable from one side of a ref-tree comparison, paired with its
/// target; empty when that side has no entry at `path`.
fn ref_tree_targets(
    repo: &gix::Repository,
    entry: Option<&(gix::objs::tree::EntryKind, ObjectId)>,
    path: &str,
) -> Result<Vec<(String, Target)>, Error> {
    let Some((kind, oid)) = entry else {
        return Ok(Vec::new());
    };
    if matches!(kind, gix::objs::tree::EntryKind::Blob) {
        return Ok(vec![(path.to_owned(), read_ref_target(repo, *oid)?)]);
    }
    let tree = repo.find_tree(*oid).map_err(Error::git)?;
    let mut updates = Vec::new();
    collect_ref_updates(repo, &tree, path, &mut updates)?;
    updates
        .into_iter()
        .map(|(name, contents)| Ok((name, parse_ref_contents(&contents)?)))
        .collect()
}

/// Read a captured ref blob as a ref target.
fn read_ref_target(repo: &gix::Repository, blob: ObjectId) -> Result<Target, Error> {
    let blob = repo.find_blob(blob).map_err(Error::git)?;
    parse_ref_contents(&blob.data)
}

/// Compose the summary line for a generated snapshot commit, or `None` when
/// `changed` has no changed parts.
fn snapshot_message(changed: Changes) -> Option<String> {
    let names = changed.names();
    let summary = match names.as_slice() {
        [] => return None,
        [one] => (*one).to_owned(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    };
    Some(format!("op: update {summary}"))
}

/// Restore repository refs and metadata from an operation-log commit.
///
/// The working tree and index are reset to the restored `HEAD`. Restoration
/// itself is recorded as a new operation commit, so `undo` can restore the
/// state that preceded the restore. The operation ref is advanced only after
/// all captured refs and files have been restored successfully.
pub fn restore(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId, Error> {
    let state = read(repo, commit)?;
    apply_state(repo, &state)?;
    append(repo, &format!("op: restore {commit}"))
}

/// Restore the state captured by the parent of the latest operation commit.
///
/// The working tree and index are reset to the restored `HEAD`. Like
/// [`restore`], undo is itself appended to the operation log. An initial
/// operation has no earlier snapshot and cannot be undone.
pub fn undo(repo: &gix::Repository) -> Result<ObjectId, Error> {
    let name = RefName::new(OP_REF).map_err(|_| Error::InvalidRef(OP_REF.to_owned()))?;
    let latest = operation_target(repo, &name)?.ok_or(Error::NothingToUndo)?;
    let parent = repo
        .find_commit(latest)
        .map_err(Error::git)?
        .parent_ids()
        .next()
        .map(|id| id.detach())
        .ok_or(Error::NothingToUndo)?;
    let state = read(repo, parent)?;
    apply_state(repo, &state)?;
    append(repo, &format!("op: undo {latest}"))
}

/// Resolve an operation commit specification using Git's revision parser.
///
/// This accepts full and abbreviated object IDs, as well as any commit
/// expression understood by `git rev-parse`, while requiring the result to be
/// a commit.
pub fn resolve_operation(repo: &gix::Repository, specification: &str) -> Result<ObjectId, Error> {
    let output = Command::new("git")
        .current_dir(repo.current_dir())
        .env("GIT_DIR", repo.git_dir())
        .args(["rev-parse", "--verify"])
        .arg(format!("{specification}^{{commit}}"))
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Err(Error::InvalidSnapshot(format!(
            "cannot resolve operation {specification:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let oid = ObjectId::from_hex(
        std::str::from_utf8(&output.stdout)
            .map_err(|error| Error::message(error.to_string()))?
            .trim()
            .as_bytes(),
    )
    .map_err(|error| {
        Error::InvalidSnapshot(format!("Git returned an invalid operation ID: {error}"))
    })?;
    read(repo, oid).map_err(|_| {
        Error::InvalidSnapshot(format!("{specification:?} is not an operation snapshot"))
    })?;
    Ok(oid)
}

/// Apply a captured state to the repository's refs and metadata files.
fn apply_state(repo: &gix::Repository, state: &RepositoryState) -> Result<(), Error> {
    let refs = repo.find_tree(state.r#refs.oid()).map_err(Error::git)?;
    let mut updates = Vec::new();
    collect_ref_updates(repo, &refs, "refs", &mut updates)?;
    let captured = updates.iter().map(|(name, _)| name.clone()).collect();
    let head = repo
        .find_reference("HEAD")
        .map_err(Error::git)?
        .target()
        .into_owned();
    apply_ref_updates(repo, updates, captured)?;
    set_head(repo, head)?;
    reset_worktree(repo)?;
    restore_metadata_file(repo, "config", state.config.map(|blob| blob.oid()))?;
    restore_metadata_file(
        repo,
        "description",
        state.description.map(|blob| blob.oid()),
    )?;
    Ok(())
}

/// Keep symbolic `HEAD` attached to the branch restored from the snapshot.
fn set_head(repo: &gix::Repository, head: Target) -> Result<(), Error> {
    let Target::Symbolic(branch) = head else {
        return Ok(());
    };
    let head = FullName::try_from("HEAD").expect("HEAD is a valid full ref name");
    repo.edit_reference(GixRefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                ..Default::default()
            },
            expected: PreviousValue::Any,
            new: Target::Symbolic(branch),
        },
        name: head,
        deref: false,
    })
    .map_err(Error::git)?;
    Ok(())
}

fn reset_worktree(repo: &gix::Repository) -> Result<(), Error> {
    let Some(workdir) = repo.workdir() else {
        return Ok(());
    };
    let status = Command::new("git")
        .current_dir(workdir)
        .env("GIT_DIR", repo.git_dir())
        .args(["reset", "--hard", "--quiet", "HEAD"])
        .status()
        .map_err(Error::git)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::message(format!(
            "git reset --hard HEAD failed with {status}"
        )))
    }
}

/// Flatten a nested refs tree into Git ref-file contents.
fn collect_ref_updates(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    updates: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), Error> {
    for entry in tree.decode().map_err(Error::git)?.entries {
        let name = entry.filename.to_str().map_err(|_| {
            Error::InvalidSnapshot(format!("non-UTF-8 reference path under {prefix}"))
        })?;
        let path = format!("{prefix}/{name}");
        match entry.mode.kind() {
            gix::objs::tree::EntryKind::Tree => {
                collect_ref_updates(
                    repo,
                    &repo.find_tree(entry.oid).map_err(Error::git)?,
                    &path,
                    updates,
                )?;
            }
            gix::objs::tree::EntryKind::Blob => {
                let blob = repo.find_blob(entry.oid).map_err(Error::git)?;
                updates.push((path, blob.data.to_vec()));
            }
            kind => {
                return Err(Error::InvalidSnapshot(format!(
                    "ref tree contains {kind:?}"
                )));
            }
        }
    }
    Ok(())
}

/// Replace captured refs with the refs represented by a snapshot.
fn apply_ref_updates(
    repo: &gix::Repository,
    updates: Vec<(String, Vec<u8>)>,
    captured: std::collections::BTreeSet<String>,
) -> Result<(), Error> {
    let mut edits = Vec::new();
    for reference in repo
        .references()
        .map_err(Error::git)?
        .all()
        .map_err(Error::git)?
    {
        let reference = reference.map_err(Error::git)?;
        let name = reference.name().as_bstr().to_str().map_err(|_| {
            Error::InvalidSnapshot("repository contains a non-UTF-8 ref name".to_owned())
        })?;
        if is_captured_ref(name.as_bytes()) && !captured.contains(name) {
            let name = FullName::try_from(name).map_err(|_| Error::InvalidRef(name.to_owned()))?;
            edits.push(GixRefEdit {
                change: Change::Delete {
                    expected: PreviousValue::MustExistAndMatch(reference.target().into_owned()),
                    log: RefLog::AndReference,
                },
                name,
                deref: false,
            });
        }
    }
    for (name, contents) in updates {
        let name =
            FullName::try_from(name.as_str()).map_err(|_| Error::InvalidRef(name.clone()))?;
        let target = parse_ref_contents(&contents)?;
        let expected = repo
            .try_find_reference(name.as_ref())
            .map_err(Error::git)?
            .map_or(PreviousValue::MustNotExist, |reference| {
                PreviousValue::MustExistAndMatch(reference.target().into_owned())
            });
        edits.push(GixRefEdit {
            change: Change::Update {
                log: LogChange::default(),
                expected,
                new: target,
            },
            name,
            deref: false,
        });
    }
    let committer = repo.committer().transpose().map_err(Error::git)?;
    repo.edit_references_as(edits, committer)
        .map_err(Error::git)?;
    Ok(())
}

/// Parse a serialized direct or symbolic Git ref target.
fn parse_ref_contents(contents: &[u8]) -> Result<Target, Error> {
    let contents = contents.strip_suffix(b"\n").unwrap_or(contents);
    if let Some(target) = contents.strip_prefix(b"ref: ") {
        let target = std::str::from_utf8(target)
            .map_err(|_| Error::InvalidSnapshot("symbolic ref is not UTF-8".to_owned()))?;
        return Ok(Target::Symbolic(FullName::try_from(target).map_err(
            |_| Error::InvalidSnapshot("invalid symbolic ref target".to_owned()),
        )?));
    }
    ObjectId::from_hex(contents)
        .map(Target::Object)
        .map_err(|_| Error::InvalidSnapshot("invalid ref object ID".to_owned()))
}

/// Restore or remove one repository metadata file from a snapshot blob.
fn restore_metadata_file(
    repo: &gix::Repository,
    name: &str,
    blob: Option<ObjectId>,
) -> Result<(), Error> {
    let path = repo.common_dir().join(name);
    match blob {
        Some(blob) => {
            let data = repo.find_blob(blob).map_err(Error::git)?.data.to_vec();
            fs::write(path, data).map_err(Error::git)?;
        }
        None => match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::git(error)),
        },
    }
    Ok(())
}

/// Process one `reference-transaction` hook invocation.
///
/// Only the committed phase attempts a snapshot, and only a transaction that
/// actually changes the captured refs, config, or description produces a new
/// commit; a captured transaction that leaves the state unchanged is a no-op.
/// Transactions that contain only excluded refs are ignored. Updating
/// [`OP_REF`] alone does not trigger a snapshot, which prevents the
/// operation-log update from recursively invoking itself when Git runs hooks
/// for ref transactions.
///
/// Git invokes this hook with `preparing`, `prepared`, `committed`, or
/// `aborted`; the non-committed phases do not write an operation commit.
///
/// # Examples
///
/// The hook's preparatory phase does not write an operation commit. This makes
/// it safe for Git to invoke the hook before the ref transaction is complete.
///
/// ```
/// let unique = std::time::SystemTime::now()
///     .duration_since(std::time::UNIX_EPOCH)
///     .expect("system clock is after the Unix epoch")
///     .as_nanos();
/// let root = std::env::temp_dir().join(format!(
///     "git-op-reference-transaction-{}-{unique}",
///     std::process::id()
/// ));
/// std::fs::create_dir(&root).expect("create temporary repository directory");
/// let repo = gix::init(&root).expect("initialize repository");
/// git_op::reference_transaction(&repo, "prepared", b"transaction input\\n")
///     .expect("process prepared transaction");
/// assert!(repo
///     .try_find_reference(git_op::OP_REF)
///     .expect("look up operation ref")
///     .is_none());
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn reference_transaction(
    repo: &gix::Repository,
    phase: &str,
    input: &[u8],
) -> Result<(), Error> {
    match phase {
        "preparing" | "prepared" | "aborted" => Ok(()),
        "committed" => {
            if transaction_changes_captured_refs(input)? {
                append_internal(repo, CommitMessage::Generated, AppendOptions::default())?;
            }
            Ok(())
        }
        phase => Err(Error::InvalidPhase(phase.to_owned())),
    }
}

/// Determine whether hook input includes a captured ref update.
///
/// Each non-empty line must contain Git's three whitespace-separated fields:
/// old object ID, new object ID, and reference name. Both LF and CRLF input
/// are accepted. The parser deliberately returns a boolean rather than the
/// affected names because the hook only needs to decide whether to append a
/// snapshot.
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
        return Ok(false);
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEMP_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

    /// Own a temporary repository and remove it when the test finishes.
    struct TemporaryRepository {
        root: PathBuf,
        repo: gix::Repository,
    }

    impl TemporaryRepository {
        /// Create a repository with deterministic commit identity settings.
        fn new() -> Self {
            let root = loop {
                let sequence = NEXT_TEMP_REPOSITORY.fetch_add(1, Ordering::Relaxed);
                let candidate = std::env::temp_dir()
                    .join(format!("git-op-test-{}-{sequence}", std::process::id()));
                match fs::create_dir(&candidate) {
                    Ok(()) => break candidate,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create temporary repository directory: {error}"),
                }
            };
            gix::init(&root).expect("initialize temporary repository");
            for (key, value) in [("user.name", "git-op test"), ("user.email", "git-op@test")] {
                let status = Command::new("git")
                    .current_dir(&root)
                    .args(["config", "--local", key, value])
                    .status()
                    .expect("configure temporary repository");
                assert!(status.success(), "git config {key} failed with {status}");
            }
            let repo = gix::open(&root).expect("reopen configured temporary repository");
            Self { root, repo }
        }
    }

    impl Drop for TemporaryRepository {
        /// Remove the temporary repository after the test completes.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Verify that the test repository can create a snapshot commit.
    #[test]
    fn temporary_repository_has_commit_identity() {
        let temporary = TemporaryRepository::new();
        append(&temporary.repo, "test snapshot").expect("append snapshot");
    }

    #[test]
    fn generated_commit_records_invoking_command() {
        let temporary = TemporaryRepository::new();
        let tree = temporary
            .repo
            .write_buf(Kind::Tree, b"")
            .expect("write empty tree");
        let signature = gix::actor::Signature {
            name: "git-op test".into(),
            email: "git-op@test".into(),
            time: gix::date::Time {
                seconds: 1_700_000_000,
                offset: 0,
            },
        };
        let commit = write_commit(
            &temporary.repo,
            tree,
            None,
            "op: capture initial repository state",
            Some(&InvokedBy("git commit".to_owned())),
            &signature,
            &signature,
            false,
        )
        .expect("write operation commit");
        assert_eq!(
            temporary
                .repo
                .find_commit(commit)
                .expect("find operation commit")
                .message_raw_sloppy(),
            b"op: capture initial repository state\n\nInvoked-by: git commit\n"
        );
    }

    /// Run Git in a temporary repository and require successful completion.
    fn git(temporary: &TemporaryRepository, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(&temporary.root)
            .args(args)
            .status()
            .expect("run Git");
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    fn git_output(temporary: &TemporaryRepository, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&temporary.root)
            .args(args)
            .output()
            .expect("run Git");
        assert!(
            output.status.success(),
            "git {args:?} failed with {}",
            output.status
        );
        String::from_utf8(output.stdout).expect("Git output is UTF-8")
    }

    /// Verify that restoring a branch ref also updates its symbolic `HEAD` target.
    #[test]
    fn restore_updates_head_for_current_branch() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let first = temporary
            .repo
            .head_id()
            .expect("read first commit")
            .detach();
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
        let second = temporary
            .repo
            .head_id()
            .expect("read second commit")
            .detach();
        assert_ne!(first, second);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");
        std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
        std::fs::write(temporary.root.join("staged"), b"staged\n").expect("write staged file");
        git(&temporary, &["add", "staged"]);
        assert_eq!(
            git_output(&temporary, &["diff", "--cached", "--name-only"]),
            "staged\n"
        );

        restore(&temporary.repo, initial).expect("restore initial snapshot");

        assert_eq!(temporary.repo.head_id().expect("read restored HEAD"), first);
        assert_eq!(
            fs::read_to_string(temporary.root.join("tracked")).expect("read restored file"),
            "initial\n"
        );
        assert_eq!(git_output(&temporary, &["status", "--porcelain"]), "");
        assert_eq!(
            temporary
                .repo
                .head_name()
                .expect("read HEAD name")
                .map(|name| name.to_string()),
            Some("refs/heads/main".to_owned())
        );
    }

    /// Verify that restoring a detached repository leaves `HEAD` detached at its current commit.
    #[test]
    fn restore_preserves_detached_head_commit() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
        let first = temporary
            .repo
            .head_id()
            .expect("read first commit")
            .detach();
        git(&temporary, &["checkout", "--detach", &first.to_string()]);
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
        let second = temporary
            .repo
            .head_id()
            .expect("read second commit")
            .detach();
        assert_ne!(first, second);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");
        std::fs::write(temporary.root.join("staged"), b"staged\n").expect("write staged file");
        git(&temporary, &["add", "staged"]);

        restore(&temporary.repo, initial).expect("restore initial snapshot");

        assert_eq!(
            temporary.repo.head_id().expect("read restored HEAD"),
            second
        );
        assert!(
            temporary
                .repo
                .head_name()
                .expect("read HEAD name")
                .is_none()
        );
    }

    /// Verify that generated messages identify changed snapshot components.
    #[test]
    fn generated_snapshot_message_identifies_changes() {
        let temporary = TemporaryRepository::new();
        let initial = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append initial snapshot");
        assert_eq!(
            temporary
                .repo
                .find_commit(initial)
                .expect("find initial snapshot")
                .message()
                .expect("read initial message")
                .summary()
                .as_bytes(),
            b"op: capture initial repository state"
        );

        fs::write(
            temporary.repo.common_dir().join("description"),
            b"example\n",
        )
        .expect("write description");
        Command::new("git")
            .current_dir(&temporary.root)
            .args(["update-ref", "refs/heads/example", &initial.to_string()])
            .status()
            .expect("update example ref")
            .success()
            .then_some(())
            .expect("update example ref succeeds");
        let update = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append changed snapshot");
        assert_eq!(
            temporary
                .repo
                .find_commit(update)
                .expect("find changed snapshot")
                .message()
                .expect("read changed message")
                .summary()
                .as_bytes(),
            b"op: update refs and description"
        );
    }

    /// Verify that a single changed part produces a singular update message.
    #[test]
    fn generated_snapshot_message_reports_single_change() {
        let temporary = TemporaryRepository::new();
        let initial = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append initial snapshot");
        Command::new("git")
            .current_dir(&temporary.root)
            .args(["update-ref", "refs/heads/example", &initial.to_string()])
            .status()
            .expect("update example ref")
            .success()
            .then_some(())
            .expect("update example ref succeeds");
        let update = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append changed snapshot");
        assert_eq!(
            temporary
                .repo
                .find_commit(update)
                .expect("find changed snapshot")
                .message()
                .expect("read changed message")
                .summary()
                .as_bytes(),
            b"op: update refs"
        );
    }

    /// Verify that three changed parts are joined with an Oxford comma.
    #[test]
    fn generated_snapshot_message_reports_three_changes() {
        let temporary = TemporaryRepository::new();
        let initial = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append initial snapshot");
        Command::new("git")
            .current_dir(&temporary.root)
            .args(["update-ref", "refs/heads/example", &initial.to_string()])
            .status()
            .expect("update example ref")
            .success()
            .then_some(())
            .expect("update example ref succeeds");
        let config_path = temporary.repo.common_dir().join("config");
        let mut config = fs::read(&config_path).expect("read config");
        config.extend_from_slice(b"[extra]\n\tflag = true\n");
        fs::write(&config_path, config).expect("write config");
        fs::write(
            temporary.repo.common_dir().join("description"),
            b"example\n",
        )
        .expect("write description");
        let update = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append changed snapshot");
        assert_eq!(
            temporary
                .repo
                .find_commit(update)
                .expect("find changed snapshot")
                .message()
                .expect("read changed message")
                .summary()
                .as_bytes(),
            b"op: update refs, config, and description"
        );
    }

    /// Verify that a generated append with nothing to record neither creates a
    /// commit nor advances the operation ref.
    #[test]
    fn generated_append_is_noop_when_nothing_changed() {
        let temporary = TemporaryRepository::new();
        let initial = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append initial snapshot");
        let repeat = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append unchanged snapshot");
        assert_eq!(repeat, initial);
        assert_eq!(
            temporary
                .repo
                .find_reference(OP_REF)
                .expect("read operation ref")
                .target()
                .try_id(),
            Some(initial.as_ref())
        );
    }

    /// Verify that `changes` reports `None` for the initial snapshot and the
    /// correct parts changed for a later one.
    #[test]
    fn changes_reports_parts_changed_since_parent() {
        let temporary = TemporaryRepository::new();
        let initial = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append initial snapshot");
        assert_eq!(
            changes(&temporary.repo, initial).expect("compute changes"),
            None
        );

        Command::new("git")
            .current_dir(&temporary.root)
            .args(["update-ref", "refs/heads/example", &initial.to_string()])
            .status()
            .expect("update example ref")
            .success()
            .then_some(())
            .expect("update example ref succeeds");
        fs::write(
            temporary.repo.common_dir().join("description"),
            b"example\n",
        )
        .expect("write description");
        let update = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append changed snapshot");
        let changed = changes(&temporary.repo, update)
            .expect("compute changes")
            .expect("updated snapshot has a parent");
        assert_eq!(
            changed,
            Changes {
                r#refs: true,
                config: false,
                description: true,
            }
        );
    }

    /// Verify that operation, remote, and pseudo refs are excluded.
    #[test]
    fn ref_filter_excludes_operation_and_remote_refs() {
        assert!(is_captured_ref(b"refs/heads/main"));
        assert!(is_captured_ref(b"refs/tags/v1"));
        assert!(!is_captured_ref(OP_REF.as_bytes()));
        assert!(!is_captured_ref(b"refs/remotes/origin/main"));
        assert!(!is_captured_ref(b"HEAD"));
    }

    /// Verify that conflicting ref paths are rejected.
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

    /// Verify that hook input identifies only captured refs.
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

    /// Verify that every non-committed hook phase is accepted without a snapshot.
    #[test]
    fn hook_accepts_non_committed_phases() {
        let temporary = TemporaryRepository::new();
        let input = b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/main\n";
        for phase in ["preparing", "prepared", "aborted"] {
            reference_transaction(&temporary.repo, phase, input).expect("process hook phase");
        }
        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none()
        );
    }

    /// Verify that an empty committed transaction leaves the operation log unchanged.
    #[test]
    fn hook_accepts_empty_committed_transaction() {
        let temporary = TemporaryRepository::new();
        reference_transaction(&temporary.repo, "committed", b"")
            .expect("process empty transaction");
        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none()
        );
    }

    /// Verify that empty hook input is treated as a no-op.
    #[test]
    fn hook_parser_accepts_empty_transactions() {
        assert!(!transaction_changes_captured_refs(b"\n").expect("empty transaction"));
        assert!(!transaction_changes_captured_refs(b"\r\n").expect("empty transaction"));
    }
}
