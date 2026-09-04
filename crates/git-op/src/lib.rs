//! An operation log for a repository's local refs and Git metadata.
//!
//! Snapshots are committed to [`OP_REF`]. The operation ref, remote refs, and
//! pseudo references such as `HEAD` are excluded from snapshots.

use std::{
    collections::{BTreeMap, HashSet},
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

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
pub use config::{install_global, install_local, uninstall_global, uninstall_local};
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

/// The operation recorded in a `Git-op` commit-message trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Reapply the state before the latest logical operation.
    Undo,
    /// Reapply the next logical operation after an undo.
    Redo,
    /// Restore one selected snapshot.
    Restore,
}

impl Action {
    /// The trailer keyword for this action.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::Restore => "restore",
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for Action {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.as_bytes())
    }
}

impl TryFrom<&[u8]> for Action {
    type Error = Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        match value {
            b"undo" => Ok(Self::Undo),
            b"redo" => Ok(Self::Redo),
            b"restore" => Ok(Self::Restore),
            _ => Err(Error::UnknownAction(
                String::from_utf8_lossy(value).into_owned(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationMetadata {
    actions: Vec<(Action, ObjectId)>,
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
    /// A `Git-op` trailer keyword was not `undo`, `redo`, or `restore`.
    #[error("unknown operation action {0}")]
    UnknownAction(String),
    /// The operation ref is not a direct reference to a commit.
    #[error("{OP_REF} must directly reference a commit")]
    InvalidOperationRef,
    /// A reference-transaction hook received malformed input.
    #[error("invalid reference-transaction input: {0}")]
    InvalidHookInput(String),
    /// The existing hook cannot be merged: unreadable, or replaced concurrently.
    #[error("refusing to overwrite existing hook {0}")]
    HookExists(PathBuf),
    /// The operation log has no earlier snapshot to restore.
    #[error("nothing to undo")]
    NothingToUndo,
    /// No operation was undone and can be redone.
    #[error("nothing to redo")]
    NothingToRedo,
    /// A snapshot contains a reference or metadata value that cannot be restored.
    #[error("invalid snapshot content: {0}")]
    InvalidSnapshot(String),
    /// A snapshot ref targets an object that is missing from the object
    /// database, so the restore cannot install refs onto it.
    #[error("cannot restore: ref {ref_name} in snapshot targets missing object {oid}")]
    PrunedObject {
        /// The full name of the ref whose target is missing.
        ref_name: String,
        /// The object ID that no longer exists.
        oid: ObjectId,
    },
    /// A snapshot branch targets an object that does not peel to a usable commit.
    #[error(
        "cannot restore: ref {ref_name} in snapshot targets {oid}, which does not peel to a commit with a tree"
    )]
    UnusableObject {
        /// The full name of the ref whose target is unusable.
        ref_name: String,
        /// The object ID that cannot back a checkout.
        oid: ObjectId,
    },
    /// Restoring would overwrite untracked files or discard uncommitted
    /// changes, so the blocked paths are reported instead.
    #[error("cannot restore: {}", collision_list(.collisions))]
    OverwrittenPaths {
        /// The blocked paths, ordered by name.
        collisions: Vec<PathCollision>,
    },
}

/// A path whose untracked file or uncommitted changes a restore would overwrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCollision {
    /// The worktree-relative path.
    pub path: String,
    /// Whether an untracked file would be overwritten, as opposed to
    /// uncommitted changes being discarded.
    pub untracked: bool,
}

/// Render the collision list of an [`Error::OverwrittenPaths`] message.
fn collision_list(collisions: &[PathCollision]) -> String {
    collisions
        .iter()
        .map(|collision| {
            if collision.untracked {
                format!("{} (untracked file)", collision.path)
            } else {
                format!("{} (uncommitted changes)", collision.path)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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
fn read_repository_file(
    repo: &gix::Repository,
    file: MetadataFile,
) -> Result<Option<Vec<u8>>, Error> {
    let path = repo.common_dir().join(file.as_str());
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
        .map_err(|_error| Error::InvalidOperationRef)?;
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
            .map_err(|_error| Error::InvalidRef(raw_name.to_string()))?;
        ref_values.insert(name.to_owned(), ref_contents(&reference));
    }

    let ref_tree = write_ref_tree(repo, &ref_values)?;
    let config = read_repository_file(repo, MetadataFile::Config)?;
    let description = read_repository_file(repo, MetadataFile::Description)?;
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
    append_internal(repo, CommitMessage::Explicit(message), options).map(|(operation, _)| operation)
}

/// The result of recording repository state on the operation log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapResult {
    /// This call appended the snapshot.
    Appended(ObjectId),
    /// The existing operation-log tip already reflected the repository state.
    Current(ObjectId),
}

impl SnapResult {
    /// The operation-log commit reflecting the repository state.
    pub const fn operation(self) -> ObjectId {
        match self {
            Self::Appended(operation) | Self::Current(operation) => operation,
        }
    }
}

/// The outcome of a [`snap`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapOutcome {
    /// The repository state was recorded, or the log already reflected it.
    Recorded(SnapResult),
    /// HEAD is detached, so nothing was recorded: normal ref writes carry no
    /// branch context worth logging.
    Detached,
}

/// Record repository state that is not yet on the operation log onto
/// [`OP_REF`], with a generated commit message summarizing the changes.
///
/// A no-op when the operation log already reflects the repository state, and
/// a silent no-op when HEAD is detached. The returned [`SnapOutcome`] reports
/// whether this call appended anything, so callers never have to infer it
/// from the ref value.
pub fn snap(repo: &gix::Repository) -> Result<SnapOutcome, Error> {
    if repo.head_name().map_err(Error::git)?.is_none() {
        return Ok(SnapOutcome::Detached);
    }
    let (operation, appended) =
        append_internal(repo, CommitMessage::Generated, AppendOptions::default())?;
    let result = if appended {
        SnapResult::Appended(operation)
    } else {
        SnapResult::Current(operation)
    };
    Ok(SnapOutcome::Recorded(result))
}

fn append_internal(
    repo: &gix::Repository,
    requested_message: CommitMessage<'_>,
    options: AppendOptions,
) -> Result<(ObjectId, bool), Error> {
    let refs = GixRefStore::new(repo);
    let name = RefName::new(OP_REF).map_err(|_error| Error::InvalidRef(OP_REF.to_owned()))?;
    let invoked_by = matches!(requested_message, CommitMessage::Generated)
        .then(invocation::detect)
        .flatten();

    for _ in 0..MAX_APPEND_ATTEMPTS {
        let parent = operation_target(repo, &name)?;
        let state = capture(repo)?;
        let message = match (requested_message, parent) {
            (CommitMessage::Explicit(message), _) => message.to_owned(),
            (CommitMessage::Generated, None) => "op: capture initial repository state".to_owned(),
            (CommitMessage::Generated, Some(parent)) => {
                let changed =
                    SnapshotEntries::from_state(&state).diff(&SnapshotEntries::read(repo, parent)?);
                let Some(message) = snapshot_message(&changed) else {
                    return Ok((parent, false));
                };
                message
            }
        };
        // Signing and identity are write-only prerequisites: resolve them
        // only now that a commit will actually be written, so a no-op snap
        // does not require configured commit identity.
        let signing = commit_signing_enabled(repo)?;
        let author = match options.author.clone() {
            Some(author) => author,
            None => refs.author().map_err(Error::git)?,
        };
        let committer = match options.committer.clone() {
            Some(committer) => committer,
            None => refs.signature().map_err(Error::git)?,
        };
        let tree = serialize(repo, &state)?;
        let commit_id = write_commit(
            repo,
            CommitTreeRequest {
                tree,
                parent,
                message: &message,
                invoked_by: invoked_by.as_ref(),
                author: &author,
                committer: &committer,
                signing,
            },
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
            Ok(()) => return Ok((commit_id, true)),
            Err(ApplyError::LostRace { .. }) => continue,
            Err(ApplyError::Backend(error)) => return Err(Error::git(error)),
        }
    }
    Err(Error::LostRace(MAX_APPEND_ATTEMPTS))
}

/// Flatten a detected invocation into a single-line trailer value.
fn trailer_text(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// Write a snapshot commit through Git, optionally letting Git sign it.
///
/// `git commit-tree` is used rather than constructing the commit object
/// directly so Git's configured signing implementation can add a `gpgsig`
/// header. It does not invoke commit hooks.
struct CommitTreeRequest<'a> {
    tree: ObjectId,
    parent: Option<ObjectId>,
    message: &'a str,
    invoked_by: Option<&'a InvokedBy>,
    author: &'a gix::actor::Signature,
    committer: &'a gix::actor::Signature,
    signing: bool,
}

fn write_commit(repo: &gix::Repository, commit: CommitTreeRequest<'_>) -> Result<ObjectId, Error> {
    let mut command = Command::new("git");
    command
        .current_dir(repo.current_dir())
        .arg("commit-tree")
        .arg(commit.tree.to_hex().to_string());
    if let Some(parent) = commit.parent {
        command.args(["-p", &parent.to_hex().to_string()]);
    }
    command.arg("-m").arg(commit.message);
    if let Some(invoked_by) = commit.invoked_by {
        command
            .arg("-m")
            .arg(format!("Invoked-by: {}", trailer_text(invoked_by.as_str())));
    }
    if commit.signing {
        command.arg("-S");
    }
    let author = format_signature(commit.author)?;
    let committer = format_signature(commit.committer)?;
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

/// A repository metadata file captured in snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataFile {
    /// The repository's `config` file.
    Config,
    /// The repository's `description` file.
    Description,
}

impl MetadataFile {
    /// The file's name in the repository's common directory.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Description => "description",
        }
    }
}

impl fmt::Display for MetadataFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A component of the captured repository state, named by [`Changes::names`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// The captured refs tree.
    Refs,
    /// A captured metadata file.
    File(MetadataFile),
}

impl Component {
    /// The component's name in log output and JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refs => "refs",
            Self::File(file) => file.as_str(),
        }
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The parts of the captured state that a snapshot changed relative to its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changes {
    /// The captured refs changed.
    Refs(Vec<RefChange>),
    /// The captured `config` file changed.
    Config,
    /// The captured `description` file changed.
    Description,
}

impl Changes {
    /// The changed components, in capture order.
    pub fn names(changes: &[Self]) -> Vec<Component> {
        changes
            .iter()
            .map(|change| match change {
                Self::Refs(_) => Component::Refs,
                Self::Config => Component::File(MetadataFile::Config),
                Self::Description => Component::File(MetadataFile::Description),
            })
            .collect()
    }

