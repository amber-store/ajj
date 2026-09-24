//! Remote-tracking bookmarks for dstore remotes, the way `jj git fetch` and `jj git push` keep them.
//!
//! Fetch records every branch under the remote's prefix as the remote bookmark `name@remote`, merges
//! it into the local bookmark when tracked, and abandons commits that only the old remote position
//! kept visible (as `jj git fetch` does). Push compares each local bookmark with its tracked remote
//! bookmark and yields the reference updates to make.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::TryStreamExt as _;
use jj_lib::backend::{BackendError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::index::IndexError;
use jj_lib::merge::Diff;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RefNameBuf, RemoteName};
use jj_lib::refs::{LocalAndRemoteRef, RefPushAction, classify_ref_push_action};
use jj_lib::repo::{MutableRepo, Repo as _};
use jj_lib::revset::{RevsetEvaluationError, RevsetExpression, RevsetStreamExt as _};
use jj_lib::view::View;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Revset(#[from] RevsetEvaluationError),
}

/// What a fetch changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Remote bookmarks whose target changed, with the new target (`None`: deleted on the remote).
    pub changed: Vec<(RefNameBuf, Option<CommitId>)>,
    /// Commits no longer visible because the remote moved away from them.
    pub abandoned: Vec<CommitId>,
}

/// Makes `name@remote` point where the remote's branches point now: `fetched` is every branch under
/// the remote's prefix. Remote bookmarks missing from `fetched` were deleted on the remote. The
/// commits of `fetched` must be in the store. New remote bookmarks are tracked when `track_new`.
pub async fn import_remote_branches(
    mut_repo: &mut MutableRepo,
    remote: &RemoteName,
    fetched: &BTreeMap<RefNameBuf, CommitId>,
    track_new: bool,
) -> Result<ImportStats, ImportError> {
    mut_repo.ensure_remote(remote);
    let existing: BTreeMap<RefNameBuf, RemoteRef> =
        mut_repo.view().remote_bookmarks(remote).map(|(name, r)| (name.to_owned(), r.clone())).collect();
    let names: BTreeSet<&RefNameBuf> = existing.keys().chain(fetched.keys()).collect();

    let mut updates = Vec::new();
    for name in names {
        let new_target = fetched.get(name).map_or_else(RefTarget::absent, |id| RefTarget::normal(id.clone()));
        let old = existing.get(name).cloned().unwrap_or_else(RemoteRef::absent);
        if old.target != new_target {
            updates.push((name.clone(), old, new_target));
        }
    }

    // Make the new commits and their ancestors visible before bookmarks point at them.
    let store = Arc::clone(mut_repo.store());
    let mut heads: Vec<Commit> = Vec::new();
    for (_, _, target) in &updates {
        for id in target.added_ids() {
            heads.push(store.get_commit_async(id).await?);
        }
    }
    mut_repo.add_heads(&heads).await?;

    let mut stats = ImportStats::default();
    let mut hidable: Vec<CommitId> = Vec::new();
    for (name, old, new_target) in updates {
        let symbol = name.to_remote_symbol(remote);
        hidable.extend(old.target.added_ids().cloned());
        let state = if old.is_present() || old.is_tracked() {
            old.state
        } else if track_new {
            RemoteRefState::Tracked
        } else {
            RemoteRefState::New
        };
        let new_ref = RemoteRef { target: new_target.clone(), state };
        if new_ref.is_tracked() {
            mut_repo.merge_local_bookmark(symbol.name, old.tracked_target(), &new_ref.target).await?;
        }
        mut_repo.set_remote_bookmark(symbol, new_ref);
        stats.changed.push((name, new_target.as_normal().cloned()));
    }
    if !hidable.is_empty() {
        stats.abandoned = abandon_unreachable(mut_repo, hidable).await?;
    }
    Ok(stats)
}

