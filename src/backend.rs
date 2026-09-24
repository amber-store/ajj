//! A jj commit backend whose objects are amber objects.
//!
//! Files, trees and commits are stored in a local core-rs packstore under the repo's store directory
//! (`.jj/repo/store/amber/packstore`), and every jj id is the 32-byte amber key of its object:
//!
//! | jj | amber |
//! | --- | --- |
//! | `FileId` | the file's root key (`Blob` or `FileNode`), chunked as `ingest` chunks |
//! | `SymlinkId` | the key of a `Blob` holding the target; trees keep the target inline |
//! | `TreeId` | a `DirLeaf` or `DirNode` key |
//! | `CommitId` | a `Commit` key |
//!
//! Because the ids are amber keys, a jj commit written here is the same object a dstore reference can
//! name: pushing a bookmark is uploading the commit's closure and putting a reference on its key
//! ([`crate::dstore`]). The mapping of the commit record is in [`crate::convert`].

use std::convert::Infallible;
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use amber_store_core::chunkers::{ItemChunker, split_bytes};
use amber_store_core::commit::Commit as AmberCommit;
use amber_store_core::fstree::{self, DirBuilder, Entry, IndexBuilder, Object};
use amber_store_core::key::{Key, Type};
use amber_store_core::packstore;
use async_trait::async_trait;
use futures::io::Cursor;
use futures::stream::BoxStream;
use futures::{AsyncRead, AsyncReadExt as _, StreamExt as _};
use jj_lib::backend::{
    Backend, BackendError, BackendInitError, BackendLoadError, BackendResult, ChangeId, Commit, CommitId,
    CopyHistory, CopyId, CopyRecord, FileId, RelatedCopy, SigningFn, SymlinkId, Tree, TreeId, TreeValue,
    make_root_commit,
};
use jj_lib::index::Index;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;

use crate::convert;

/// The name recorded in `.jj/repo/store/type`.
pub const BACKEND_NAME: &str = "amber";

/// Every id is an amber key.
pub const COMMIT_ID_LENGTH: usize = amber_store_core::key::SIZE;
/// jj's customary change-id length; amber stores it opaquely (key 7).
pub const CHANGE_ID_LENGTH: usize = 16;

/// Item chunker bits for file and directory indexes: `ingest`'s default, so a file stored here has
/// the key `amber-store ingest` gives the same bytes.
const ITEM_BITS: u32 = 7;

/// Small segments: every jj command is a process that opens the store, and opening costs about
/// 0.1 us per record in the active segments (core `CLAUDE.md`, "opening a store").
const SEGMENT_SIZE: u64 = 64 << 20;

const MODE_TYPE: u64 = 0o170000;
const S_IFREG: u64 = 0o100000;
const S_IFDIR: u64 = 0o040000;
const S_IFLNK: u64 = 0o120000;
const MODE_FILE: u64 = 0o100644;
const MODE_EXEC: u64 = 0o100755;
const MODE_DIR: u64 = 0o040755;
const MODE_LINK: u64 = 0o120777;

/// The key of the empty directory: a `DirLeaf` whose body is the empty CBOR array.
pub fn empty_dir_key() -> Key {
    Key::new(Type::DirLeaf, 1, &[0x80])
}

pub struct AmberBackend {
    path: PathBuf,
    store: Arc<packstore::Store>,
    root_commit_id: CommitId,
    root_change_id: ChangeId,
    empty_tree_id: TreeId,
}

impl fmt::Debug for AmberBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AmberBackend").field("path", &self.path).finish_non_exhaustive()
    }
}

impl AmberBackend {
    /// The directory under jj's store directory that holds the packstore and the remote list.
    pub fn amber_dir(store_path: &Path) -> PathBuf {
        store_path.join("amber")
    }

    pub fn init(_settings: &UserSettings, store_path: &Path) -> Result<Self, BackendInitError> {
        let backend = Self::open(store_path).map_err(|e| BackendInitError(e.into()))?;
        let empty = fstree::encode_dir_leaf(&[]).map_err(|e| BackendInitError(e.into()))?;
        debug_assert_eq!(empty.key, empty_dir_key());
        backend.store.put(empty.key, &empty.bytes).map_err(|e| BackendInitError(e.into()))?;
        Ok(backend)
    }