    /// The changed metadata files, in capture order.
    pub fn file_names(changes: &[Self]) -> Vec<MetadataFile> {
        changes
            .iter()
            .filter_map(|change| match change {
                Self::Config => Some(MetadataFile::Config),
                Self::Description => Some(MetadataFile::Description),
                Self::Refs(_) => None,
            })
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    fn diff(&self, previous: &Self) -> Vec<Changes> {
        let mut changes = Vec::new();
        if self.r#refs != previous.r#refs {
            changes.push(Changes::Refs(Vec::new()));
        }
        if self.config != previous.config {
            changes.push(Changes::Config);
        }
        if self.description != previous.description {
            changes.push(Changes::Description);
        }
        changes
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
/// assert_eq!(
///     git_op::Changes::names(&changed),
///     vec![git_op::Component::File(git_op::MetadataFile::Description)]
/// );
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn changes(repo: &gix::Repository, commit: ObjectId) -> Result<Option<Vec<Changes>>, Error> {
    let Some(parent) = parent_snapshot(repo, commit)? else {
        return Ok(None);
    };
    let current = SnapshotEntries::read(repo, commit)?;
    let previous = SnapshotEntries::read(repo, parent)?;
    let mut changes = current.diff(&previous);
    if let Some(Changes::Refs(refs)) = changes
        .iter_mut()
        .find(|change| matches!(change, Changes::Refs(_)))
    {
        *refs = ref_changes_between(repo, previous.r#refs, current.r#refs)?;
    }
    Ok(Some(changes))
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
    Ok(Some(ref_changes_between(
        repo,
        previous.r#refs,
        current.r#refs,
    )?))
}

/// Return the individual refs changed between two captured ref trees.
///
/// The tree IDs must identify the `refs` entries of serialized repository
/// states. Results are ordered by full ref name.
fn ref_changes_between(
    repo: &gix::Repository,
    previous: ObjectId,
    current: ObjectId,
) -> Result<Vec<RefChange>, Error> {
    let mut changes = Vec::new();
    diff_ref_trees(repo, previous, current, "refs", &mut changes)?;
    Ok(changes)
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
            .map_err(|_error| Error::InvalidSnapshot("non-UTF-8 reference path".to_owned()))?;
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
/// `changed` has no changed components.
fn snapshot_message(changed: &[Changes]) -> Option<String> {
    let names = Changes::names(changed);
    let summary = match names.as_slice() {
        [] => return None,
        [one] => one.to_string(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!(
            "{}, and {last}",
            rest.iter()
                .map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Some(format!("op: update {summary}"))
}

/// The result of a state-changing operation, including its logical target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionResult {
    /// The operation-log commit created by the action, or the existing tip for a no-op.
    pub operation: ObjectId,
    /// The operation whose captured state was selected.
    pub target: ObjectId,
    /// The snapshot that was applied.
    pub restored: ObjectId,
    /// The captured state components changed while applying the snapshot.
    /// An empty list means the selected state was already current and no log
    /// entry was appended.
    pub changes: Vec<Changes>,
}

/// A state transition that can be displayed without applying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionPlan {
    /// The action recorded in the operation trailer.
    pub action: Action,
    /// The operation identified by the transition.
    pub target: ObjectId,
    /// The snapshot that would become the repository state.
    pub restored: ObjectId,
}

/// Restore repository refs and metadata from an operation-log commit.
pub fn restore(repo: &gix::Repository, commit: ObjectId) -> Result<ObjectId, Error> {
    Ok(restore_action(repo, commit)?.operation)
}

/// Restore a selected snapshot and append a restore operation.
pub fn restore_action(repo: &gix::Repository, commit: ObjectId) -> Result<ActionResult, Error> {
    apply_action(
        repo,
        ActionPlan {
            action: Action::Restore,
            target: commit,
            restored: commit,
        },
    )
}

/// Restore the previous logical state and append an undo operation.
pub fn undo(repo: &gix::Repository) -> Result<ObjectId, Error> {
    Ok(undo_action(repo)?.operation)
}

/// Select the next undo transition without changing the repository.
pub fn plan_undo(repo: &gix::Repository) -> Result<ActionPlan, Error> {
    let tip = current_operation(repo)?.ok_or(Error::NothingToUndo)?;
    let metadata = operation_metadata(repo, tip)?;
    let action = metadata.actions.first().map(|(action, _)| *action);
    let (target, restored) = match action {
        None | Some(Action::Restore) => (
            tip,
            parent_snapshot(repo, tip)?.ok_or(Error::NothingToUndo)?,
        ),
        Some(Action::Undo) => {
            let restored = metadata
                .actions
                .iter()
                .find_map(|(action, target)| (*action == Action::Restore).then_some(*target))
                .ok_or_else(|| missing_trailer(tip, "Git-op: restore"))?;
            (
                restored,
                parent_snapshot(repo, restored)?.ok_or(Error::NothingToUndo)?,
            )
        }
        Some(Action::Redo) => {
            let target = metadata
                .actions
                .iter()
                .find_map(|(action, target)| (*action == Action::Redo).then_some(*target))
                .ok_or_else(|| missing_trailer(tip, "Git-op: redo"))?;
            (
                target,
                parent_snapshot(repo, target)?.ok_or(Error::NothingToUndo)?,
            )
        }
    };
    Ok(ActionPlan {
        action: Action::Undo,
        target,
        restored,
    })
}

/// Apply the next undo transition and append its operation trailer.
pub fn undo_action(repo: &gix::Repository) -> Result<ActionResult, Error> {
    let operation = current_operation(repo)?.ok_or(Error::NothingToUndo)?;
    let metadata = operation_metadata(repo, operation)?;
    let plan = plan_undo(repo)?;
    let target = if metadata.actions.first().map(|(action, _)| *action) == Some(Action::Restore) {
        operation
    } else {
        plan.target
    };
    apply_action_with_trailers(repo, plan, Some(target), Some(plan.restored))
}

/// Select the next redo transition without changing the repository.
pub fn plan_redo(repo: &gix::Repository) -> Result<ActionPlan, Error> {
    let tip = current_operation(repo)?.ok_or(Error::NothingToRedo)?;
    let metadata = operation_metadata(repo, tip)?;
    let target = match metadata.actions.first() {
        Some((Action::Undo, target)) => Some(*target),
        Some((Action::Redo, target)) => next_redo_target(repo, tip, *target)?,
        _ => None,
    }
    .ok_or(Error::NothingToRedo)?;
    Ok(ActionPlan {
        action: Action::Redo,
        target,
        restored: target,
    })
}

/// Apply the next redo transition and append its operation trailer.
pub fn redo_action(repo: &gix::Repository) -> Result<ActionResult, Error> {
    apply_action(repo, plan_redo(repo)?)
}

fn apply_action(repo: &gix::Repository, plan: ActionPlan) -> Result<ActionResult, Error> {
    let restored = matches!(plan.action, Action::Redo).then_some(plan.restored);
    apply_action_with_trailers(repo, plan, None, restored)
}

fn apply_action_with_trailers(
    repo: &gix::Repository,
    plan: ActionPlan,
    primary_target: Option<ObjectId>,
    restored: Option<ObjectId>,
) -> Result<ActionResult, Error> {
    let target_entries = SnapshotEntries::read(repo, plan.restored)?;
    let current = capture(repo)?;
    let Some(operation) = current_operation(repo)? else {
        return Err(Error::InvalidOperationRef);
    };
    let current_entries = SnapshotEntries::from_state(&current);
    let mut changes = current_entries.diff(&target_entries);
    if let Some(Changes::Refs(refs)) = changes
        .iter_mut()
        .find(|change| matches!(change, Changes::Refs(_)))
    {
        *refs = ref_changes_between(repo, current_entries.r#refs, target_entries.r#refs)?;
    }
    let state = read(repo, plan.restored)?;
    verify_snapshot_objects(repo, &state)?;
    if current_entries == target_entries {
        return Ok(ActionResult {
            operation,
            target: plan.target,
            restored: plan.restored,
            changes,
        });
    }
    ensure_no_local_collisions(repo, &state)?;
    apply_state(repo, &state)?;
    let primary_target = primary_target.unwrap_or(plan.target);
    let mut message = format!(
        "op: {} {}\n\nGit-op: {}:{}",
        plan.action, primary_target, plan.action, primary_target
    );
    if let Some(restored) = restored {
        message.push_str(&format!("\nGit-op: restore:{restored}"));
    }
    let operation = append(repo, &message)?;
    Ok(ActionResult {
        operation,
        target: plan.target,
        restored: plan.restored,
        changes,
    })
}

fn missing_trailer(commit: ObjectId, trailer: &str) -> Error {
    Error::InvalidSnapshot(format!("operation {commit} is missing {trailer}"))
}

fn current_operation(repo: &gix::Repository) -> Result<Option<ObjectId>, Error> {
    let name = RefName::new(OP_REF).map_err(|_error| Error::InvalidRef(OP_REF.to_owned()))?;
    operation_target(repo, &name)
}

fn operation_metadata(
    repo: &gix::Repository,
    commit: ObjectId,
) -> Result<OperationMetadata, Error> {
    let message = repo
        .find_commit(commit)
        .map_err(Error::git)?
        .message_raw_sloppy()
        .to_vec();
    let mut actions = Vec::new();
    for line in message.split(|byte| *byte == b'\n') {
        let Some(value) = line.strip_prefix(b"Git-op: ") else {
            continue;
        };
        let Some(separator) = value.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let (action, value) = value.split_at(separator);
        let Ok(action) = Action::try_from(action) else {
            continue;
        };
        let Some(value) = value.strip_prefix(b":") else {
            continue;
        };
        if let Some(target) = parse_operation_id(value)? {
            actions.push((action, target));
        }
    }
    Ok(OperationMetadata { actions })
}

fn parse_operation_id(value: &[u8]) -> Result<Option<ObjectId>, Error> {
    let value = value.strip_suffix(b"\r").unwrap_or(value);
    if value.is_empty() {
        return Ok(None);
    }
    ObjectId::from_hex(value)
        .map(Some)
        .map_err(|error| Error::InvalidSnapshot(format!("invalid operation trailer: {error}")))
}

fn next_redo_target(
    repo: &gix::Repository,
    redo: ObjectId,
    undone: ObjectId,
) -> Result<Option<ObjectId>, Error> {
    let parent = parent_snapshot(repo, redo)?;
    let Some(parent) = parent else {
        return Ok(None);
    };
    let metadata = operation_metadata(repo, parent)?;
    if !metadata.actions.contains(&(Action::Undo, undone)) {
        return Ok(None);
    }
    let Some(previous_undo) = parent_snapshot(repo, parent)? else {
        return Ok(None);
    };
    Ok(operation_metadata(repo, previous_undo)?
        .actions
        .iter()
        .find_map(|(action, target)| (*action == Action::Undo).then_some(*target)))
}

/// The first parent of an operation snapshot, or `None` for the initial one.
pub fn parent_operation(
    repo: &gix::Repository,
    commit: ObjectId,
) -> Result<Option<ObjectId>, Error> {
    parent_snapshot(repo, commit)
}

/// Every ref a snapshot captured, each as a `Created` change.
///
/// The initial snapshot has no parent to diff against, so its captured refs
/// are reported as creations.
pub fn captured_refs(repo: &gix::Repository, commit: ObjectId) -> Result<Vec<RefChange>, Error> {
    let state = read(repo, commit)?;
    let refs = repo.find_tree(state.r#refs.oid()).map_err(Error::git)?;
    let mut updates = Vec::new();
    collect_ref_updates(repo, &refs, "refs", &mut updates)?;
    updates
        .into_iter()
        .map(|(name, contents)| {
            Ok(RefChange {
                name,
                kind: RefChangeKind::Created(parse_ref_contents(&contents)?),
            })
        })
        .collect()
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
    read(repo, oid).map_err(|_error| {
        Error::InvalidSnapshot(format!("{specification:?} is not an operation snapshot"))
    })?;
    Ok(oid)
}

/// Verify that every object targeted by a snapshot's refs still exists.
///
/// Ref targets captured in a snapshot are plain bytes that do not keep their
/// objects reachable, so garbage collection can prune them after the snapshot
/// was captured. Applying such a snapshot would move refs onto missing
/// objects and leave the repository unusable.
fn verify_snapshot_objects(repo: &gix::Repository, state: &RepositoryState) -> Result<(), Error> {
    let refs = repo.find_tree(state.r#refs.oid()).map_err(Error::git)?;
    let mut updates = Vec::new();
    collect_ref_updates(repo, &refs, "refs", &mut updates)?;
    for (name, contents) in updates {
        if let Target::Object(oid) = parse_ref_contents(&contents)? {
            verify_object(repo, &name, oid)?;
        }
    }
    Ok(())
}

/// Verify one snapshot ref target can back the restored repository.
fn verify_object(repo: &gix::Repository, ref_name: &str, oid: ObjectId) -> Result<(), Error> {
    let Some(object) = repo.try_find_object(oid).map_err(Error::git)? else {
        return Err(Error::PrunedObject {
            ref_name: ref_name.to_owned(),
            oid,
        });
    };
    if !ref_name.starts_with("refs/heads/") {
        return Ok(());
    }
    let commit = object
        .peel_to_kind(gix::objs::Kind::Commit)
        .map_err(|_error| Error::UnusableObject {
            ref_name: ref_name.to_owned(),
            oid,
        })?;
    commit
        .peel_to_kind(gix::objs::Kind::Tree)
        .map_err(|_error| Error::UnusableObject {
            ref_name: ref_name.to_owned(),
            oid,
        })?;
    Ok(())
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
    // Metadata is restored only after the ref transaction and the worktree
    // reset: `reset_worktree` shells out to `git read-tree`, which reads the
    // repository's config, so a snapshot config must not become active while
    // the index and worktree are being rewritten. A snapshot's `core.worktree`
    // or repository-format settings could otherwise redirect or break the
    // destructive checkout after refs have already moved.
    restore_metadata_file(
        repo,
        MetadataFile::Config,
        state.config.map(|blob| blob.oid()),
    )?;
    restore_metadata_file(
        repo,
        MetadataFile::Description,
        state.description.map(|blob| blob.oid()),
    )?;
    Ok(())
}

/// Keep symbolic `HEAD` attached to the branch restored from the snapshot.
fn set_head(repo: &gix::Repository, head: Target) -> Result<(), Error> {
    let Target::Symbolic(branch) = head else {
        return Ok(());
    };
    let head = FullName::try_from("HEAD").map_err(Error::git)?;
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

/// Sync the index and working tree to the resolved `HEAD` commit.
///
/// This performs the same index and worktree update as `git reset --hard`,
/// but through `git read-tree --reset -u HEAD` rather than `git reset`, since
/// `reset` also rewrites the current branch ref to the same value it already
/// holds. That redundant ref update is otherwise indistinguishable from a
/// real ref change to the `reference-transaction` hook, which would record a
/// spurious operation snapshot for `restore` and `undo` themselves. Reading
/// the tree instead touches only the index and worktree, so it cannot fire
/// ref-update hooks and needs no hook-suppressing configuration.
///
/// TODO: Replace this shell-out with `gix`'s native worktree checkout
/// (the `worktree-mutation` feature, backed by `gix-worktree-state` and
/// `gix-index`) once we've implemented the "remove files absent from the
/// target tree" half of a hard reset ourselves; `gix_worktree_state::checkout`
/// only writes/overwrites entries present in the target tree, it does not
/// delete worktree files that a hard reset would remove.
fn reset_worktree(repo: &gix::Repository) -> Result<(), Error> {
    let Some(workdir) = repo.workdir() else {
        return Ok(());
    };
    let head = repo.head_id().map_err(Error::git)?.detach().to_string();
    let status = Command::new("git")
        .current_dir(workdir)
        .env("GIT_DIR", repo.git_dir())
        .args(["read-tree", "--reset", "-u", &head])
        .status()
        .map_err(Error::git)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::message(format!(
            "git read-tree --reset -u {head} failed with {status}"
        )))
    }
}

/// The tree the worktree is reset to when `state` is applied, or `None` when
/// no reset can be predicted.
///
/// A symbolic HEAD is reset to the snapshot's captured tip of the current
/// branch, and a detached HEAD to the commit it already names. A branch the
/// snapshot does not capture leaves the reset to fail later, and a symbolic
/// captured ref is too unusual to predict.
fn snapshot_worktree_tree(
    repo: &gix::Repository,
    state: &RepositoryState,
) -> Result<Option<ObjectId>, Error> {
    if let Some(branch) = repo.head_name().map_err(Error::git)? {
        let refs = repo.find_tree(state.r#refs.oid()).map_err(Error::git)?;
        let mut updates = Vec::new();
        collect_ref_updates(repo, &refs, "refs", &mut updates)?;
        let Some((_, contents)) = updates
            .iter()
            .find(|(name, _)| name.as_bytes() == branch.as_bstr().as_bytes())
        else {
            return Ok(None);
        };
        return match parse_ref_contents(contents)? {
            Target::Object(oid) => {
                let commit = repo.find_commit(oid).map_err(Error::git)?;
                Ok(Some(commit.tree_id().map_err(Error::git)?.detach()))
            }
            Target::Symbolic(_) => Ok(None),
        };
    }
    match repo.head_id() {
        Ok(head) => {
            let commit = repo.find_commit(head.detach()).map_err(Error::git)?;
            Ok(Some(commit.tree_id().map_err(Error::git)?.detach()))
        }
        Err(_) => Ok(None),
    }
}

/// Refuse to apply `state` when the reset would overwrite untracked files or
/// discard uncommitted changes, matching `git checkout`'s protection.
///
/// `git read-tree --reset -u` clobbers any worktree path the target tree
/// touches, including untracked files and worktree modifications to tracked
/// files whose index entry already matches the target, so the protection has
/// to be our own. Only genuinely affected paths block the restore: unrelated
/// untracked files and clean paths proceed.
fn ensure_no_local_collisions(
    repo: &gix::Repository,
    state: &RepositoryState,
) -> Result<(), Error> {
    let Some(workdir) = repo.workdir() else {
        return Ok(());
    };
    let Some(target) = snapshot_worktree_tree(repo, state)? else {
        return Ok(());
    };
    let target_paths = tree_paths(workdir, target)?;
    let status = status_paths(workdir)?;
    let differing = index_differences(workdir, target)?;

    let mut collisions: std::collections::BTreeMap<String, bool> =
        std::collections::BTreeMap::new();
    for path in &status.untracked {
        if target_paths.contains(path) {
            collisions.insert(path.clone(), true);
        } else if let Some(ancestor) = ancestors(path).find(|name| target_paths.contains(*name)) {
            // The target holds a file where the untracked directory sits.
            collisions.insert(ancestor.to_owned(), true);
        }
    }
    for path in &target_paths {
        if let Some(obstruction) = ancestors(path).find(|name| status.untracked.contains(*name)) {
            // An untracked file blocks a directory the target needs.
            collisions.insert(obstruction.to_owned(), true);
        }
    }
    for path in &status.staged {
        if differing.contains(path) {
            collisions.insert(path.clone(), false);
        }
    }
    for path in &status.worktree_modified {
        collisions.insert(path.clone(), false);
    }
    if collisions.is_empty() {
        return Ok(());
    }
    Err(Error::OverwrittenPaths {
        collisions: collisions
            .into_iter()
            .map(|(path, untracked)| PathCollision { path, untracked })
            .collect(),
    })
}

/// The directory prefixes of a path, nearest first: `a/b/c` yields `a/b`, `a`.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let mut current = path;
    std::iter::from_fn(move || {
        let (parent, _) = current.rsplit_once('/')?;
        current = parent;
        Some(current)
    })
}

/// The paths of every file in `tree`, as worktree-relative names.
fn tree_paths(workdir: &Path, tree: ObjectId) -> Result<HashSet<String>, Error> {
    run_git_lines(
        workdir,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            &tree.to_hex().to_string(),
        ],
    )
}

/// The paths whose index entry differs from `tree`.
fn index_differences(workdir: &Path, tree: ObjectId) -> Result<HashSet<String>, Error> {
    run_git_lines(
        workdir,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "-z",
            &tree.to_hex().to_string(),
        ],
    )
}

/// Run Git in `workdir` and split NUL-separated output into a set of paths.
fn run_git_lines(workdir: &Path, args: &[&str]) -> Result<HashSet<String>, Error> {
    let output = Command::new("git")
        .current_dir(workdir)
        .args(args)
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Err(Error::message(format!(
            "git {} failed with {}",
            args.first().copied().unwrap_or("git"),
            output.status
        )));
    }
    Ok(output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// The worktree state classes `git status` reports.
struct WorktreeStatus {
    /// Paths Git considers untracked.
    untracked: HashSet<String>,
    /// Paths whose worktree content differs from the index, excluding files
    /// deleted in the worktree: the reset restores those from the index,
    /// losing nothing, so like `git checkout` they do not block a restore.
    worktree_modified: HashSet<String>,
    /// Paths whose index entry differs from `HEAD`, including rename sources.
    staged: HashSet<String>,
}

/// Classify `git status --porcelain` output. Rename records contribute both
/// endpoints.
fn status_paths(workdir: &Path) -> Result<WorktreeStatus, Error> {
    let output = Command::new("git")
        .current_dir(workdir)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .map_err(Error::git)?;
    if !output.status.success() {
        return Err(Error::message(format!(
            "git status failed with {}",
            output.status
        )));
    }
    let mut status = WorktreeStatus {
        untracked: HashSet::new(),
        worktree_modified: HashSet::new(),
        staged: HashSet::new(),
    };
    let mut records = output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let Some((&index_status, record)) = record.split_first() else {
            return Err(Error::message("git status returned an empty record"));
        };
        let Some((&worktree_status, path)) = record.split_first() else {
            return Err(Error::message("git status returned a truncated record"));
        };
        let Some(path) = path.strip_prefix(b" ") else {
            return Err(Error::message("git status returned a malformed record"));
        };
        let path = String::from_utf8_lossy(path).into_owned();
        if (index_status, worktree_status) == (b'?', b'?') {
            status.untracked.insert(path);
            continue;
        }
        if index_status != b' ' {
            status.staged.insert(path.clone());
        }
        if worktree_status != b' ' && worktree_status != b'D' {
            status.worktree_modified.insert(path.clone());
        }
        if [index_status, worktree_status].contains(&b'R')
            || [index_status, worktree_status].contains(&b'C')
        {
            // Rename records carry the original path as a second field.
            if let Some(original) = records.next() {
                status
                    .staged
                    .insert(String::from_utf8_lossy(original).into_owned());
            }
        }
    }
    Ok(status)
}

/// Flatten a nested refs tree into Git ref-file contents.
fn collect_ref_updates(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    updates: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), Error> {
    for entry in tree.decode().map_err(Error::git)?.entries {
        let name = entry.filename.to_str().map_err(|_error| {
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
        let name = reference.name().as_bstr().to_str().map_err(|_error| {
            Error::InvalidSnapshot("repository contains a non-UTF-8 ref name".to_owned())
        })?;
        if is_captured_ref(name.as_bytes()) && !captured.contains(name) {
            let name =
                FullName::try_from(name).map_err(|_error| Error::InvalidRef(name.to_owned()))?;
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
            FullName::try_from(name.as_str()).map_err(|_error| Error::InvalidRef(name.clone()))?;
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
            .map_err(|_error| Error::InvalidSnapshot("symbolic ref is not UTF-8".to_owned()))?;
        return Ok(Target::Symbolic(FullName::try_from(target).map_err(
            |_error| Error::InvalidSnapshot("invalid symbolic ref target".to_owned()),
        )?));
    }
    ObjectId::from_hex(contents)
        .map(Target::Object)
        .map_err(|_error| Error::InvalidSnapshot("invalid ref object ID".to_owned()))
}

/// Restore or remove one repository metadata file from a snapshot blob.
fn restore_metadata_file(
    repo: &gix::Repository,
    file: MetadataFile,
    blob: Option<ObjectId>,
) -> Result<(), Error> {
    let path = repo.common_dir().join(file.as_str());
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

/// The phase Git reports when invoking a `reference-transaction` hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceTransactionPhase {
    /// The transaction is being prepared.
    Preparing,
    /// The transaction is prepared and about to be committed.
    Prepared,
    /// The transaction committed.
    Committed,
    /// The transaction was aborted.
    Aborted,
}

impl ReferenceTransactionPhase {
    /// The keyword Git passes to the hook for this phase.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

impl fmt::Display for ReferenceTransactionPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ReferenceTransactionPhase {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "prepared" => Ok(Self::Prepared),
            "committed" => Ok(Self::Committed),
            "aborted" => Ok(Self::Aborted),
            _ => Err(Error::InvalidPhase(value.to_owned())),
        }
    }
}

/// Process one `reference-transaction` hook invocation.
///
/// Only the committed phase writes a snapshot, and only when the transaction
/// changes a captured ref, config, or description; updating [`OP_REF`] alone
/// never does. A committed transaction on a repository without an operation
/// log records the initial snapshot. Deleting [`OP_REF`] uninstalls the local
/// hook instead and records nothing, so the log stays deleted.
///
/// # Examples
///
/// The preparing phase writes nothing:
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
/// git_op::reference_transaction(
///     &repo,
///     git_op::ReferenceTransactionPhase::Prepared,
///     b"transaction input\\n",
/// )
/// .expect("process prepared transaction");
/// assert!(repo
///     .try_find_reference(git_op::OP_REF)
///     .expect("look up operation ref")
///     .is_none());
/// std::fs::remove_dir_all(root).expect("remove temporary repository");
/// ```
pub fn reference_transaction(
    repo: &gix::Repository,
    phase: ReferenceTransactionPhase,
    input: &[u8],
) -> Result<(), Error> {
    match phase {
        ReferenceTransactionPhase::Preparing
        | ReferenceTransactionPhase::Prepared
        | ReferenceTransactionPhase::Aborted => Ok(()),
        ReferenceTransactionPhase::Committed => {
            if transaction_deletes_operation_ref(input)? {
                // Deleting the operation log uninstalls git-op from this
                // repository: the committed deletion removes the local hook
                // so nothing records snapshots again, and no snapshot is
                // captured either way, so the deletion is not resurrected by
                // a capture of the surviving state.
                return uninstall_local(repo);
            }
            if transaction_changes_captured_refs(input)?
                || repo
                    .try_find_reference(OP_REF)
                    .map_err(Error::git)?
                    .is_none()
            {
                // A detached HEAD records nothing; deletion handling above
                // still applies to detached repositories.
                snap(repo)?;
            }
            Ok(())
        }
    }
}

/// One ref update from reference-transaction hook input.
struct TransactionUpdate<'a> {
    /// The new value: an object ID, or all zeros for a deletion.
    new: &'a [u8],
    /// The full name of the updated ref.
    name: &'a [u8],
}

/// Parse reference-transaction hook input into ref updates.
///
/// Each non-empty line holds `old-value new-value ref-name`; anything else is
/// malformed.
fn transaction_updates(input: &[u8]) -> Result<Vec<TransactionUpdate<'_>>, Error> {
    let mut updates = Vec::new();
    for line in input.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let mut fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let parsed = (fields.next(), fields.next(), fields.next(), fields.next());
        let (Some(_old), Some(new), Some(name), None) = parsed else {
            return Err(Error::InvalidHookInput(
                String::from_utf8_lossy(line).into_owned(),
            ));
        };
        updates.push(TransactionUpdate { new, name });
    }
    Ok(updates)
}

/// Determine whether hook input includes a captured ref update.
fn transaction_changes_captured_refs(input: &[u8]) -> Result<bool, Error> {
    Ok(transaction_updates(input)?
        .iter()
        .any(|update| is_captured_ref(update.name)))
}

/// Determine whether hook input deletes the operation ref.
///
/// Git reports a deletion with an all-zero new value.
fn transaction_deletes_operation_ref(input: &[u8]) -> Result<bool, Error> {
    Ok(transaction_updates(input)?.iter().any(|update| {
        update.name == OP_REF.as_bytes() && update.new.iter().all(|&byte| byte == b'0')
    }))
}

#[cfg(test)]
#[expect(
    clippy::assertions_on_result_states,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::string_slice,
    reason = "tests use panics and direct indexing to express failed expectations"
)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEMP_REPOSITORY: AtomicUsize = AtomicUsize::new(0);

    /// The all-zero object ID Git reports for ref deletions.
    const ZERO_OID: &str = "0000000000000000000000000000000000000000";

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
            drop(fs::remove_dir_all(&self.root));
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
        let invoked_by = InvokedBy("git commit".to_owned());
        let commit = write_commit(
            &temporary.repo,
            CommitTreeRequest {
                tree,
                parent: None,
                message: "op: capture initial repository state",
                invoked_by: Some(&invoked_by),
                author: &signature,
                committer: &signature,
                signing: false,
            },
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

        let result = restore_action(&temporary.repo, initial).expect("restore initial snapshot");
        assert_eq!(
            result.changes,
            vec![Changes::Refs(vec![RefChange {
                name: "refs/heads/main".to_owned(),
                kind: RefChangeKind::Updated {
                    old: Target::Object(second),
                    new: Target::Object(first),
                },
            }])]
        );

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

    /// Verify that restoring a snapshot whose ref target has been pruned
    /// aborts with an error and leaves the repository unmodified.
    #[test]
    fn restore_aborts_when_snapshot_ref_target_is_pruned() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let pruned = temporary
            .repo
            .head_id()
            .expect("read pruned commit")
            .detach();
        let snapshot = append(&temporary.repo, "snapshot of pruned commit")
            .expect("append snapshot of pruned commit");
        git(&temporary, &["commit", "--amend", "-m", "one amended"]);
        git(
            &temporary,
            &[
                "reflog",
                "expire",
                "--expire=now",
                "--expire-unreachable=now",
                "--all",
            ],
        );
        git(&temporary, &["gc", "--prune=now"]);

        let status = Command::new("git")
            .current_dir(&temporary.root)
            .args(["cat-file", "-t", &pruned.to_string()])
            .status()
            .expect("check pruned object");
        assert!(
            !status.success(),
            "object {pruned} survived garbage collection"
        );

        match restore_action(&temporary.repo, snapshot) {
            Err(Error::PrunedObject { ref_name, oid }) => {
                assert_eq!(ref_name, "refs/heads/main");
                assert_eq!(oid, pruned);
            }
            result => panic!("expected PrunedObject, got {result:?}"),
        }
        let amended = temporary
            .repo
            .head_id()
            .expect("read amended HEAD")
            .detach()
            .to_string();
        assert_eq!(
            git_output(&temporary, &["rev-parse", "refs/heads/main"]).trim(),
            amended
        );
        git_output(&temporary, &["status"]);
        git_output(&temporary, &["log", "--oneline", "-1"]);
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

    /// Verify that restoring and then undoing returns refs and checkout state.
    #[test]
    fn restore_then_undo_restores_original_state() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"original\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let target = append(&temporary.repo, "target snapshot").expect("append target snapshot");
        std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
        git(&temporary, &["commit", "-am", "two"]);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");

        let original_head = temporary
            .repo
            .head_id()
            .expect("read original HEAD")
            .detach();
        let hooks = temporary.root.join("hooks");
        fs::create_dir(&hooks).expect("create hooks directory");
        let hook = hooks.join("reference-transaction");
        fs::write(&hook, b"#!/bin/sh\nexit 1\n").expect("write rejecting hook");
        let status = Command::new("chmod")
            .args(["+x", hook.to_str().expect("hook path is UTF-8")])
            .status()
            .expect("make hook executable");
        assert!(status.success(), "chmod failed with {status}");
        git(
            &temporary,
            &[
                "config",
                "core.hooksPath",
                hooks.to_str().expect("hooks path is UTF-8"),
            ],
        );

        let restored = restore(&temporary.repo, target).expect("restore target snapshot");
        undo(&temporary.repo).expect("undo restore");
        assert_ne!(restored, target);

        assert_eq!(
            temporary.repo.head_id().expect("read restored HEAD"),
            original_head
        );
        assert_eq!(
            fs::read_to_string(temporary.root.join("tracked")).expect("read restored file"),
            "changed\n"
        );
        assert_eq!(
            git_output(&temporary, &["status", "--porcelain"]),
            "?? hooks/\n"
        );
    }

    #[test]
    fn undo_and_redo_follow_the_logical_transition_table() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["commit", "--allow-empty", "-m", "base"]);
        let a = append(&temporary.repo, "A").expect("append A");
        let initial_description = fs::read(temporary.repo.common_dir().join("description"))
            .expect("read initial description");
        fs::write(temporary.repo.common_dir().join("description"), b"B\n").expect("write B");
        let b = append(&temporary.repo, "B").expect("append B");
        fs::write(temporary.repo.common_dir().join("description"), b"C\n").expect("write C");
        let c = append(&temporary.repo, "C").expect("append C");

        let undo_c = undo_action(&temporary.repo).expect("undo C");
        assert_eq!(undo_c.target, c);
        assert_eq!(
            fs::read(temporary.repo.common_dir().join("description")).expect("read B"),
            b"B\n"
        );
        let undo_b = undo_action(&temporary.repo).expect("undo B");
        assert_eq!(undo_b.target, b);
        assert_eq!(
            fs::read(temporary.repo.common_dir().join("description")).expect("read A"),
            initial_description
        );

        let redo_b = redo_action(&temporary.repo).expect("redo B");
        assert_eq!(redo_b.target, b);
        let redo_c = redo_action(&temporary.repo).expect("redo C");
        assert_eq!(redo_c.target, c);
        assert_eq!(
            fs::read(temporary.repo.common_dir().join("description")).expect("read C"),
            b"C\n"
        );
        assert!(matches!(
            redo_action(&temporary.repo),
            Err(Error::NothingToRedo)
        ));

        let operation = temporary
            .repo
            .find_commit(undo_c.operation)
            .expect("read undo operation");
        let message = operation.message_raw_sloppy();
        assert!(
            message
                .windows(b"Git-op: undo:".len())
                .any(|window| window == b"Git-op: undo:")
        );
        assert!(
            message
                .windows(b"Git-op: undo:".len())
                .any(|window| window == b"Git-op: undo:")
        );
        assert!(
            message
                .windows(b"Git-op: restore:".len())
                .any(|window| window == b"Git-op: restore:")
        );
        assert_ne!(a, b);
    }