/// Abandons the commits that only the old remote positions `hidable` kept visible: what is not an
/// ancestor of a local bookmark, of an untracked remote bookmark, or of the root.
async fn abandon_unreachable(
    mut_repo: &mut MutableRepo,
    hidable: Vec<CommitId>,
) -> Result<Vec<CommitId>, ImportError> {
    let view = mut_repo.view();
    let pinned_local: Vec<CommitId> =
        view.local_bookmarks().flat_map(|(_, t)| t.added_ids()).cloned().collect();
    let pinned_remote: Vec<CommitId> = view
        .all_remote_bookmarks()
        .filter(|(_, r)| !r.is_tracked())
        .flat_map(|(_, r)| r.target.added_ids())
        .cloned()
        .collect();
    let pinned = RevsetExpression::union_all(&[
        RevsetExpression::commits(pinned_local),
        RevsetExpression::commits(pinned_remote).intersection(&RevsetExpression::visible_heads().ancestors()),
        RevsetExpression::root(),
    ]);
    let abandoned_expr = pinned
        .range(&RevsetExpression::commits(hidable))
        .intersection(&RevsetExpression::visible_heads().ancestors());
    let abandoned: Vec<Commit> =
        abandoned_expr.evaluate(mut_repo)?.stream().commits(mut_repo.store()).try_collect().await?;
    for commit in &abandoned {
        mut_repo.record_abandoned_commit(commit);
    }
    // Descendants of abandoned commits (a working-copy commit on top, say) move to their parents.
    mut_repo.rebase_descendants().await?;
    Ok(abandoned.iter().map(|c| c.id().clone()).collect())
}

/// Which bookmarks a push considers.
#[derive(Clone, Debug, Default)]
pub struct PushSelection {
    /// Named bookmarks: pushed whatever their state, new or deleted ones included.
    pub names: Vec<RefNameBuf>,
    /// Every local bookmark, new ones included.
    pub all: bool,
    /// Also push deletions of tracked bookmarks deleted locally.
    pub deleted: bool,
}

/// One reference to move: `before` is the tracked remote target, `after` the local one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushUpdate {
    pub name: RefNameBuf,
    pub diff: Diff<Option<CommitId>>,
}

/// A bookmark the push cannot move, and what to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub name: RefNameBuf,
    pub message: String,
    pub hint: Option<String>,
}

/// The updates a push makes. Without names or `all`, it considers the tracked bookmarks: those that
/// moved locally, and with `deleted` those deleted locally.
pub fn plan_push(view: &View, remote: &RemoteName, sel: &PushSelection) -> (Vec<PushUpdate>, Vec<Rejected>) {
    let mut updates = Vec::new();
    let mut rejected = Vec::new();
    let mut unknown = Vec::new();
    let mut consider = |name: &RefName, targets: LocalAndRemoteRef, explicit: bool| {
        let symbol = name.to_remote_symbol(remote);
        let reject =
            |message: String, hint: Option<String>| Rejected { name: name.to_owned(), message, hint };
        match classify_ref_push_action(targets) {
            RefPushAction::AlreadyMatches => {}
            RefPushAction::LocalConflicted => rejected.push(reject(
                format!("Bookmark {} is conflicted", name.as_symbol()),
                Some("Run `ajj bookmark list` to inspect, and use `ajj bookmark set` to fix it up.".into()),
            )),
            RefPushAction::RemoteConflicted => rejected.push(reject(
                format!("Bookmark {symbol} is conflicted"),
                Some("Run `ajj dstore fetch` to update the conflicted remote bookmark.".into()),
            )),
            RefPushAction::RemoteUntracked => rejected.push(reject(
                format!("Non-tracking remote bookmark {symbol} exists"),
                Some(format!("Run `ajj bookmark track {symbol}` to import the remote bookmark.")),
            )),
            RefPushAction::Update(diff) => {
                if diff.after.is_none() && !(explicit || sel.deleted) {
                    return;
                }
                if !(targets.remote_ref.is_tracked() || explicit || sel.all) {
                    return;
                }
                updates.push(PushUpdate { name: name.to_owned(), diff });
            }
        }
    };
    if sel.names.is_empty() {
        for (name, targets) in view.local_remote_bookmarks(remote) {
            let tracked = targets.remote_ref.is_tracked();
            if (sel.all && targets.local_target.is_present()) || tracked {
                consider(name, targets, false);
            }
        }
    } else {
        for name in &sel.names {
            let targets = LocalAndRemoteRef {
                local_target: view.get_local_bookmark(name),
                remote_ref: view.get_remote_bookmark(name.to_remote_symbol(remote)),
            };
            if targets.local_target.is_absent() && targets.remote_ref.is_absent() {
                unknown.push(Rejected {
                    name: name.clone(),
                    message: format!("No such bookmark: {}", name.as_symbol()),
                    hint: None,
                });
                continue;
            }
            consider(name, targets, true);
        }
    }
    rejected.extend(unknown);
    (updates, rejected)
}

/// Records a pushed update: the remote bookmark now points where the local one does, tracked.
pub fn record_pushed(mut_repo: &mut MutableRepo, remote: &RemoteName, update: &PushUpdate) {
    let new_ref =
        RemoteRef { target: RefTarget::resolved(update.diff.after.clone()), state: RemoteRefState::Tracked };
    mut_repo.set_remote_bookmark(update.name.to_remote_symbol(remote), new_ref);
}