    pub fn load(_settings: &UserSettings, store_path: &Path) -> Result<Self, BackendLoadError> {
        Self::open(store_path).map_err(|e| BackendLoadError(e.into()))
    }

    fn open(store_path: &Path) -> Result<Self, packstore::Error> {
        let path = Self::amber_dir(store_path);
        let store = packstore::Store::open_with(
            path.join("packstore"),
            packstore::Options::new().sync(true).segment_size(SEGMENT_SIZE),
        )?;
        Ok(Self {
            path,
            store: Arc::new(store),
            root_commit_id: CommitId::new(vec![0; COMMIT_ID_LENGTH]),
            root_change_id: ChangeId::new(vec![0; CHANGE_ID_LENGTH]),
            empty_tree_id: TreeId::new(empty_dir_key().as_bytes().to_vec()),
        })
    }

    /// `.jj/repo/store/amber`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The local packstore, shared with the dstore transfers.
    pub fn store(&self) -> &Arc<packstore::Store> {
        &self.store
    }

    /// The amber key a commit id stands for; the root commit has none.
    pub fn commit_key(&self, id: &CommitId) -> BackendResult<Option<Key>> {
        if *id == self.root_commit_id {
            return Ok(None);
        }
        typed_key(id.as_bytes(), &[Type::Commit], "commit").map(Some)
    }

    fn get(&self, k: Key, object_type: &str) -> BackendResult<Vec<u8>> {
        self.store.get(k).map_err(|e| {
            if e.is_not_found() {
                BackendError::ObjectNotFound {
                    object_type: object_type.to_owned(),
                    hash: k.to_string(),
                    source: Box::new(e),
                }
            } else {
                BackendError::ReadObject {
                    object_type: object_type.to_owned(),
                    hash: k.to_string(),
                    source: Box::new(e),
                }
            }
        })
    }

    fn put_all(&self, objects: Vec<Object>, object_type: &'static str) -> BackendResult<()> {
        self.store
            .write_batch(objects.into_iter().map(|o| Ok::<_, Infallible>(packstore::Object::from(o))))
            .map_err(|e| BackendError::WriteObject { object_type, source: Box::new(e) })
    }

    /// Stores `data` as a file and returns its root key: one `Blob` per chunk under a `FileNode`
    /// index, or the single `Blob` itself, as `ingest` builds files. The empty file is the empty Blob.
    pub fn write_file_bytes(&self, data: &[u8]) -> BackendResult<Key> {
        let (root, objects) = build_file(data)?;
        self.put_all(objects, "file")?;
        Ok(root)
    }

    pub fn read_file_bytes(&self, k: Key) -> BackendResult<Vec<u8>> {
        let mut out = Vec::new();
        fstree::write_content(&mut out, k, |kk| self.store.get(kk)).map_err(|e| {
            BackendError::ReadObject { object_type: "file".into(), hash: k.to_string(), source: Box::new(e) }
        })?;
        Ok(out)
    }

    fn tree_value_to_entry(&self, name: &str, value: &TreeValue) -> BackendResult<Entry> {
        let mut e = Entry { name: name.as_bytes().to_vec(), ..Default::default() };
        match value {
            TreeValue::File { id, executable, copy_id: _ } => {
                typed_key(id.as_bytes(), &[Type::Blob, Type::FileNode], "file")?;
                e.mode = if *executable { MODE_EXEC } else { MODE_FILE };
                e.content_key = id.to_bytes();
            }
            TreeValue::Symlink(id) => {
                let k = typed_key(id.as_bytes(), &[Type::Blob], "symlink")?;
                e.mode = MODE_LINK;
                e.link_target = self.get(k, "symlink")?;
            }
            TreeValue::Tree(id) => {
                typed_key(id.as_bytes(), &[Type::DirLeaf, Type::DirNode], "tree")?;
                e.mode = MODE_DIR;
                e.content_key = id.to_bytes();
            }
            TreeValue::GitSubmodule(id) => {
                // A directory entry may hold a commit and reads as its tree (core
                // architecture/commits.md, "Commits inside directories").
                let Some(k) = self.commit_key(id)? else {
                    return Err(BackendError::Unsupported(format!(
                        "{name}: a submodule cannot point at the root commit"
                    )));
                };
                e.mode = MODE_DIR;
                e.content_key = k.as_bytes().to_vec();
            }
        }
        Ok(e)
    }

