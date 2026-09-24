//! The amber backend on its own: every jj object round-trips, and the stored objects are the amber
//! objects core would build (file keys match `ingest`, commits decode with core's strict decoder).

use std::sync::Arc;

use ajj::backend::{AmberBackend, CHANGE_ID_LENGTH, empty_dir_key};
use amber_store_core::commit::{Commit as AmberCommit, Identity};
use amber_store_core::fstree;
use amber_store_core::ingest;
use amber_store_core::key::{Key, Type};
use jj_lib::backend::{
    Backend, ChangeId, Commit, CommitId, CopyId, MillisSinceEpoch, Signature, Timestamp, Tree, TreeId,
    TreeValue, make_root_commit,
};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::merge::Merge;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::{RepoPath, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use pollster::FutureExt as _;

fn settings() -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    config.add_layer(
        ConfigLayer::parse(ConfigSource::User, "user.name = \"Ann\"\nuser.email = \"ann@example.com\"\n")
            .unwrap(),
    );
    UserSettings::from_config(config).unwrap()
}

fn backend(dir: &tempfile::TempDir) -> AmberBackend {
    AmberBackend::init(&settings(), dir.path()).unwrap()
}

/// Deterministic incompressible-ish bytes.
fn data(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn write_file(b: &AmberBackend, bytes: &[u8]) -> jj_lib::backend::FileId {
    let mut r = futures::io::Cursor::new(bytes.to_vec());
    b.write_file(RepoPath::root(), &mut r).block_on().unwrap()
}

fn read_file(b: &AmberBackend, id: &jj_lib::backend::FileId) -> Vec<u8> {
    use futures::AsyncReadExt as _;
    let mut r = b.read_file(RepoPath::root(), id).block_on().unwrap();
    let mut out = Vec::new();
    r.read_to_end(&mut out).block_on().unwrap();
    out
}

fn sig(name: &str, ms: i64, tz: i32) -> Signature {
    Signature {
        name: name.into(),
        email: format!("{}@example.com", name.to_lowercase()),
        timestamp: Timestamp { timestamp: MillisSinceEpoch(ms), tz_offset: tz },
    }
}

fn commit(_b: &AmberBackend, parents: Vec<CommitId>, tree: TreeId, msg: &str) -> Commit {
    Commit {
        parents,
        predecessors: vec![],
        root_tree: Merge::resolved(tree),
        conflict_labels: Merge::resolved(String::new()),
        change_id: ChangeId::new((0..CHANGE_ID_LENGTH as u8).collect()),
        description: msg.into(),
        author: sig("Ann", 1_700_000_000_123, 120),
        committer: sig("Bob", 1_700_000_000_456, -300),
        secure_sig: None,
    }
}

fn name(s: &str) -> RepoPathComponentBuf {
    RepoPathComponentBuf::new(s).unwrap()
}

#[test]
fn root_commit_and_empty_tree() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    assert_eq!(b.empty_tree_id().as_bytes(), empty_dir_key().as_bytes());
    assert!(b.read_tree(RepoPath::root(), b.empty_tree_id()).block_on().unwrap().is_empty());
    assert_eq!(b.write_tree(RepoPath::root(), &Tree::default()).block_on().unwrap(), *b.empty_tree_id());
    let root = b.read_commit(b.root_commit_id()).block_on().unwrap();
    assert_eq!(root, make_root_commit(b.root_change_id().clone(), b.empty_tree_id().clone()));
}

#[test]
fn files_round_trip_with_ingest_keys() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    let src = tempfile::tempdir().unwrap();
    // Empty, one chunk, and several chunks under a FileNode (chunks are at most 1 MiB).
    let cases = [(0usize, "empty"), (5, "small"), (3 << 20, "big")];
    for (n, file) in cases {
        let bytes = data(n, n as u64 + 1);
        std::fs::write(src.path().join(file), &bytes).unwrap();
        let id = write_file(&b, &bytes);
        assert_eq!(read_file(&b, &id), bytes, "{file}");
    }
    // The same files ingested by core get the same keys.
    let (_, root) = ingest::dir(b.store(), src.path(), ingest::Opts::default());
    let entries = fstree::collect_entries(root.unwrap(), |k| b.store().get(k)).unwrap();
    for e in entries {
        let bytes = std::fs::read(src.path().join(std::str::from_utf8(&e.name).unwrap())).unwrap();
        assert_eq!(write_file(&b, &bytes).as_bytes(), &e.content_key[..]);
    }
    let big = Key::parse(write_file(&b, &data(3 << 20, (3 << 20) + 1)).as_bytes()).unwrap();
    assert_eq!(big.type_(), Type::FileNode);
    assert_eq!(big.length(), 3 << 20);
}