    #[test]
    fn new_work_after_undo_ends_the_redo_chain() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["commit", "--allow-empty", "-m", "base"]);
        append(&temporary.repo, "A").expect("append A");
        fs::write(temporary.repo.common_dir().join("description"), b"B\n").expect("write B");
        append(&temporary.repo, "B").expect("append B");
        fs::write(temporary.repo.common_dir().join("description"), b"C\n").expect("write C");
        let c = append(&temporary.repo, "C").expect("append C");
        undo_action(&temporary.repo).expect("undo C");
        fs::write(temporary.repo.common_dir().join("description"), b"D\n").expect("write D");
        append(&temporary.repo, "D").expect("append D");

        assert!(matches!(
            redo_action(&temporary.repo),
            Err(Error::NothingToRedo)
        ));
        undo_action(&temporary.repo).expect("undo D");
        assert_eq!(
            fs::read(temporary.repo.common_dir().join("description")).expect("read B"),
            b"B\n"
        );
        restore(&temporary.repo, c).expect("restore C");
        assert_eq!(
            fs::read(temporary.repo.common_dir().join("description")).expect("read C"),
            b"C\n"
        );
    }

    #[test]
    fn restore_of_current_state_does_not_append() {
        let temporary = TemporaryRepository::new();
        let current = append(&temporary.repo, "current").expect("append current");
        let result = restore_action(&temporary.repo, current).expect("restore current");
        assert!(result.changes.is_empty());
        assert_eq!(result.operation, current);
    }

    /// Verify that a ref-level no-op restore leaves uncommitted index and
    /// worktree changes untouched.
    #[test]
    fn restore_noop_preserves_uncommitted_changes() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let head = temporary.repo.head_id().expect("read HEAD").detach();
        let snapshot = append(&temporary.repo, "snapshot").expect("append snapshot");

        std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
        std::fs::write(temporary.root.join("staged"), b"staged\n").expect("write staged file");
        git(&temporary, &["add", "staged"]);
        let dirty = git_output(&temporary, &["status", "--porcelain"]);
        assert!(
            !dirty.is_empty(),
            "index and worktree should start desynced"
        );

        let result = restore_action(&temporary.repo, snapshot).expect("restore snapshot");
        assert!(result.changes.is_empty());
        assert_eq!(result.operation, snapshot);
        assert_eq!(temporary.repo.head_id().expect("read HEAD"), head);
        assert_eq!(
            git_output(&temporary, &["status", "--porcelain"]),
            dirty,
            "uncommitted index and worktree changes must survive a no-op restore"
        );
    }

    /// Verify that unrelated untracked files do not block a restore and
    /// survive it.
    #[test]
    fn restore_proceeds_with_unrelated_untracked_files() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");

        std::fs::write(temporary.root.join("unrelated"), b"untracked\n")
            .expect("write unrelated untracked file");

        restore(&temporary.repo, initial).expect("restore with unrelated untracked file");

        assert_eq!(
            fs::read_to_string(temporary.root.join("unrelated")).expect("read untracked file"),
            "untracked\n",
            "unrelated untracked files must survive a restore"
        );
    }

    /// Verify that a restore refuses to overwrite a colliding untracked file
    /// and leaves the repository untouched.
    #[test]
    fn restore_refuses_to_overwrite_untracked_files() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        fs::remove_file(temporary.root.join("tracked")).expect("remove tracked file");
        git(&temporary, &["commit", "-am", "two"]);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");

        std::fs::write(temporary.root.join("tracked"), b"untracked\n")
            .expect("write colliding untracked file");

        let error =
            restore_action(&temporary.repo, initial).expect_err("untracked collision must refuse");
        assert!(
            matches!(error, Error::OverwrittenPaths { ref collisions }
                if collisions.len() == 1
                    && collisions[0]
                        == PathCollision { path: "tracked".to_owned(), untracked: true }),
            "expected the untracked collision, got {error:?}"
        );
        assert_eq!(
            fs::read_to_string(temporary.root.join("tracked")).expect("read untracked file"),
            "untracked\n",
            "the colliding file must be untouched"
        );
        assert_eq!(
            git_output(&temporary, &["status", "--porcelain"]),
            "?? tracked\n",
            "refs must not move when the restore is refused"
        );
    }

    /// Verify that a restore refuses to discard unstaged worktree
    /// modifications on a path the target tree touches.
    #[test]
    fn restore_refuses_unstaged_modifications_on_affected_paths() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
        git(&temporary, &["commit", "-am", "two"]);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");

        // Even an index entry matching the target loses the worktree change:
        // read-tree --reset -u rewrites the file, so the path must block.
        git(&temporary, &["checkout", "--", "tracked"]);
        std::fs::write(temporary.root.join("tracked"), b"local edit\n")
            .expect("modify tracked file");

        let error =
            restore_action(&temporary.repo, initial).expect_err("unstaged change must refuse");
        assert!(
            matches!(error, Error::OverwrittenPaths { ref collisions }
                if collisions.len() == 1
                    && collisions[0]
                        == PathCollision { path: "tracked".to_owned(), untracked: false }),
            "expected the uncommitted change, got {error:?}"
        );
        assert_eq!(
            fs::read_to_string(temporary.root.join("tracked")).expect("read tracked file"),
            "local edit\n"
        );
    }

    /// Verify that a restore refuses to discard staged changes on a path the
    /// target tree would change.
    #[test]
    fn restore_refuses_staged_changes_on_affected_paths() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        std::fs::write(temporary.root.join("tracked"), b"changed\n").expect("change tracked file");
        git(&temporary, &["commit", "-am", "two"]);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");

        std::fs::write(temporary.root.join("tracked"), b"staged\n").expect("stage change");
        git(&temporary, &["add", "tracked"]);

        let error =
            restore_action(&temporary.repo, initial).expect_err("staged change must refuse");
        assert!(matches!(error, Error::OverwrittenPaths { .. }));
        assert_eq!(
            git_output(&temporary, &["diff", "--cached", "--name-only"]),
            "tracked\n",
            "the staged change must survive the refused restore"
        );
    }

    /// Verify that a restore refuses when the target deletes a file carrying
    /// uncommitted changes.
    #[test]
    fn restore_refuses_when_target_deletes_a_dirty_file() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let initial = append(&temporary.repo, "initial snapshot").expect("append initial snapshot");
        std::fs::write(temporary.root.join("victim"), b"victim\n").expect("write victim file");
        git(&temporary, &["add", "victim"]);
        git(&temporary, &["commit", "-m", "add victim"]);
        append(&temporary.repo, "current snapshot").expect("append current snapshot");

        std::fs::write(temporary.root.join("victim"), b"dirty\n").expect("modify victim file");

        let error =
            restore_action(&temporary.repo, initial).expect_err("dirty deletion must refuse");
        assert!(
            matches!(error, Error::OverwrittenPaths { ref collisions }
                if collisions.len() == 1
                    && collisions[0]
                        == PathCollision { path: "victim".to_owned(), untracked: false }),
            "expected the victim path, got {error:?}"
        );
        assert_eq!(
            fs::read_to_string(temporary.root.join("victim")).expect("read victim file"),
            "dirty\n"
        );
    }

    /// Verify that a no-op restore still reports snapshot refs that point at
    /// missing objects instead of silently reporting success.
    #[test]
    fn restore_noop_reports_missing_snapshot_referents() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("tracked"), b"initial\n").expect("write tracked file");
        git(&temporary, &["add", "tracked"]);
        git(&temporary, &["commit", "-m", "one"]);
        let missing = temporary.repo.head_id().expect("read commit").detach();
        let snapshot = append(&temporary.repo, "snapshot").expect("append snapshot");
        let hex = missing.to_string();
        let loose = temporary
            .repo
            .git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        std::fs::remove_file(&loose).expect("remove loose commit object");

        match restore_action(&temporary.repo, snapshot) {
            Err(Error::PrunedObject { ref_name, oid }) => {
                assert_eq!(ref_name, "refs/heads/main");
                assert_eq!(oid, missing);
            }
            result => panic!("expected PrunedReferent, got {result:?}"),
        }
    }

    /// Verify that restoring a snapshot whose branch targets an existing
    /// non-commit object is rejected before refs move.
    #[test]
    fn restore_aborts_when_snapshot_branch_targets_non_commit() {
        let temporary = TemporaryRepository::new();
        std::fs::write(temporary.root.join("blob"), b"blob\n").expect("write blob file");
        let blob = git_output(&temporary, &["hash-object", "-w", "blob"])
            .trim()
            .to_owned();
        // Git refuses to point a branch at a blob, so write the loose ref
        // directly; a snapshot records ref targets as plain bytes either way.
        std::fs::create_dir_all(temporary.repo.git_dir().join("refs/heads"))
            .expect("create refs/heads");
        std::fs::write(
            temporary.repo.git_dir().join("refs/heads/broken"),
            format!("{blob}\n"),
        )
        .expect("write loose ref");
        let snapshot = append(&temporary.repo, "snapshot of blob branch")
            .expect("append snapshot of blob branch");
        git(&temporary, &["commit", "--allow-empty", "-m", "two"]);
        let moved = temporary.repo.head_id().expect("read moved HEAD").detach();

        match restore_action(&temporary.repo, snapshot) {
            Err(Error::UnusableObject { ref_name, oid }) => {
                assert_eq!(ref_name, "refs/heads/broken");
                assert_eq!(oid.to_string(), blob);
            }
            result => panic!("expected UnusableReferent, got {result:?}"),
        }
        assert_eq!(
            temporary.repo.head_id().expect("read HEAD"),
            moved,
            "refs must not move when a branch target is unusable"
        );
    }

    /// Verify that a no-op snap does not resolve write-only commit
    /// prerequisites such as signing configuration or commit identity.
    #[test]
    fn snap_noop_does_not_resolve_commit_prerequisites() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["config", "--unset", "user.name"]);
        git(&temporary, &["config", "--unset", "user.email"]);
        // An invalid boolean makes commit_signing_enabled fail regardless of
        // any globally configured identity.
        git(&temporary, &["config", "commit.gpgSign", "maybe"]);

        // Publish the parent snapshot directly with explicit signatures so
        // its creation does not depend on the broken prerequisites that the
        // captured state itself contains.
        let state = capture(&temporary.repo).expect("capture state");
        let tree = serialize(&temporary.repo, &state).expect("serialize state");
        let signature = gix::actor::Signature {
            name: "git-op test".into(),
            email: "git-op@test".into(),
            time: gix::date::Time {
                seconds: 1_700_000_000,
                offset: 0,
            },
        };
        let parent = write_commit(
            &temporary.repo,
            CommitTreeRequest {
                tree,
                parent: None,
                message: "op: capture initial repository state",
                invoked_by: None,
                author: &signature,
                committer: &signature,
                signing: false,
            },
        )
        .expect("write parent snapshot");
        GixRefStore::new(&temporary.repo)
            .apply(RefEdit::Create {
                name: RefName::new(OP_REF).expect("valid OP_REF"),
                new: parent,
            })
            .expect("publish parent snapshot");

        let snapped = snap(&temporary.repo).expect("snap on a current repository");
        let SnapOutcome::Recorded(snapped) = snapped else {
            panic!("snap on a branch records or finds the current state");
        };
        assert_eq!(snapped, SnapResult::Current(parent));
    }

    /// Verify that generated messages identify changed snapshot components.
    #[test]
    fn generated_snapshot_message_identifies_changes() {
        let temporary = TemporaryRepository::new();
        let (initial, _) = append_internal(
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
        let (update, _) = append_internal(
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
        let (initial, _) = append_internal(
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
        let (update, _) = append_internal(
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
        let (initial, _) = append_internal(
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
        let (update, _) = append_internal(
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
        let (initial, _) = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append initial snapshot");
        let (repeat, _) = append_internal(
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

    /// Verify that `snap` on an up-to-date repository returns the current tip
    /// and leaves the operation ref untouched.
    #[test]
    fn snap_is_noop_when_log_is_current() {
        let temporary = TemporaryRepository::new();
        let SnapOutcome::Recorded(initial) =
            snap(&temporary.repo).expect("append initial snapshot")
        else {
            panic!("first snap on a branch records the initial snapshot");
        };
        let SnapResult::Appended(initial) = initial else {
            panic!("first snap should append");
        };
        let SnapOutcome::Recorded(snapped) =
            snap(&temporary.repo).expect("snap up-to-date repository")
        else {
            panic!("snap on a branch records or finds the current state");
        };
        assert_eq!(snapped, SnapResult::Current(initial));
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

    /// Verify that `snap` records drift, such as a direct edit to the
    /// repository description that no reference transaction observed.
    #[test]
    fn snap_records_metadata_drift() {
        let temporary = TemporaryRepository::new();
        let SnapOutcome::Recorded(initial) =
            snap(&temporary.repo).expect("append initial snapshot")
        else {
            panic!("snap on a branch records the initial snapshot");
        };
        let initial = initial.operation();
        fs::write(
            temporary.repo.common_dir().join("description"),
            b"drifted description\n",
        )
        .expect("write description");
        let snapped = match snap(&temporary.repo).expect("snap drifted repository") {
            SnapOutcome::Recorded(snapped) => snapped.operation(),
            SnapOutcome::Detached => panic!("snap on a branch records the drifted state"),
        };
        assert_ne!(snapped, initial);
        let changed = changes(&temporary.repo, snapped)
            .expect("compute changes")
            .expect("snapped snapshot has a parent");
        assert!(matches!(changed.as_slice(), [Changes::Description]));
        let state = read(&temporary.repo, snapped).expect("read snapped state");
        let description = temporary
            .repo
            .find_blob(state.description.expect("description was captured").oid())
            .expect("read description blob");
        assert_eq!(description.data, b"drifted description\n");
    }

    /// Verify that `changes` reports `None` for the initial snapshot and the
    /// correct parts changed for a later one.
    #[test]
    fn changes_reports_parts_changed_since_parent() {
        let temporary = TemporaryRepository::new();
        let (initial, _) = append_internal(
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
        let (update, _) = append_internal(
            &temporary.repo,
            CommitMessage::Generated,
            AppendOptions::default(),
        )
        .expect("append changed snapshot");
        let changed = changes(&temporary.repo, update)
            .expect("compute changes")
            .expect("updated snapshot has a parent");
        assert!(
            matches!(changed.as_slice(), [Changes::Refs(refs), Changes::Description] if refs.len() == 1)
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
        for phase in [
            ReferenceTransactionPhase::Preparing,
            ReferenceTransactionPhase::Prepared,
            ReferenceTransactionPhase::Aborted,
        ] {
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

    /// Verify that a `HEAD`-only transaction captures the initial state.
    #[test]
    fn hook_captures_initial_state_on_head_only_transaction() {
        let temporary = TemporaryRepository::new();
        let input = b"0000000000000000000000000000000000000000 ref:refs/heads/main HEAD\n";
        reference_transaction(&temporary.repo, ReferenceTransactionPhase::Committed, input)
            .expect("process committed HEAD transaction");
        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_some()
        );
    }

    /// Verify that a detached HEAD records nothing: normal ref writes carry no
    /// branch context, so neither the hook nor a manual snap captures one.
    #[test]
    fn snap_skips_detached_head() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
        git(&temporary, &["checkout", "--detach"]);

        let snapped = snap(&temporary.repo).expect("snap detached repository");
        assert_eq!(snapped, SnapOutcome::Detached);
        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none(),
            "a detached HEAD must not start an operation log"
        );
    }

    /// Verify that a committed transaction on a detached HEAD records nothing.
    #[test]
    fn hook_skips_detached_head() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
        git(&temporary, &["checkout", "--detach"]);

        reference_transaction(
            &temporary.repo,
            ReferenceTransactionPhase::Committed,
            b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/main\n",
        )
        .expect("process committed transaction on a detached head");

        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none(),
            "a detached HEAD must not start an operation log"
        );
    }

    /// Verify that deleting the operation log still uninstalls on a detached
    /// HEAD, and that no snapshot is captured either way.
    #[test]
    fn op_ref_deletion_uninstalls_on_detached_head() {
        let temporary = TemporaryRepository::new();
        git(&temporary, &["commit", "--allow-empty", "-m", "one"]);
        git(&temporary, &["checkout", "--detach"]);
        install_local(&temporary.repo).expect("install hook");

        reference_transaction(
            &temporary.repo,
            ReferenceTransactionPhase::Committed,
            format!("{ZERO_OID} {ZERO_OID} {OP_REF}\n").as_bytes(),
        )
        .expect("process deletion transaction on a detached head");

        assert!(
            !temporary
                .repo
                .git_dir()
                .join("hooks/reference-transaction")
                .exists(),
            "deletion handling takes precedence over the detached no-op"
        );
        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none()
        );
    }

    /// Verify that trailer values flatten newlines.
    #[test]
    fn trailer_text_flattens_newlines() {
        assert_eq!(trailer_text("git commit"), "git commit");
        assert_eq!(trailer_text("git\nEvil: forged"), "git Evil: forged");
    }

    /// Verify that an empty committed transaction leaves an existing operation log unchanged.
    #[test]
    fn hook_accepts_empty_committed_transaction() {
        let temporary = TemporaryRepository::new();
        reference_transaction(
            &temporary.repo,
            ReferenceTransactionPhase::Committed,
            b"0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 refs/heads/main\n",
        )
        .expect("record initial snapshot");
        let before = temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .expect("operation ref exists")
            .id();
        reference_transaction(&temporary.repo, ReferenceTransactionPhase::Committed, b"")
            .expect("process empty transaction");
        assert_eq!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .expect("operation ref exists")
                .id(),
            before
        );
    }

    /// Verify that empty hook input is treated as a no-op.
    #[test]
    fn hook_parser_accepts_empty_transactions() {
        assert!(!transaction_changes_captured_refs(b"\n").expect("empty transaction"));
        assert!(!transaction_changes_captured_refs(b"\r\n").expect("empty transaction"));
    }

    /// Verify that a committed deletion of the operation ref uninstalls the
    /// local hook and is not resurrected by a capture.
    #[test]
    fn op_ref_deletion_uninstalls_the_local_hook() {
        let temporary = TemporaryRepository::new();
        install_local(&temporary.repo).expect("install hook");
        append(&temporary.repo, "snapshot").expect("append snapshot");
        let tip = temporary
            .repo
            .try_find_reference(OP_REF)
            .expect("look up operation ref")
            .expect("operation ref exists")
            .target()
            .try_id()
            .expect("direct operation ref")
            .to_owned();
        // Git deletes the ref before invoking the committed-phase hook, and
        // gix applies the deletion without firing the installed hook, keeping
        // the simulation hermetic.
        temporary
            .repo
            .edit_reference(GixRefEdit {
                change: Change::Delete {
                    expected: PreviousValue::MustExistAndMatch(Target::Object(tip)),
                    log: RefLog::AndReference,
                },
                name: FullName::try_from(OP_REF).expect("valid operation ref name"),
                deref: false,
            })
            .expect("delete operation ref");

        reference_transaction(
            &temporary.repo,
            ReferenceTransactionPhase::Committed,
            format!("{tip} {ZERO_OID} {OP_REF}\n").as_bytes(),
        )
        .expect("process deletion transaction");

        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none(),
            "a deleted operation log must not be resurrected"
        );
        assert!(
            !temporary
                .repo
                .git_dir()
                .join("hooks/reference-transaction")
                .exists(),
            "the committed deletion must remove the local hook"
        );
    }

    /// Verify that a deletion seen before the committed phase changes nothing.
    #[test]
    fn op_ref_deletion_on_prepared_phase_leaves_the_hook() {
        let temporary = TemporaryRepository::new();
        install_local(&temporary.repo).expect("install hook");
        let tip = "1111111111111111111111111111111111111111";

        reference_transaction(
            &temporary.repo,
            ReferenceTransactionPhase::Prepared,
            format!("{tip} {ZERO_OID} {OP_REF}\n").as_bytes(),
        )
        .expect("process prepared deletion transaction");

        assert!(
            temporary
                .repo
                .git_dir()
                .join("hooks/reference-transaction")
                .exists(),
            "only the committed phase uninstalls"
        );
        assert!(
            temporary
                .repo
                .try_find_reference(OP_REF)
                .expect("look up operation ref")
                .is_none()
        );
    }

    /// Verify that the hook parser recognizes only operation-ref deletions.
    #[test]
    fn hook_parser_detects_operation_ref_deletions() {
        let oid = "1111111111111111111111111111111111111111";
        assert!(
            transaction_deletes_operation_ref(format!("{oid} {ZERO_OID} {OP_REF}\n").as_bytes())
                .expect("parse deletion")
        );
        assert!(
            !transaction_deletes_operation_ref(format!("{ZERO_OID} {oid} {OP_REF}\n").as_bytes())
                .expect("parse creation"),
            "creating the operation ref is not a deletion"
        );
        assert!(
            !transaction_deletes_operation_ref(
                format!("{oid} {ZERO_OID} refs/heads/main\n").as_bytes()
            )
            .expect("parse branch deletion")
        );
        assert!(
            !transaction_deletes_operation_ref(b"").expect("parse empty input"),
            "empty input deletes nothing"
        );
    }
}
