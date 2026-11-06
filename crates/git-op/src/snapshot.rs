//! Git-shaped serialization of repository state.
//!
//! A snapshot tree mirrors the repository's common directory: captured refs
//! form a nested `refs/` tree, and each captured metadata file is a blob at
//! its own name. Absent metadata files are omitted rather than wrapped in a
//! presence marker, so the tree is exactly what Git would store for those
//! files.

use gix::bstr::ByteSlice;
use gix::objs::Write;
use gix::objs::tree::{Entry, EntryKind, EntryMode, EntryRef};
use gix_refstore::ObjectId;

use crate::{Error, RawBlob, RawTree, RepositoryState};

/// The top-level tree entry holding the captured refs.
const REFS_ENTRY: &str = "refs";

/// Serialize a repository state into a Git tree.
pub(crate) fn serialize(
    repo: &gix::Repository,
    state: &RepositoryState,
) -> Result<ObjectId, Error> {
    let mut entries = vec![tree_entry(REFS_ENTRY, state.r#refs.oid())];
    if let Some(config) = state.config {
        entries.push(blob_entry("config", config.oid()));
    }
    if let Some(description) = state.description {
        entries.push(blob_entry("description", description.oid()));
    }
    entries.sort();
    repo.write(&gix::objs::Tree { entries }).map_err(Error::git)
}

/// Read a repository state from a snapshot commit's tree.
pub(crate) fn read(repo: &gix::Repository, commit: ObjectId) -> Result<RepositoryState, Error> {
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
            Some(REFS_ENTRY) => {
                if !entry.mode.is_tree() {
                    return Err(Error::InvalidSnapshot(format!(
                        "{commit} has a non-tree {REFS_ENTRY} entry"
                    )));
                }
                refs = Some(entry.oid.to_owned());
            }
            Some("config") => config = optional_blob(repo, &entry)?,
            Some("description") => description = optional_blob(repo, &entry)?,
            Some(name) => {
                return Err(Error::InvalidSnapshot(format!(
                    "{commit} has an unexpected {name} entry"
                )));
            }
            None => {
                return Err(Error::InvalidSnapshot(format!(
                    "{commit} has a non-UTF-8 tree entry"
                )));
            }
        }
    }
    let refs = refs.ok_or_else(|| {
        Error::InvalidSnapshot(format!("{commit} is missing its {REFS_ENTRY} tree entry"))
    })?;
    Ok(RepositoryState {
        r#refs: RawTree::new(refs),
        config: config.map(RawBlob::new),
        description: description.map(RawBlob::new),
    })
}

/// Read a metadata entry, accepting both the current blob layout and the
/// legacy `facet_git_tree` `Option` tree.
pub(crate) fn optional_blob(
    repo: &gix::Repository,
    entry: &EntryRef<'_>,
) -> Result<Option<ObjectId>, Error> {
    match entry.mode.kind() {
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(Some(entry.oid.to_owned())),
        EntryKind::Tree => {
            let tree = repo.find_tree(entry.oid.to_owned()).map_err(Error::git)?;
            for child in tree.decode().map_err(Error::git)?.entries {
                if child.filename.to_str().ok() == Some("some") {
                    return blob_oid(&child).map(Some);
                }
            }
            Ok(None)
        }
        _ => Err(Error::InvalidSnapshot(format!(
            "metadata entry {:?} is not a blob",
            entry.filename
        ))),
    }
}

/// Return a blob entry's object ID, rejecting non-blob modes.
fn blob_oid(entry: &EntryRef<'_>) -> Result<ObjectId, Error> {
    match entry.mode.kind() {
        EntryKind::Blob | EntryKind::BlobExecutable => Ok(entry.oid.to_owned()),
        _ => Err(Error::InvalidSnapshot(format!(
            "metadata entry {:?} is not a blob",
            entry.filename
        ))),
    }
}

fn tree_entry(name: &str, oid: ObjectId) -> Entry {
    Entry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: name.into(),
        oid,
    }
}

fn blob_entry(name: &str, oid: ObjectId) -> Entry {
    Entry {
        mode: EntryMode::from(EntryKind::Blob),
        filename: name.into(),
        oid,
    }
}