#[test]
fn trees_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    let file = write_file(&b, b"hello\n");
    let link = b.write_symlink(RepoPath::root(), "../target").block_on().unwrap();
    assert_eq!(b.read_symlink(RepoPath::root(), &link).block_on().unwrap(), "../target");
    let sub = Tree::from_sorted_entries(vec![(
        name("x.sh"),
        TreeValue::File { id: file.clone(), executable: true, copy_id: CopyId::placeholder() },
    )]);
    let sub_id = b.write_tree(RepoPath::root(), &sub).block_on().unwrap();
    let c = commit(&b, vec![b.root_commit_id().clone()], sub_id.clone(), "vendored");
    let (commit_id, _) = b.write_commit(c, None).block_on().unwrap();

    let tree = Tree::from_sorted_entries(vec![
        (name("a.txt"), TreeValue::File { id: file, executable: false, copy_id: CopyId::placeholder() }),
        (name("link"), TreeValue::Symlink(link)),
        (name("sub"), TreeValue::Tree(sub_id)),
        (name("vendor"), TreeValue::GitSubmodule(commit_id)),
    ]);
    let id = b.write_tree(RepoPath::root(), &tree).block_on().unwrap();
    assert_eq!(b.read_tree(RepoPath::root(), &id).block_on().unwrap(), tree);

    // What core sees: plain POSIX entries, the link target inline, and a directory entry holding the
    // commit, which reads as that commit's tree.
    let key = Key::parse(id.as_bytes()).unwrap();
    let entries = fstree::collect_entries(key, |k| b.store().get(k)).unwrap();
    let modes: Vec<(String, u64)> =
        entries.iter().map(|e| (String::from_utf8(e.name.clone()).unwrap(), e.mode)).collect();
    assert_eq!(
        modes,
        [("a.txt", 0o100644), ("link", 0o120777), ("sub", 0o040755), ("vendor", 0o040755)]
            .map(|(n, m)| (n.to_owned(), m))
    );
    assert_eq!(entries[1].link_target, b"../target");
    let through = fstree::resolve_entry(key, "vendor/x.sh", |k| b.store().get(k)).unwrap().unwrap();
    assert_eq!(through.mode, 0o100755);
}

#[test]
fn large_directories_become_dir_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    let file = write_file(&b, b"x");
    let entries: Vec<_> = (0..5000)
        .map(|i| {
            (
                name(&format!("f{i:05}")),
                TreeValue::File { id: file.clone(), executable: false, copy_id: CopyId::placeholder() },
            )
        })
        .collect();
    let tree = Tree::from_sorted_entries(entries);
    let id = b.write_tree(RepoPath::root(), &tree).block_on().unwrap();
    assert_eq!(Key::parse(id.as_bytes()).unwrap().type_(), Type::DirNode);
    assert_eq!(b.read_tree(RepoPath::root(), &id).block_on().unwrap(), tree);
}

#[test]
fn commits_round_trip_as_amber_commits() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    let empty = b.empty_tree_id().clone();
    let file = write_file(&b, b"1");
    let t1 = b
        .write_tree(
            RepoPath::root(),
            &Tree::from_sorted_entries(vec![(
                name("f"),
                TreeValue::File { id: file, executable: false, copy_id: CopyId::placeholder() },
            )]),
        )
        .block_on()
        .unwrap();

    let a = commit(&b, vec![b.root_commit_id().clone()], empty.clone(), "a");
    let (a_id, a_stored) = b.write_commit(a.clone(), None).block_on().unwrap();
    assert_eq!(a_stored, a);
    assert_eq!(b.read_commit(&a_id).block_on().unwrap(), a);

    // Only the root as parent: stored as a root commit, which core decodes.
    let a_key = Key::parse(a_id.as_bytes()).unwrap();
    assert_eq!(a_key.type_(), Type::Commit);
    let raw = AmberCommit::decode(&b.store().get(a_key).unwrap()).unwrap();
    assert!(raw.parents.is_empty());
    assert_eq!(raw.change_id, a.change_id.to_bytes());
    assert_eq!(raw.author.when, 1_700_000_000_123_000_000);
    assert_eq!(raw.committer.tz_offset, -300);

    // A conflicted merge with labels.
    let mut m = commit(&b, vec![a_id.clone(), a_id.clone()], empty.clone(), "merge\n");
    m.parents[1] = b
        .write_commit(commit(&b, vec![b.root_commit_id().clone()], t1.clone(), "b"), None)
        .block_on()
        .unwrap()
        .0;
    m.root_tree = Merge::from_vec(vec![t1.clone(), empty.clone(), t1.clone()]);
    m.conflict_labels = Merge::from_vec(vec!["ours".into(), String::new(), "theirs".into()]);
    let (m_id, m_stored) = b.write_commit(m.clone(), None).block_on().unwrap();
    assert_eq!(m_stored, m);
    assert_eq!(b.read_commit(&m_id).block_on().unwrap(), m);
    let raw = AmberCommit::decode(&b.store().get(Key::parse(m_id.as_bytes()).unwrap()).unwrap()).unwrap();
    assert_eq!(raw.conflict_terms.len(), 2);
    assert_eq!(raw.conflict_labels, ["ours", "", "theirs"]);
    // The key's length is the footprint: own bytes plus every tree's length.
    let own = raw.encode().unwrap().len() as u64;
    let trees: u64 = raw.trees().iter().map(|k| k.length()).sum();
    assert_eq!(Key::parse(m_id.as_bytes()).unwrap().length(), own + trees);

    // Unlabelled conflicts store no labels and read back unlabelled.
    let mut u = m.clone();
    u.conflict_labels = Merge::resolved(String::new());
    let (u_id, _) = b.write_commit(u.clone(), None).block_on().unwrap();
    assert_eq!(b.read_commit(&u_id).block_on().unwrap(), u);

    // Predecessors are not stored, and the returned commit says so.
    let mut p = commit(&b, vec![a_id.clone()], empty, "p");
    p.predecessors = vec![a_id.clone()];
    let (p_id, p_stored) = b.write_commit(p, None).block_on().unwrap();
    assert!(p_stored.predecessors.is_empty());
    assert_eq!(b.read_commit(&p_id).block_on().unwrap(), p_stored);
}

