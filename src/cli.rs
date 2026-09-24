//! The `ajj` binary: jj's CLI with the amber backend registered and the `ajj dstore` commands.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use amber_store_core::key::Key;
use dstore_client::human_bytes;
use jj_cli::cli_util::{CliRunner, CommandHelper, WorkspaceCommandHelper};
use jj_cli::command_error::{CommandError, internal_error, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::{RefNameBuf, RemoteName, RemoteNameBuf};
use jj_lib::repo::{Repo as _, StoreFactories};
use jj_lib::signing::Signer;
use jj_lib::workspace::{Workspace, WorkspaceInitError};

use crate::backend::{AmberBackend, BACKEND_NAME};
use crate::bookmarks::{self, PushSelection};
use crate::dstore::{self, Moved, Remote, Remotes, Session};
use crate::progress::Meter;

#[derive(clap::Parser, Clone, Debug)]
enum AjjCommand {
    #[command(subcommand)]
    Dstore(DstoreCommand),
}

/// Store commits in amber and share bookmarks through a dstore cluster
///
/// Bookmark `B` on remote `R` is the dstore reference `<prefix of R>/B`, holding the key of an amber
/// commit. `fetch` and `push` keep `B@R` in step with it, as `jj git fetch/push` do for git.
#[derive(clap::Subcommand, Clone, Debug)]
enum DstoreCommand {
    Init(InitArgs),
    Clone(CloneArgs),
    Fetch(FetchArgs),
    Push(PushArgs),
    #[command(subcommand)]
    Remote(RemoteCommand),
}

#[derive(clap::Args, Clone, Debug)]
struct NetArgs {
    /// Directory of the repository's references on the cluster: `--prefix myrepo` maps bookmark
    /// `main` to reference `myrepo/main`
    #[arg(long, default_value = "")]
    prefix: String,
    /// Custom iroh relay URL
    #[arg(long, default_value = "")]
    relay: String,
    /// Disable iroh relays (direct connections only)
    #[arg(long)]
    no_relay: bool,
    /// Disable mDNS and DNS node discovery
    #[arg(long)]
    no_discovery: bool,
}

impl NetArgs {
    fn remote(&self, ticket: &str) -> Remote {
        Remote {
            ticket: ticket.to_owned(),
            prefix: dstore::normalize_prefix(&self.prefix),
            relay: self.relay.clone(),
            no_relay: self.no_relay,
            no_discovery: self.no_discovery,
        }
    }
}

/// Create a repository whose commits are amber objects
#[derive(clap::Args, Clone, Debug)]
struct InitArgs {
    /// The destination directory
    #[arg(default_value = ".", value_hint = clap::ValueHint::DirPath)]
    destination: String,
}

/// Create a repository and fetch a dstore remote into it
#[derive(clap::Args, Clone, Debug)]
struct CloneArgs {
    /// Ticket of the dstore cluster (`dstore1…`, or node ids)
    #[arg(long, env = "DSTORE_TICKET", hide_env_values = true)]
    ticket: String,
    /// The destination directory
    #[arg(value_hint = clap::ValueHint::DirPath)]
    destination: String,
    /// Name of the remote
    #[arg(long = "remote", default_value = "origin")]
    remote_name: RemoteNameBuf,
    /// Bookmark to check out; defaults to main, master or trunk
    #[arg(long, short)]
    branch: Option<String>,
    #[command(flatten)]
    net: NetArgs,
}

/// Fetch the branches of a dstore remote
#[derive(clap::Args, Clone, Debug)]
struct FetchArgs {
    /// The remote; defaults to `origin`, or the only remote
    #[arg(long)]
    remote: Option<RemoteNameBuf>,
    /// Leave new remote bookmarks untracked
    #[arg(long)]
    no_track: bool,
    /// Ticket for this run, overriding the stored one and `$DSTORE_TICKET`
    #[arg(long)]
    ticket: Option<String>,
}

/// Push bookmarks to a dstore remote
///
/// Without `-b` or `--all`, pushes the bookmarks that track the remote and moved locally. Each
/// reference moves only if it still names the commit last fetched or pushed.
#[derive(clap::Args, Clone, Debug)]
struct PushArgs {
    /// The remote; defaults to `origin`, or the only remote
    #[arg(long)]
    remote: Option<RemoteNameBuf>,
    /// Push this bookmark (new and deleted bookmarks included); repeatable
    #[arg(long = "bookmark", short = 'b')]
    bookmarks: Vec<RefNameBuf>,
    /// Push every local bookmark
    #[arg(long, conflicts_with = "bookmarks")]
    all: bool,
    /// Also push deletions of tracked bookmarks
    #[arg(long)]
    deleted: bool,
    /// Only print what would be pushed
    #[arg(long)]
    dry_run: bool,
    /// Ticket for this run, overriding the stored one and `$DSTORE_TICKET`
    #[arg(long)]
    ticket: Option<String>,
}

/// Manage dstore remotes
#[derive(clap::Subcommand, Clone, Debug)]
enum RemoteCommand {
    /// Add a remote
    ///
    /// Without a ticket (neither `--ticket` nor `$DSTORE_TICKET`), none is stored, and fetch and push
    /// take `$DSTORE_TICKET` when they run.
    Add {
        name: RemoteNameBuf,
        /// Ticket of the dstore cluster (`dstore1…`, or node ids)
        #[arg(long, env = "DSTORE_TICKET", hide_env_values = true, default_value = "")]
        ticket: String,
        #[command(flatten)]
        net: NetArgs,
    },
    /// Remove a remote and its remote bookmarks
    Remove { name: RemoteNameBuf },
    /// List remotes
    List,
}

fn create_store_factories() -> StoreFactories {
    let mut factories = StoreFactories::empty();
    factories.add_backend(
        BACKEND_NAME,
        Box::new(|settings, store_path| Ok(Box::new(AmberBackend::load(settings, store_path)?))),
    );
    factories
}

pub fn run() -> std::process::ExitCode {
    CliRunner::init()
        .name("ajj")
        .about("Jujutsu (jj) with commits stored in amber-store and bookmarks shared through dstore")
        .version(env!("CARGO_PKG_VERSION"))
        .add_store_factories(create_store_factories())
        .add_subcommand(run_command)
        .run()
        .into()
}

async fn run_command(ui: &mut Ui, command: &CommandHelper, cmd: AjjCommand) -> Result<(), CommandError> {
    let AjjCommand::Dstore(cmd) = cmd;
    match cmd {
        DstoreCommand::Init(args) => cmd_init(ui, command, &args).await,
        DstoreCommand::Clone(args) => cmd_clone(ui, command, &args).await,
        DstoreCommand::Fetch(args) => cmd_fetch(ui, command, &args).await,
        DstoreCommand::Push(args) => cmd_push(ui, command, &args).await,
        DstoreCommand::Remote(args) => cmd_remote(ui, command, args).await,
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, CommandError> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(internal_error)
}

fn backend(ws: &WorkspaceCommandHelper) -> Result<&AmberBackend, CommandError> {
    ws.repo().store().backend_impl::<AmberBackend>().ok_or_else(|| {
        user_error(format!(
            "This repository does not use the {BACKEND_NAME} backend; create one with `ajj dstore init`"
        ))
    })
}

fn map_dstore(e: dstore::Error) -> CommandError {
    match e {
        dstore::Error::Changed { .. } => user_error(e).hinted("Run `ajj dstore fetch`, then push again."),
        e => user_error(e),
    }
}

fn pick_remote(
    remotes: &Remotes,
    name: Option<&RemoteName>,
) -> Result<(RemoteNameBuf, Remote), CommandError> {
    let name: RemoteNameBuf = match name {
        Some(n) => n.to_owned(),
        None if remotes.remotes.contains_key("origin") => "origin".into(),
        None if remotes.remotes.len() == 1 => remotes.remotes.keys().next().unwrap().as_str().into(),
        None if remotes.remotes.is_empty() => {
            return Err(user_error("No dstore remote is configured")
                .hinted("Add one with `ajj dstore remote add origin --ticket <ticket> --prefix <repo>`."));
        }
        None => return Err(user_error("Several dstore remotes are configured; pick one with --remote")),
    };
    let remote = remotes
        .remotes
        .get(name.as_str())
        .cloned()
        .ok_or_else(|| user_error(format!("No such dstore remote: {}", name.as_symbol())))?;
    Ok((name, remote))
}

async fn init_workspace(
    ui: &Ui,
    command: &CommandHelper,
    wc_path: &Path,
) -> Result<WorkspaceCommandHelper, CommandError> {
    let settings = command.settings_for_new_workspace(ui, wc_path)?.0;
    let (workspace, repo) = Workspace::init_with_backend(
        &settings,
        wc_path,
        &|settings, store_path| Ok(Box::new(AmberBackend::init(settings, store_path)?)),
        Signer::from_settings(&settings).map_err(WorkspaceInitError::SignInit)?,
    )
    .await?;
    command.for_workable_repo(ui, workspace, repo)
}

fn prepare_destination(command: &CommandHelper, destination: &str) -> Result<(PathBuf, bool), CommandError> {
    let wc_path = command.cwd().join(destination);
    let existed = wc_path.exists();
    if existed && wc_path.join(".jj").exists() {
        return Err(user_error(format!("{} is already a jj repository", wc_path.display())));
    }
    if existed && destination != "." && std::fs::read_dir(&wc_path)?.next().is_some() {
        return Err(user_error("Destination path exists and is not an empty directory"));
    }
    std::fs::create_dir_all(&wc_path)?;
    let wc_path = wc_path.canonicalize()?;
    Ok((wc_path, existed))
}

async fn cmd_init(ui: &mut Ui, command: &CommandHelper, args: &InitArgs) -> Result<(), CommandError> {
    let (wc_path, _) = prepare_destination(command, &args.destination)?;
    init_workspace(ui, command, &wc_path).await?;
    writeln!(ui.status(), "Initialized repo in \"{}\"", wc_path.display())?;
    Ok(())
}

async fn cmd_clone(ui: &mut Ui, command: &CommandHelper, args: &CloneArgs) -> Result<(), CommandError> {
    let remote = args.net.remote(&args.ticket);
    remote.validate().map_err(user_error)?;
    let (wc_path, existed) = prepare_destination(command, &args.destination)?;
    let result = async {
        let mut ws = init_workspace(ui, command, &wc_path).await?;
        let amber_dir = backend(&ws)?.path().to_owned();
        let mut remotes = Remotes::default();
        remotes.remotes.insert(args.remote_name.as_str().to_owned(), remote.clone());
        remotes.save(&amber_dir).map_err(user_error)?;
        writeln!(ui.status(), "Fetching into new repo in \"{}\"", wc_path.display())?;
        fetch_remote(ui, &mut ws, &args.remote_name, &remote, true).await?;
        Ok::<_, CommandError>(ws)
    }
    .await;
    let mut ws = match result {
        Ok(ws) => ws,
        Err(e) => {
            let _ = std::fs::remove_dir_all(wc_path.join(".jj"));
            if !existed {
                let _ = std::fs::remove_dir(&wc_path);
            }
            return Err(e);
        }
    };

    let view = ws.repo().view();
    let candidates: Vec<String> = match &args.branch {
        Some(b) => vec![b.clone()],
        None => ["main", "master", "trunk"].iter().map(|s| s.to_string()).collect(),
    };
    let working = candidates.into_iter().find_map(|b| {
        let name = RefNameBuf::from(b.as_str());
        let target =
            view.get_remote_bookmark(name.to_remote_symbol(&args.remote_name)).target.as_normal().cloned();
        target.map(|id| (name, id))
    });
    match working {
        Some((name, id)) => {
            let mut tx = ws.start_transaction();
            let commit = tx.repo().store().get_commit_async(&id).await?;
            tx.check_out(&commit)?;
            tx.finish(ui, format!("check out dstore remote's branch: {}", name.as_symbol())).await?;
        }
        None if args.branch.is_some() => {
            return Err(user_error(format!(
                "No branch {} on remote {}",
                args.branch.as_deref().unwrap_or_default(),
                args.remote_name.as_symbol()
            )));
        }
        None => {}
    }
    Ok(())
}

async fn cmd_fetch(ui: &mut Ui, command: &CommandHelper, args: &FetchArgs) -> Result<(), CommandError> {
    let mut ws = command.workspace_helper(ui).await?;
    let remotes = Remotes::load(backend(&ws)?.path()).map_err(user_error)?;
    let (name, remote) = pick_remote(&remotes, args.remote.as_deref())?;
    let remote = remote.for_run(args.ticket.as_deref()).map_err(user_error)?;
    fetch_remote(ui, &mut ws, &name, &remote, !args.no_track).await
}

/// Lists the remote's branches, copies their commits into the local store, and updates the remote
/// bookmarks in one transaction.
async fn fetch_remote(
    ui: &Ui,
    ws: &mut WorkspaceCommandHelper,
    remote_name: &RemoteName,
    remote: &Remote,
    track_new: bool,
) -> Result<(), CommandError> {
    let store = Arc::clone(backend(ws)?.store());
    let meter = Meter::new(ui);
    let rt = runtime()?;
    let result = rt.block_on(async {
        meter.phase("Connecting to dstore");
        let session = Session::dial(remote).await?;
        let result = async {
            meter.phase(format!("Listing {}", remote.ref_prefix()));
            let (branches, other) = dstore::list_branches(&session.cluster, &remote.ref_prefix()).await?;
            let mut moved = Moved::default();
            for b in &branches {
                meter.phase(format!("Fetching {}", remote.ref_name(&b.bookmark)));
                moved += dstore::fetch(&session.cluster, &store, b.key, meter.callback()).await?;
            }
            Ok::<_, dstore::Error>((branches, other, moved))
        }
        .await;
        meter.phase("Disconnecting");
        session.close().await;
        result
    });
    meter.clear();
    drop(rt);
    let (branches, other, moved) = result.map_err(map_dstore)?;
    if moved.objects > 0 {
        writeln!(ui.status(), "Fetched {} objects ({}).", moved.objects, human_bytes(moved.bytes))?;
    }

    for name in &other {
        writeln!(ui.warning_default(), "Skipping dstore reference {name}: it does not name a commit")?;
    }
    let fetched: BTreeMap<RefNameBuf, CommitId> = branches
        .iter()
        .map(|b| (RefNameBuf::from(b.bookmark.as_str()), CommitId::new(b.key.as_bytes().to_vec())))
        .collect();

    let mut tx = ws.start_transaction();
    let stats = bookmarks::import_remote_branches(tx.repo_mut(), remote_name, &fetched, track_new)
        .await
        .map_err(internal_error)?;
    if stats.changed.is_empty() {
        writeln!(ui.status(), "Nothing changed.")?;
        return Ok(());
    }
    let mut summary = String::new();
    for (name, target) in &stats.changed {
        let symbol = name.to_remote_symbol(remote_name);
        match target {
            Some(id) => writeln!(summary, "  {symbol} -> {}", short(id)).unwrap(),
            None => writeln!(summary, "  {symbol} deleted").unwrap(),
        }
    }
    if !stats.abandoned.is_empty() {
        writeln!(summary, "Abandoned {} commits that are no longer reachable.", stats.abandoned.len())
            .unwrap();
    }
    tx.finish(ui, format!("fetch from dstore remote {}", remote_name.as_symbol())).await?;
    write!(ui.status(), "Fetched from {}:\n{summary}", remote_name.as_symbol())?;
    Ok(())
}

async fn cmd_push(ui: &mut Ui, command: &CommandHelper, args: &PushArgs) -> Result<(), CommandError> {
    let mut ws = command.workspace_helper(ui).await?;
    let remotes = Remotes::load(backend(&ws)?.path()).map_err(user_error)?;
    let (remote_name, remote) = pick_remote(&remotes, args.remote.as_deref())?;
    let remote = remote.for_run(args.ticket.as_deref()).map_err(user_error)?;
    let store = Arc::clone(backend(&ws)?.store());

    let sel = PushSelection { names: args.bookmarks.clone(), all: args.all, deleted: args.deleted };
    let (updates, rejected) = bookmarks::plan_push(ws.repo().view(), &remote_name, &sel);
    for r in &rejected {
        writeln!(ui.warning_default(), "{}", r.message)?;
        if let Some(hint) = &r.hint {
            writeln!(ui.hint_default(), "{hint}")?;
        }
    }
    if !rejected.is_empty() {
        return Err(user_error("Refusing to push: some bookmarks cannot be pushed"));
    }
    if updates.is_empty() {
        writeln!(ui.status(), "Nothing changed.")?;
        return Ok(());
    }
    let root = ws.repo().store().root_commit_id().clone();
    let mut plan = Vec::new();
    for u in &updates {
        if u.diff.after.as_ref() == Some(&root) {
            return Err(user_error(format!("Bookmark {} points at the root commit", u.name.as_symbol())));
        }
        let key_of = |id: &Option<CommitId>| -> Result<Option<Key>, CommandError> {
            match id {
                None => Ok(None),
                Some(id) => backend(&ws)?.commit_key(id).map_err(internal_error),
            }
        };
        plan.push((
            u.clone(),
            remote.ref_name(u.name.as_str()),
            key_of(&u.diff.before)?,
            key_of(&u.diff.after)?,
        ));
    }

    writeln!(ui.status(), "Changes to push to {}:", remote_name.as_symbol())?;
    for (u, ref_name, _, _) in &plan {
        let line = match (&u.diff.before, &u.diff.after) {
            (None, Some(a)) => format!("Add bookmark {} to {}", u.name.as_symbol(), short(a)),
            (Some(b), Some(a)) => {
                format!("Move bookmark {} from {} to {}", u.name.as_symbol(), short(b), short(a))
            }
            (Some(b), None) => format!("Delete bookmark {} from {}", u.name.as_symbol(), short(b)),
            (None, None) => continue,
        };
        writeln!(ui.status(), "  {line} (reference {ref_name})")?;
    }
    if args.dry_run {
        writeln!(ui.status(), "Dry-run requested, not pushing.")?;
        return Ok(());
    }

    let settings = ws.settings();
    let user = dstore::ref_user(settings.user_name(), settings.user_email());
    let meter = Meter::new(ui);
    let rt = runtime()?;
    let results = rt.block_on(async {
        meter.phase("Connecting to dstore");
        let session = Session::dial(&remote).await?;
        let mut results = Vec::new();
        for (_, ref_name, before, after) in &plan {
            let r = match (before, after) {
                (_, Some(a)) => {
                    meter.phase(format!("Pushing {ref_name}"));
                    dstore::put_branch(
                        &session.cluster,
                        &store,
                        ref_name,
                        *a,
                        *before,
                        &user,
                        meter.callback(),
                    )
                    .await
                }
                (Some(b), None) => {
                    meter.phase(format!("Deleting {ref_name}"));
                    dstore::delete_branch(&session.cluster, ref_name, *b).await.map(|()| Moved::default())
                }
                (None, None) => Ok(Moved::default()),
            };
            results.push(r);
        }
        meter.phase("Disconnecting");
        session.close().await;
        Ok::<_, dstore::Error>(results)
    });
    meter.clear();
    drop(rt);
    let results = results.map_err(map_dstore)?;
    let mut moved = Moved::default();
    for m in results.iter().flatten() {
        moved += *m;
    }
    writeln!(ui.status(), "Uploaded {} objects ({}).", moved.objects, human_bytes(moved.bytes))?;

    let mut tx = ws.start_transaction();
    let mut first_err = None;
    for ((u, _, _, _), r) in plan.iter().zip(results) {
        match r {
            Ok(_) => bookmarks::record_pushed(tx.repo_mut(), &remote_name, u),
            Err(e) => {
                writeln!(ui.warning_default(), "Failed to push {}: {e}", u.name.as_symbol())?;
                first_err.get_or_insert(e);
            }
        }
    }
    if tx.repo().has_changes() {
        tx.finish(ui, format!("push to dstore remote {}", remote_name.as_symbol())).await?;
    }
    match first_err {
        Some(e) => Err(map_dstore(e)),
        None => Ok(()),
    }
}

async fn cmd_remote(ui: &mut Ui, command: &CommandHelper, cmd: RemoteCommand) -> Result<(), CommandError> {
    let mut ws = command.workspace_helper_no_snapshot(ui).await?;
    let amber_dir = backend(&ws)?.path().to_owned();
    let mut remotes = Remotes::load(&amber_dir).map_err(user_error)?;
    match cmd {
        RemoteCommand::Add { name, ticket, net } => {
            if name.as_str() == "git" {
                return Err(user_error("The remote name `git` is reserved by jj"));
            }
            if remotes.remotes.contains_key(name.as_str()) {
                return Err(user_error(format!("Remote {} already exists", name.as_symbol())));
            }
            let remote = net.remote(&ticket);
            remote.validate().map_err(user_error)?;
            remotes.remotes.insert(name.as_str().to_owned(), remote);
            remotes.save(&amber_dir).map_err(user_error)?;
        }
        RemoteCommand::Remove { name } => {
            if remotes.remotes.remove(name.as_str()).is_none() {
                return Err(user_error(format!("No such dstore remote: {}", name.as_symbol())));
            }
            remotes.save(&amber_dir).map_err(user_error)?;
            let mut tx = ws.start_transaction();
            tx.repo_mut().remove_remote(&name);
            if tx.repo().has_changes() {
                tx.finish(ui, format!("remove dstore remote {}", name.as_symbol())).await?;
            }
        }
        RemoteCommand::List => {
            let mut out = ui.stdout();
            for (name, r) in &remotes.remotes {
                let prefix = if r.prefix.is_empty() {
                    String::new()
                } else {
                    format!(" prefix={}", dstore::normalize_prefix(&r.prefix))
                };
                let ticket = if r.ticket.is_empty() { "$DSTORE_TICKET" } else { &r.ticket };
                writeln!(out, "{name} {ticket}{prefix}")?;
            }
        }
    }
    Ok(())
}

fn short(id: &CommitId) -> String {
    id.hex()[..16].to_owned()
}
