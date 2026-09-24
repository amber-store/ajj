//! Moving commits and bookmarks through a dstore cluster (dstore-testkit's in-memory fake nodes):
//! the reference operations with their compare-and-swap, and two jj repositories sharing a branch
//! through fetch and push as the CLI does it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ajj::backend::AmberBackend;
use ajj::bookmarks::{self, PushSelection};
use ajj::dstore::{self, Error};
use amber_store_core::fstree;
use amber_store_core::key::Key;
use amber_store_core::packstore;
use dstore_client::{Cluster, Config as ClientConfig, Ctx};
use dstore_testkit::fake::{FakeCluster, FakeClusterConfig};
use dstore_transport::Endpoint;
use dstore_transport::mem::Network;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::workspace::Workspace;

struct Env {
    fc: Arc<FakeCluster>,
    net: Arc<Network>,
}

impl Env {
    async fn start() -> Env {
        let net = Network::new();
        let fc = FakeCluster::start(&net, FakeClusterConfig::default()).await;
        Env { fc, net }
    }

    async fn dial(&self, id: u8) -> Cluster {
        let endpoint: Arc<dyn Endpoint> = self.net.bind(dstore_client::NodeId([id; 32]), &[]);
        Cluster::dial(
            &Ctx::background(),
            ClientConfig {
                endpoint: Some(endpoint),
                ticket: self.fc.ticket(),
                logger: Some(dstore::quiet_logger()),
                request_timeout: Duration::from_secs(20),
                ..ClientConfig::default()
            },
        )
        .await
        .unwrap()
    }
}

fn settings(user: &str) -> UserSettings {
    let mut config = StackedConfig::with_defaults();
    let text = format!("user.name = \"{user}\"\nuser.email = \"{}@example.com\"\n", user.to_lowercase());
    config.add_layer(ConfigLayer::parse(ConfigSource::User, &text).unwrap());
    UserSettings::from_config(config).unwrap()
}

async fn init_repo(dir: &std::path::Path, user: &str) -> Arc<ReadonlyRepo> {
    let s = settings(user);
    let (_, repo) = Workspace::init_with_backend(
        &s,
        dir,
        &|settings, store_path| Ok(Box::new(AmberBackend::init(settings, store_path)?)),
        Signer::from_settings(&s).unwrap(),
    )
    .await
    .unwrap();
    repo
}

fn amber(repo: &ReadonlyRepo) -> &AmberBackend {
    repo.store().backend_impl::<AmberBackend>().unwrap()
}

fn key(id: &CommitId) -> Key {
    Key::parse(id.as_bytes()).unwrap()
}

/// Commits a child of `parent` (the root when `None`) with a new description; returns the repo.
async fn commit_on(
    repo: Arc<ReadonlyRepo>,
    parent: Option<&CommitId>,
    msg: &str,
) -> (Arc<ReadonlyRepo>, Commit) {
    let parent = parent.cloned().unwrap_or_else(|| repo.store().root_commit_id().clone());
    let mut tx = repo.start_transaction();
    let tree = repo.store().get_commit_async(&parent).await.unwrap().tree();
    let c = tx.repo_mut().new_commit(vec![parent], tree).set_description(msg).write().await.unwrap();
    let repo = tx.commit(format!("commit {msg}")).await.unwrap();
    (repo, c)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn references_move_only_from_the_expected_commit() {
    let env = Env::start().await;
    let cl = env.dial(0xc1).await;
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), "Ann").await;
    let (repo, a) = commit_on(repo, None, "a").await;
    let (repo, b) = commit_on(repo, Some(a.id()), "b").await;
    let store = Arc::clone(amber(&repo).store());

    // Create-only, then a compare-and-swap from the right and from a wrong commit.
    dstore::put_branch(&cl, &store, "r/main", key(a.id()), None, "ann", None).await.unwrap();
    let err = dstore::put_branch(&cl, &store, "r/main", key(b.id()), None, "ann", None).await.unwrap_err();
    assert!(matches!(err, Error::Changed { .. }), "{err}");
    dstore::put_branch(&cl, &store, "r/main", key(b.id()), Some(key(a.id())), "ann", None).await.unwrap();
    // Re-pushing what is already there succeeds (an interrupted push that got that far).
    dstore::put_branch(&cl, &store, "r/main", key(b.id()), Some(key(a.id())), "ann", None).await.unwrap();

    // A tree reference under the prefix is not a branch.
    let (_, tree_root) = {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("f"), b"x").unwrap();
        amber_store_core::ingest::dir(&store, src.path(), amber_store_core::ingest::Opts::default())
    };
    let tree_root = tree_root.unwrap();
    cl.push(&Ctx::background(), Arc::clone(&store), tree_root, "r/tree", "ann", Default::default(), None)
        .await
        .unwrap();
    let (branches, other) = dstore::list_branches(&cl, "r/").await.unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].bookmark, "main");
    assert_eq!(branches[0].key, key(b.id()));
    assert_eq!(other, ["r/tree"]);

    // A fresh store fetches the whole history.
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = Arc::new(packstore::Store::open(fresh_dir.path()).unwrap());
    let fetched = dstore::fetch(&cl, &fresh, key(b.id()), None).await.unwrap();
    assert!(fetched.objects > 0 && fetched.bytes > 0);
    let all = fstree::reachable_keys(key(b.id()), |k| store.get(k)).unwrap();
    assert!(fresh.missing(&all).unwrap().is_empty());
    assert!(all.contains(&key(a.id())));

    // Deletion is conditional too.
    let err = dstore::delete_branch(&cl, "r/main", key(a.id())).await.unwrap_err();
    assert!(matches!(err, Error::Changed { .. }), "{err}");
    dstore::delete_branch(&cl, "r/main", key(b.id())).await.unwrap();
    assert!(dstore::list_branches(&cl, "r/").await.unwrap().0.is_empty());

    cl.close();
    env.fc.close().await;
}