#[test]
fn signatures_cover_the_amber_payload() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    let c = commit(&b, vec![b.root_commit_id().clone()], b.empty_tree_id().clone(), "signed");
    let mut seen = Vec::new();
    let mut sign = |data: &[u8]| {
        seen = data.to_vec();
        Ok(blake3::hash(data).as_bytes().to_vec())
    };
    let (id, stored) = b.write_commit(c, Some(&mut sign)).block_on().unwrap();
    let sig = stored.secure_sig.clone().expect("signed");
    assert_eq!(sig.data, seen);
    assert_eq!(sig.sig, blake3::hash(&seen).as_bytes().to_vec());
    assert_eq!(b.read_commit(&id).block_on().unwrap(), stored);
    let raw = AmberCommit::decode(&b.store().get(Key::parse(id.as_bytes()).unwrap()).unwrap()).unwrap();
    assert_eq!(raw.signature_payload().unwrap(), seen);
    assert_eq!(raw.signature, sig.sig);
}

#[test]
fn foreign_commits_get_stable_change_ids() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    let id = Identity {
        name: "dstore".into(),
        email: String::new(),
        when: 1_700_000_000_000_000_001,
        tz_offset: 0,
    };
    let mut c = AmberCommit {
        tree: empty_dir_key(),
        parents: vec![],
        author: id.clone(),
        committer: id,
        message: "pushed by dstore".into(),
        signature: vec![],
        public_key: vec![],
        change_id: vec![],
        conflict_terms: vec![],
        conflict_labels: vec![],
    };
    let (k1, raw1) = c.object().unwrap();
    b.store().put(k1, &raw1).unwrap();
    c.change_id = vec![7; 4];
    let (k2, raw2) = c.object().unwrap();
    b.store().put(k2, &raw2).unwrap();

    let read = |k: Key| b.read_commit(&CommitId::new(k.as_bytes().to_vec())).block_on().unwrap();
    let (j1, j2) = (read(k1), read(k2));
    assert_eq!(j1.change_id.as_bytes().len(), CHANGE_ID_LENGTH);
    assert_eq!(j2.change_id.as_bytes().len(), CHANGE_ID_LENGTH);
    assert_ne!(j1.change_id, j2.change_id);
    assert_eq!(read(k1).change_id, j1.change_id);
    assert_eq!(j1.parents, vec![b.root_commit_id().clone()]);
    // Nanoseconds that are not whole milliseconds round down.
    assert_eq!(j1.author.timestamp.timestamp, MillisSinceEpoch(1_700_000_000_000));
}

#[test]
fn unsupported_shapes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(&dir);
    assert!(b.write_symlink(RepoPath::root(), "").block_on().is_err());
    let (a_id, _) = b
        .write_commit(commit(&b, vec![b.root_commit_id().clone()], b.empty_tree_id().clone(), "a"), None)
        .block_on()
        .unwrap();
    let m = commit(&b, vec![a_id, b.root_commit_id().clone()], b.empty_tree_id().clone(), "m");
    assert!(b.write_commit(m, None).block_on().is_err());
    let bad = CommitId::new(write_file(&b, b"not a commit").to_bytes());
    assert!(b.read_commit(&bad).block_on().is_err());
}

#[test]
fn a_second_process_sees_the_objects() {
    let dir = tempfile::tempdir().unwrap();
    let b = Arc::new(backend(&dir));
    let (id, c) = b
        .write_commit(commit(&b, vec![b.root_commit_id().clone()], b.empty_tree_id().clone(), "a"), None)
        .block_on()
        .unwrap();
    // Another handle on the same directory, as another jj command would open it.
    let other = AmberBackend::load(&settings(), dir.path()).unwrap();
    assert_eq!(other.read_commit(&id).block_on().unwrap(), c);
}