    /// Maps one directory entry to a jj tree value. Entries jj cannot represent (devices, fifos,
    /// sockets, which a tree ingested from a filesystem may hold) map to `None` and are left out.
    fn entry_to_tree_value(&self, e: &Entry, extra: &mut Vec<Object>) -> BackendResult<Option<TreeValue>> {
        let value = match e.mode & MODE_TYPE {
            S_IFREG => {
                let id = if e.content_key.is_empty() {
                    // Not written by core's ingest, which always stores the empty Blob; accept it.
                    let empty = fstree::encode_blob(&[]);
                    let k = empty.key;
                    extra.push(empty);
                    k.as_bytes().to_vec()
                } else {
                    e.content_key.clone()
                };
                TreeValue::File {
                    id: FileId::new(id),
                    executable: e.mode & 0o100 != 0,
                    copy_id: CopyId::placeholder(),
                }
            }
            S_IFDIR => {
                let k = Key::parse(&e.content_key).map_err(|err| BackendError::ReadObject {
                    object_type: "tree".into(),
                    hash: hex::encode(&e.content_key),
                    source: Box::new(err),
                })?;
                if k.type_() == Type::Commit {
                    TreeValue::GitSubmodule(CommitId::new(e.content_key.clone()))
                } else {
                    TreeValue::Tree(TreeId::new(e.content_key.clone()))
                }
            }
            S_IFLNK => {
                // The target lives in the entry; its SymlinkId is the key of a Blob of the target,
                // which is stored so that read_symlink finds it.
                let blob = fstree::encode_blob(&e.link_target);
                let id = SymlinkId::new(blob.key.as_bytes().to_vec());
                extra.push(blob);
                TreeValue::Symlink(id)
            }
            _ => return Ok(None),
        };
        Ok(Some(value))
    }
}

/// Parses a 32-byte id as a canonical amber key of one of the given types.
fn typed_key(bytes: &[u8], types: &[Type], object_type: &str) -> BackendResult<Key> {
    if bytes.len() != COMMIT_ID_LENGTH {
        return Err(BackendError::InvalidHashLength {
            expected: COMMIT_ID_LENGTH,
            actual: bytes.len(),
            object_type: object_type.to_owned(),
            hash: hex::encode(bytes),
        });
    }
    let k = Key::parse(bytes).map_err(|e| BackendError::ReadObject {
        object_type: object_type.to_owned(),
        hash: hex::encode(bytes),
        source: Box::new(e),
    })?;
    if !types.contains(&k.type_()) {
        return Err(BackendError::ReadObject {
            object_type: object_type.to_owned(),
            hash: k.to_string(),
            source: format!("key of type {} where {object_type} was expected", k.type_()).into(),
        });
    }
    Ok(k)
}

fn build_file(data: &[u8]) -> BackendResult<(Key, Vec<Object>)> {
    let to_err = |e: String| BackendError::WriteObject { object_type: "file", source: e.into() };
    let mut objects = Vec::new();
    let mut ib = IndexBuilder::new_file(ItemChunker::new(ITEM_BITS));
    let mut chunks = Vec::new();
    split_bytes(data, None, |chunk: Vec<u8>| -> Result<(), Infallible> {
        chunks.push(chunk);
        Ok(())
    })
    .map_err(|e| to_err(format!("{e:?}")))?;
    if chunks.is_empty() {
        chunks.push(Vec::new());
    }
    let mut emit = |o: Object| -> Result<(), Infallible> {
        objects.push(o);
        Ok(())
    };
    for chunk in chunks {
        let blob = fstree::encode_blob(&chunk);
        let k = blob.key;
        emit(blob).unwrap();
        ib.add_child(&mut emit, k, &[]).map_err(|e| to_err(format!("{e:?}")))?;
    }
    let root = ib.finish(&mut emit).map_err(|e| to_err(format!("{e:?}")))?;
    Ok((root, objects))
}