async fn fetch_into(
    repo: Arc<ReadonlyRepo>,
    cl: &Cluster,
    prefix: &str,
) -> (Arc<ReadonlyRepo>, bookmarks::ImportStats) {
    let store = Arc::clone(amber(&repo).store());
    let (branches, _) = dstore::list_branches(cl, prefix).await.unwrap();
    let mut fetched = BTreeMap::new();
    for b in branches {
        dstore::fetch(cl, &store, b.key, None).await.unwrap();
        fetched.insert(RefNameBuf::from(b.bookmark.as_str()), CommitId::new(b.key.as_bytes().to_vec()));
    }
    let mut tx = repo.start_transaction();
    let stats = bookmarks::import_remote_branches(tx.repo_mut(), RemoteName::new("origin"), &fetched, true)
        .await
        .unwrap();
    (tx.commit("fetch").await.unwrap(), stats)
}

async fn push_from(
    repo: Arc<ReadonlyRepo>,
    cl: &Cluster,
    prefix: &str,
    sel: &PushSelection,
) -> (Arc<ReadonlyRepo>, Result<(), Error>) {
    let origin = RemoteName::new("origin");
    let (updates, rejected) = bookmarks::plan_push(repo.view(), origin, sel);
    assert!(rejected.is_empty(), "{rejected:?}");
    let store = Arc::clone(amber(&repo).store());
    let mut tx = repo.start_transaction();
    let mut result = Ok(());
    for u in &updates {
        let name = format!("{prefix}{}", u.name.as_str());
        let before = u.diff.before.as_ref().map(key);
        let r = match &u.diff.after {
            Some(a) => dstore::put_branch(cl, &store, &name, key(a), before, "t", None).await.map(|_| ()),
            None => dstore::delete_branch(cl, &name, before.unwrap()).await,
        };
        match r {
            Ok(()) => bookmarks::record_pushed(tx.repo_mut(), origin, u),
            Err(e) => result = Err(e),
        }
    }
    (tx.commit("push").await.unwrap(), result)
}

