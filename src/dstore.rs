//! Talking to a dstore cluster: remotes, dialing, listing branches, moving commits and references.
//!
//! A **remote** is a dstore cluster (its ticket) plus a reference-name prefix. The jj bookmark `B`
//! on remote `R` is the dstore reference `<prefix of R>B`, and a reference is a branch when its key is
//! a commit key. This module knows nothing about jj's view; [`crate::cli`] maps these operations onto
//! remote-tracking bookmarks.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use amber_store_core::key::{Key, Type};
use amber_store_core::packstore;
use dstore_client::{Cluster, Cond, Config as ClientConfig, Ctx, Logger, PullStats};
use dstore_gocompat::slog::{Attr, Handler, Level, Record};
use dstore_transport::Endpoint;
use dstore_transport_iroh::{IrohConfig, IrohEndpoint, bind_iroh, generate_secret_key, relay_mode_of};
use serde::{Deserialize, Serialize};

/// How long closing the endpoint may take (dstore's CLI uses the same bound).
const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),
    #[error(transparent)]
    Client(#[from] dstore_client::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("dstore reference {name} changed on the remote (now {current}); fetch first")]
    Changed { name: String, current: String },
}

/// One configured dstore remote.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    /// A `dstore1…` ticket, or node ids; empty to take `$DSTORE_TICKET` at each use.
    pub ticket: String,
    /// Prepended to a bookmark name to form the reference name, e.g. `myrepo/`.
    #[serde(default)]
    pub prefix: String,
    /// A custom relay URL; empty for iroh's default relays.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub relay: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_relay: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_discovery: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Remote {
    pub fn ref_name(&self, bookmark: &str) -> String {
        format!("{}{}", self.prefix, bookmark)
    }

    /// The remote as one run uses it: `ticket` given for the run, else the stored ticket, else
    /// `$DSTORE_TICKET` (dstore's working copies resolve theirs in the same order).
    pub fn for_run(&self, ticket: Option<&str>) -> Result<Remote, Error> {
        let env = std::env::var("DSTORE_TICKET").unwrap_or_default();
        let ticket = match ticket {
            Some(t) => t.to_owned(),
            None if !self.ticket.is_empty() => self.ticket.clone(),
            None if !env.is_empty() => env,
            None => {
                return Err(Error::Msg(
                    "no ticket: the remote stores none; pass --ticket or set DSTORE_TICKET".into(),
                ));
            }
        };
        let r = Remote { ticket, ..self.clone() };
        r.validate()?;
        Ok(r)
    }

    /// Checks the ticket, when one is stored, and that the prefix makes valid reference names.
    pub fn validate(&self) -> Result<(), Error> {
        if !self.ticket.is_empty() {
            dstore_ticket::parse(self.ticket.as_bytes()).map_err(|e| Error::Msg(e.to_string()))?;
        }
        if !self.prefix.is_empty() {
            dstore_client::validate_name_bytes(self.ref_name("x").as_bytes())
                .map_err(|e| Error::Msg(format!("prefix {:?}: {e}", self.prefix)))?;
        }
        relay_mode_of(&self.relay, self.no_relay).map_err(Error::Msg)?;
        Ok(())
    }
}

/// The remotes of one repo, kept in `.jj/repo/store/amber/remotes.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remotes {
    #[serde(default)]
    pub remotes: BTreeMap<String, Remote>,
}

impl Remotes {
    pub fn path(amber_dir: &Path) -> PathBuf {
        amber_dir.join("remotes.json")
    }