fn to_other(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> BackendError {
    BackendError::Other(e.into())
}

#[async_trait]
impl Backend for AmberBackend {
    fn name(&self) -> &str {
        BACKEND_NAME
    }

    fn commit_id_length(&self) -> usize {
        COMMIT_ID_LENGTH
    }

    fn change_id_length(&self) -> usize {
        CHANGE_ID_LENGTH
    }

    fn root_commit_id(&self) -> &CommitId {
        &self.root_commit_id
    }

    fn root_change_id(&self) -> &ChangeId {
        &self.root_change_id
    }

    fn empty_tree_id(&self) -> &TreeId {
        &self.empty_tree_id
    }

    fn concurrency(&self) -> usize {
        1
    }

    async fn read_file(
        &self,
        _path: &RepoPath,
        id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>> {
        let k = typed_key(id.as_bytes(), &[Type::Blob, Type::FileNode], "file")?;
        let data = self.read_file_bytes(k)?;
        Ok(Box::pin(Cursor::new(data)))
    }

    async fn write_file(
        &self,
        _path: &RepoPath,
        contents: &mut (dyn AsyncRead + Send + Unpin),
    ) -> BackendResult<FileId> {
        let mut data = Vec::new();
        contents.read_to_end(&mut data).await.map_err(to_other)?;
        let k = self.write_file_bytes(&data)?;
        Ok(FileId::new(k.as_bytes().to_vec()))
    }

    async fn read_symlink(&self, _path: &RepoPath, id: &SymlinkId) -> BackendResult<String> {
        let k = typed_key(id.as_bytes(), &[Type::Blob], "symlink")?;
        let target = self.get(k, "symlink")?;
        String::from_utf8(target).map_err(|e| BackendError::InvalidUtf8 {
            object_type: "symlink".into(),
            hash: k.to_string(),
            source: e.utf8_error(),
        })
    }

    async fn write_symlink(&self, _path: &RepoPath, target: &str) -> BackendResult<SymlinkId> {
        if target.is_empty() {
            // A directory entry's empty link target is the absent one.
            return Err(BackendError::Unsupported("an empty symlink target cannot be stored".into()));
        }
        let blob = fstree::encode_blob(target.as_bytes());
        let id = SymlinkId::new(blob.key.as_bytes().to_vec());
        self.put_all(vec![blob], "symlink")?;
        Ok(id)
    }

    async fn read_copy(&self, _id: &CopyId) -> BackendResult<CopyHistory> {
        Err(BackendError::Unsupported("the amber backend does not track copies".into()))
    }

    async fn write_copy(&self, _copy: &CopyHistory) -> BackendResult<CopyId> {
        Err(BackendError::Unsupported("the amber backend does not track copies".into()))
    }

    async fn get_related_copies(&self, _copy_id: &CopyId) -> BackendResult<Vec<RelatedCopy>> {
        Err(BackendError::Unsupported("the amber backend does not track copies".into()))
    }

    async fn read_tree(&self, _path: &RepoPath, id: &TreeId) -> BackendResult<Tree> {
        let k = typed_key(id.as_bytes(), &[Type::DirLeaf, Type::DirNode], "tree")?;
        let entries = fstree::collect_entries(k, |kk| self.store.get(kk)).map_err(|e| {
            if e.missing_object().is_some() || e.is_not_found() {
                BackendError::ObjectNotFound {
                    object_type: "tree".into(),
                    hash: k.to_string(),
                    source: Box::new(e),
                }
            } else {
                BackendError::ReadObject {
                    object_type: "tree".into(),
                    hash: k.to_string(),
                    source: Box::new(e),
                }
            }
        })?;
        let mut extra = Vec::new();
        let mut out = Vec::with_capacity(entries.len());
        for e in &entries {
            let name = std::str::from_utf8(&e.name).map_err(|err| BackendError::InvalidUtf8 {
                object_type: "tree".into(),
                hash: k.to_string(),
                source: err,
            })?;
            let Some(value) = self.entry_to_tree_value(e, &mut extra)? else { continue };
            let name = RepoPathComponentBuf::new(name).map_err(to_other)?;
            out.push((name, value));
        }
        if !extra.is_empty() {
            self.put_all(extra, "symlink")?;
        }
        Ok(Tree::from_sorted_entries(out))
    }

    async fn write_tree(&self, _path: &RepoPath, contents: &Tree) -> BackendResult<TreeId> {
        let mut builder = DirBuilder::new(ItemChunker::new(ITEM_BITS));
        let mut objects = Vec::new();
        let mut emit = |o: Object| -> Result<(), Infallible> {
            objects.push(o);
            Ok(())
        };
        // jj's order (bytewise on the UTF-8 name) is amber's order (bytewise on the name).
        for entry in contents.entries() {
            let e = self.tree_value_to_entry(entry.name().as_internal_str(), entry.value())?;
            builder.add_entry(&mut emit, e).map_err(|e| BackendError::WriteObject {
                object_type: "tree",
                source: format!("{e:?}").into(),
            })?;
        }
        let root = builder.finish(&mut emit).map_err(|e| BackendError::WriteObject {
            object_type: "tree",
            source: format!("{e:?}").into(),
        })?;
        self.put_all(objects, "tree")?;
        Ok(TreeId::new(root.as_bytes().to_vec()))
    }

    async fn read_commit(&self, id: &CommitId) -> BackendResult<Commit> {
        let Some(k) = self.commit_key(id)? else {
            return Ok(make_root_commit(self.root_change_id.clone(), self.empty_tree_id.clone()));
        };
        let bytes = self.get(k, "commit")?;
        let c = AmberCommit::decode(&bytes).map_err(|e| BackendError::ReadObject {
            object_type: "commit".into(),
            hash: k.to_string(),
            source: Box::new(e),
        })?;
        Ok(convert::commit_from_amber(k, &c, &self.root_commit_id))
    }

    async fn write_commit(
        &self,
        contents: Commit,
        sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)> {
        assert!(contents.secure_sig.is_none(), "commit.secure_sig was set");
        let mut c = convert::commit_to_amber(&contents, &self.root_commit_id)
            .map_err(|e| BackendError::WriteObject { object_type: "commit", source: e.into() })?;
        if let Some(sign) = sign_with {
            let payload = c
                .signature_payload()
                .map_err(|e| BackendError::WriteObject { object_type: "commit", source: Box::new(e) })?;
            c.signature = sign(&payload).map_err(to_other)?;
        }
        let (k, bytes) = c
            .object()
            .map_err(|e| BackendError::WriteObject { object_type: "commit", source: Box::new(e) })?;
        self.store
            .put(k, &bytes)
            .map_err(|e| BackendError::WriteObject { object_type: "commit", source: Box::new(e) })?;
        // Return what a read gives back: predecessors are not stored, and a signature comes back
        // with its payload.
        let stored = convert::commit_from_amber(k, &c, &self.root_commit_id);
        debug_assert!(c.signature.is_empty() || stored.secure_sig.is_some());
        Ok((CommitId::new(k.as_bytes().to_vec()), stored))
    }

    fn get_copy_records(
        &self,
        _paths: Option<&[RepoPathBuf]>,
        _root: &CommitId,
        _head: &CommitId,
    ) -> BackendResult<BoxStream<'_, BackendResult<CopyRecord>>> {
        Ok(futures::stream::empty().boxed())
    }

    fn gc(&self, _index: &dyn Index, _keep_newer: SystemTime) -> BackendResult<()> {
        // Objects live as long as dstore references reach them; the local packstore is a cache that
        // keeps everything jj ever wrote.
        Ok(())
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AmberBackend>();
};
