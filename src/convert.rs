//! jj `backend::Commit` ⇄ amber `commit::Commit` (core `architecture/commits.md`, "Conflicts").
//!
//! | jj | amber |
//! | --- | --- |
//! | `parents` | key 1; a commit whose only parent is the virtual root commit has no parents |
//! | `root_tree: Merge<TreeId>` `[A0, R0, A1, …]` | key 0 is `A0`, key 8 the rest |
//! | `conflict_labels: Merge<String>` | key 9, absent when the tree is resolved or no term is labelled |
//! | `change_id` | key 7 |
//! | `description`, `author`, `committer` | keys 4, 2, 3; milliseconds ⇄ nanoseconds |
//! | `secure_sig` | key 5, over [`AmberCommit::signature_payload`] |
//! | `predecessors` | not stored (deprecated in jj, which keeps them in its operation log) |
//!
//! A commit not written by jj (dstore's working copies write them) may lack a change id or carry one
//! of another length; jj needs [`CHANGE_ID_LENGTH`] bytes, so one is derived ([`change_id_of`]).

use amber_store_core::commit::{Commit as AmberCommit, Identity};
use amber_store_core::key::{Key, Type};
use jj_lib::backend::{
    ChangeId, Commit, CommitId, MillisSinceEpoch, SecureSig, Signature, Timestamp, TreeId,
};
use jj_lib::merge::{Merge, MergeBuilder};
use jj_lib::object_id::ObjectId as _;

use crate::backend::CHANGE_ID_LENGTH;

const NANOS_PER_MILLI: i64 = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("{what} {hex} is not a canonical {expected} key")]
    BadKey { what: &'static str, hex: String, expected: &'static str },
    #[error("the root commit can only be a commit's sole parent")]
    RootInMerge,
    #[error("{0}: time out of range (amber keeps nanoseconds in 64 bits, about 292 years around 1970)")]
    TimeRange(&'static str),
}

pub fn commit_to_amber(c: &Commit, root: &CommitId) -> Result<AmberCommit, ConvertError> {
    let parents = if c.parents.len() == 1 && c.parents[0] == *root {
        Vec::new()
    } else {
        let mut parents = Vec::with_capacity(c.parents.len());
        for p in &c.parents {
            if p == root {
                return Err(ConvertError::RootInMerge);
            }
            parents.push(key_of(p.as_bytes(), "parent", &[Type::Commit], "Commit")?);
        }
        parents
    };
    let mut trees = Vec::with_capacity(c.root_tree.as_slice().len());
    for t in c.root_tree.iter() {
        trees.push(key_of(t.as_bytes(), "tree", &[Type::DirLeaf, Type::DirNode], "DirLeaf or DirNode")?);
    }
    let tree = trees.remove(0);
    let conflict_labels = if c.root_tree.is_resolved()
        || c.conflict_labels.is_resolved()
        || c.conflict_labels.iter().all(String::is_empty)
    {
        Vec::new()
    } else {
        c.conflict_labels.iter().cloned().collect()
    };
    Ok(AmberCommit {
        tree,
        parents,
        author: identity_of(&c.author, "author")?,
        committer: identity_of(&c.committer, "committer")?,
        message: c.description.clone(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id: c.change_id.to_bytes(),
        conflict_terms: trees,
        conflict_labels,
    })
}

pub fn commit_from_amber(k: Key, c: &AmberCommit, root: &CommitId) -> Commit {
    let parents = if c.parents.is_empty() {
        vec![root.clone()]
    } else {
        c.parents.iter().map(|p| CommitId::new(p.as_bytes().to_vec())).collect()
    };
    let root_tree: MergeBuilder<TreeId> =
        c.trees().iter().map(|t| TreeId::new(t.as_bytes().to_vec())).collect();
    let conflict_labels = if c.conflict_labels.is_empty() {
        Merge::resolved(String::new())
    } else {
        Merge::from_vec(c.conflict_labels.clone())
    };
    let secure_sig = if c.signature.is_empty() {
        None
    } else {
        // The payload is the record without key 5; a stored commit re-encodes, so this holds.
        c.signature_payload().ok().map(|data| SecureSig { data, sig: c.signature.clone() })
    };
    Commit {
        parents,
        predecessors: Vec::new(),
        root_tree: root_tree.build(),
        conflict_labels,
        change_id: change_id_of(k, c),
        description: c.message.clone(),
        author: signature_of(&c.author),
        committer: signature_of(&c.committer),
        secure_sig,
    }
}

/// The change id jj sees: the stored one when it has jj's length, otherwise 16 bytes derived from it,
/// or from the commit key when there is none. Derived ids are stable, so every reader agrees.
pub fn change_id_of(k: Key, c: &AmberCommit) -> ChangeId {
    if c.change_id.len() == CHANGE_ID_LENGTH {
        return ChangeId::new(c.change_id.clone());
    }
    let mut h = blake3::Hasher::new();
    h.update(b"ajj change id\0");
    if c.change_id.is_empty() {
        h.update(b"key\0");
        h.update(k.as_bytes());
    } else {
        h.update(b"id\0");
        h.update(&c.change_id);
    }
    ChangeId::new(h.finalize().as_bytes()[..CHANGE_ID_LENGTH].to_vec())
}

fn key_of(
    bytes: &[u8],
    what: &'static str,
    types: &[Type],
    expected: &'static str,
) -> Result<Key, ConvertError> {
    let bad = || ConvertError::BadKey { what, hex: hex::encode(bytes), expected };
    let k = Key::parse(bytes).map_err(|_| bad())?;
    if !types.contains(&k.type_()) {
        return Err(bad());
    }
    Ok(k)
}

fn identity_of(s: &Signature, what: &'static str) -> Result<Identity, ConvertError> {
    let when = s.timestamp.timestamp.0.checked_mul(NANOS_PER_MILLI).ok_or(ConvertError::TimeRange(what))?;
    Ok(Identity { name: s.name.clone(), email: s.email.clone(), when, tz_offset: s.timestamp.tz_offset })
}

fn signature_of(id: &Identity) -> Signature {
    Signature {
        name: id.name.clone(),
        email: id.email.clone(),
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(id.when.div_euclid(NANOS_PER_MILLI)),
            tz_offset: id.tz_offset,
        },
    }
}