fn local(repo: &ReadonlyRepo, name: &str) -> RefTarget {
    repo.view().get_local_bookmark(RefName::new(name)).clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_repos_share_a_branch() {
    let env = Env::start().await;
    let cl = env.dial(0xc1).await;
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ann = init_repo(d1.path(), "Ann").await;
    let bob = init_repo(d2.path(), "Bob").await;
    let named = |n: &str| PushSelection { names: vec![n.into()], ..Default::default() };

    // Ann pushes a new bookmark; it needs naming.
    let (ann, a) = commit_on(ann, None, "a").await;
    let mut tx = ann.start_transaction();
    tx.repo_mut().set_local_bookmark_target(RefName::new("main"), RefTarget::normal(a.id().clone()));
    let ann = tx.commit("bookmark").await.unwrap();
    let (updates, _) = bookmarks::plan_push(ann.view(), RemoteName::new("origin"), &PushSelection::default());
    assert!(updates.is_empty(), "an untracked new bookmark is not pushed by default");
    let (ann, r) = push_from(ann, &cl, "p/", &named("main")).await;
    r.unwrap();
    let origin_main =
        ann.view().get_remote_bookmark(RefName::new("main").to_remote_symbol(RemoteName::new("origin")));
    assert!(origin_main.is_tracked());

    // Bob fetches: main@origin and main, and the commit with its change id.
    let (bob, stats) = fetch_into(bob, &cl, "p/").await;
    assert_eq!(stats.changed, vec![(RefNameBuf::from("main"), Some(a.id().clone()))]);
    assert_eq!(local(&bob, "main"), RefTarget::normal(a.id().clone()));
    let bob_a = bob.store().get_commit_async(a.id()).await.unwrap();
    assert_eq!(bob_a.change_id(), a.change_id());
    assert_eq!(bob_a.description(), "a");

    // Bob advances main and pushes by default (it is tracked now).
    let (bob, b) = commit_on(bob, Some(a.id()), "b").await;
    let mut tx = bob.start_transaction();
    tx.repo_mut().set_local_bookmark_target(RefName::new("main"), RefTarget::normal(b.id().clone()));
    let bob = tx.commit("move").await.unwrap();
    let (_bob, r) = push_from(bob, &cl, "p/", &PushSelection::default()).await;
    r.unwrap();

    // Ann, still at a, moves main elsewhere: her push is refused, her fetch conflicts the bookmark.
    let (ann, a2) = commit_on(ann, Some(a.id()), "a2").await;
    let mut tx = ann.start_transaction();
    tx.repo_mut().set_local_bookmark_target(RefName::new("main"), RefTarget::normal(a2.id().clone()));
    let ann = tx.commit("move").await.unwrap();
    let (ann, r) = push_from(ann, &cl, "p/", &PushSelection::default()).await;
    assert!(matches!(r, Err(Error::Changed { .. })));
    let (ann, _) = fetch_into(ann, &cl, "p/").await;
    assert!(local(&ann, "main").has_conflict());

    // Resolving to Bob's commit makes the bookmark match; nothing left to push.
    let mut tx = ann.start_transaction();
    tx.repo_mut().set_local_bookmark_target(RefName::new("main"), RefTarget::normal(b.id().clone()));
    let ann = tx.commit("resolve").await.unwrap();
    let (updates, _) = bookmarks::plan_push(ann.view(), RemoteName::new("origin"), &PushSelection::default());
    assert!(updates.is_empty());

    // Deleting the bookmark and pushing the deletion removes the reference.
    let mut tx = ann.start_transaction();
    tx.repo_mut().set_local_bookmark_target(RefName::new("main"), RefTarget::absent());
    let ann = tx.commit("delete").await.unwrap();
    let (updates, _) = bookmarks::plan_push(ann.view(), RemoteName::new("origin"), &PushSelection::default());
    assert!(updates.is_empty(), "deletions need --deleted");
    let (_ann, r) = push_from(ann, &cl, "p/", &PushSelection { deleted: true, ..Default::default() }).await;
    r.unwrap();
    assert!(dstore::list_branches(&cl, "p/").await.unwrap().0.is_empty());

    cl.close();
    env.fc.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_abandons_what_the_remote_dropped() {
    let env = Env::start().await;
    let cl = env.dial(0xc1).await;
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ann = init_repo(d1.path(), "Ann").await;
    let bob = init_repo(d2.path(), "Bob").await;
    let store = Arc::clone(amber(&ann).store());

    let (ann, a) = commit_on(ann, None, "a").await;
    let (ann, x) = commit_on(ann, Some(a.id()), "x").await;
    dstore::put_branch(&cl, &store, "q/topic", key(x.id()), None, "ann", None).await.unwrap();
    let (bob, _) = fetch_into(bob, &cl, "q/").await;
    assert!(bob.view().heads().contains(x.id()));

    // The topic is rewritten upstream: x is replaced by a sibling.
    let (_, y) = commit_on(ann, Some(a.id()), "y").await;
    dstore::put_branch(&cl, &store, "q/topic", key(y.id()), Some(key(x.id())), "ann", None).await.unwrap();
    let (bob, stats) = fetch_into(bob, &cl, "q/").await;
    assert_eq!(stats.abandoned, vec![x.id().clone()]);
    assert!(!bob.view().heads().contains(x.id()));
    assert!(bob.view().heads().contains(y.id()));

    cl.close();
    env.fc.close().await;
}