    pub fn load(amber_dir: &Path) -> Result<Remotes, Error> {
        match std::fs::read(Self::path(amber_dir)) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| Error::Msg(format!("{}: {e}", Self::path(amber_dir).display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Remotes::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, amber_dir: &Path) -> Result<(), Error> {
        let path = Self::path(amber_dir);
        let tmp = path.with_extension("json.tmp");
        let mut b = serde_json::to_vec_pretty(self).map_err(|e| Error::Msg(e.to_string()))?;
        b.push(b'\n');
        std::fs::write(&tmp, b)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

struct Discard;

impl Handler for Discard {
    fn enabled(&self, _level: Level) -> bool {
        false
    }
    fn handle(&self, _handler_attrs: &[Attr], _r: &Record) {}
}

/// dstore's client logs every dial and push at INFO; jj's own output reports what happened.
pub fn quiet_logger() -> Logger {
    Logger::new(Arc::new(Discard))
}

/// A dialed cluster, and the iroh endpoint when this process bound one.
pub struct Session {
    pub cluster: Cluster,
    endpoint: Option<Arc<IrohEndpoint>>,
}

impl Session {
    /// Binds an iroh endpoint and dials the remote's cluster. Needs a multi-thread tokio runtime.
    pub async fn dial(remote: &Remote) -> Result<Session, Error> {
        let ctx = Ctx::background();
        let ticket = dstore_ticket::parse(remote.ticket.as_bytes()).map_err(|e| Error::Msg(e.to_string()))?;
        let relay = relay_mode_of(&remote.relay, remote.no_relay).map_err(Error::Msg)?;
        let log = quiet_logger();
        let endpoint = bind_iroh(
            &ctx,
            IrohConfig {
                secret_key: generate_secret_key(),
                alpns: Vec::new(),
                relay,
                advertise: None,
                bind_addr: None,
                loopback: false,
                direct_timeout: None,
                discover: !remote.no_discovery,
                announce: false,
                logger: Some(log.clone()),
            },
        )
        .await
        .map_err(|e| Error::Msg(format!("dstore: {e}")))?;
        let ep: Arc<dyn Endpoint> = endpoint.clone();
        let cfg = ClientConfig { endpoint: Some(ep), ticket, logger: Some(log), ..ClientConfig::default() };
        match Cluster::dial(&ctx, cfg).await {
            Ok(cluster) => Ok(Session { cluster, endpoint: Some(endpoint) }),
            Err(e) => {
                Endpoint::close(&*endpoint).await;
                Err(e.into())
            }
        }
    }

    /// A session over a cluster dialed elsewhere (tests use an in-memory transport).
    pub fn from_cluster(cluster: Cluster) -> Session {
        Session { cluster, endpoint: None }
    }

    pub async fn close(self) {
        self.cluster.close();
        if let Some(ep) = self.endpoint {
            ep.close_bounded(CLOSE_TIMEOUT).await;
        }
    }
}

/// A reference under a remote's prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRef {
    /// The reference name without the prefix: the bookmark name.
    pub bookmark: String,
    pub key: Key,
}

/// Lists the references under `prefix`. Those naming commits are branches; the names of the others
/// (trees pushed by `dstore push` without a branch) come back separately.
pub async fn list_branches(cl: &Cluster, prefix: &str) -> Result<(Vec<RemoteRef>, Vec<String>), Error> {
    let ctx = Ctx::background();
    let infos = cl.ref_list(&ctx, prefix.as_bytes()).await?;
    let mut branches = Vec::new();
    let mut other = Vec::new();
    for info in infos {
        let Some(bookmark) = info.name.strip_prefix(prefix) else { continue };
        let key = info.key.as_deref().and_then(|k| Key::parse(k).ok());
        match key {
            Some(key) if key.type_() == Type::Commit && !bookmark.is_empty() => {
                branches.push(RemoteRef { bookmark: bookmark.to_owned(), key })
            }
            _ => other.push(info.name),
        }
    }
    Ok((branches, other))
}

/// Copies the closure of `key` (for a commit: its trees and its whole history) into `local`.
/// Subtrees already complete locally are not fetched again. Returns the number of objects fetched.
pub async fn fetch(cl: &Cluster, local: &Arc<packstore::Store>, key: Key) -> Result<i64, Error> {
    let ctx = Ctx::background();
    let mut st = PullStats::default();
    cl.pull_tree(&ctx, Arc::clone(local), key, &mut st, None).await?;
    Ok(st.fetched)
}

/// Uploads the closure of `key` and points the reference `name` at it, provided the reference still
/// names `expected_old` (`None`: the reference must not exist). Returns the objects uploaded.
pub async fn put_branch(
    cl: &Cluster,
    local: &Arc<packstore::Store>,
    name: &str,
    key: Key,
    expected_old: Option<Key>,
    user: &str,
) -> Result<i64, Error> {
    let ctx = Ctx::background();
    let cond = keyed(expected_old);
    match cl.push(&ctx, Arc::clone(local), key, name, user, cond, None).await {
        Ok(ps) => Ok(ps.uploaded),
        Err(e) => match e.cas_mismatch() {
            // The reference already names our commit: an earlier push got this far.
            Some(cm) if cm.has_current && cm.current == key.as_bytes() => Ok(0),
            Some(cm) => Err(Error::Changed { name: name.to_owned(), current: describe_current(cm) }),
            None => Err(e.into()),
        },
    }
}

/// Deletes the reference `name`, provided it still names `expected_old`.
pub async fn delete_branch(cl: &Cluster, name: &str, expected_old: Key) -> Result<(), Error> {
    let ctx = Ctx::background();
    match cl.ref_delete(&ctx, name, &keyed(Some(expected_old))).await {
        Ok(()) => Ok(()),
        Err(e) if e.is_unknown_ref() => Ok(()),
        Err(e) => match e.cas_mismatch() {
            Some(cm) if !cm.has_current => Ok(()),
            Some(cm) => Err(Error::Changed { name: name.to_owned(), current: describe_current(cm) }),
            None => Err(e.into()),
        },
    }
}

fn keyed(expected_old: Option<Key>) -> Cond {
    Cond {
        keyed: true,
        expected_old: expected_old.map(|k| k.as_bytes().to_vec()).unwrap_or_default(),
        ..Cond::default()
    }
}

fn describe_current(cm: &dstore_client::CasMismatch) -> String {
    if cm.has_current { hex::encode(&cm.current) } else { "absent".to_owned() }
}

/// The user recorded on references: dstore's user-name rules are stricter than jj's.
pub fn ref_user(name: &str, email: &str) -> String {
    for candidate in [email, name] {
        if !candidate.is_empty() && dstore_client::validate_user_bytes(candidate.as_bytes()).is_ok() {
            return candidate.to_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "5b7f9fd5b7d899a67bdf2acb82a52d164593cb836f5a3b7546899bf5f3334c27";
    const T2: &str = "6c8fa0e6c8e9aab78ce03bdc93b63e275604dc947c4b86578a9aac06a4445d38";

    #[test]
    fn a_run_ticket_overrides_the_stored_one() {
        let stored = Remote { ticket: T1.into(), prefix: "p/".into(), ..Remote::default() };
        assert_eq!(stored.for_run(None).unwrap().ticket, T1);
        let run = stored.for_run(Some(T2)).unwrap();
        assert_eq!(run.ticket, T2);
        assert_eq!(run.prefix, "p/");
        assert!(stored.for_run(Some("not a ticket")).is_err());
    }

    #[test]
    fn a_remote_may_store_no_ticket() {
        let r = Remote { prefix: "p/".into(), ..Remote::default() };
        r.validate().unwrap();
        assert_eq!(r.for_run(Some(T1)).unwrap().ticket, T1);
        let json =
            serde_json::to_string(&Remotes { remotes: [("origin".into(), r.clone())].into() }).unwrap();
        let back: Remotes = serde_json::from_str(&json).unwrap();
        assert_eq!(back.remotes["origin"], r);
    }
}
