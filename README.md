# ajj

`ajj` is [jj (Jujutsu)](https://jj-vcs.github.io/jj/) with a commit backend whose objects are
[amber-store](https://github.com/amber-store/core) objects. Bookmarks are shared through a
[dstore](https://github.com/amber-store/dstore) cluster:

- A jj commit is an amber `Commit` object (CAS type 5), and its commit id is the amber key.
- The bookmark `main` on a remote is the dstore reference `<prefix>main`, holding that key.

Everything else is jj 0.45.1, unchanged: `ajj log`, `ajj new`, `ajj rebase`, `ajj bookmark`,
`ajj op log` and so on.

```sh
ajj dstore clone dstore1… myrepo --prefix myrepo/   # fetch every branch under myrepo/, check out main
cd myrepo
echo hi > hello.txt && ajj commit -m hello
ajj bookmark move main --to @-
ajj dstore push                                     # moves reference myrepo/main
```

Because the objects are amber's own, dstore's working copies share the same branches.
`dstore clone myrepo/main` checks out a jj-made commit, and a `dstore push -m …` on it is a commit
that `ajj dstore fetch` imports.

## Commands

| Command | What it does |
| --- | --- |
| `ajj dstore init [DIR]` | Create a repository with the amber backend. |
| `ajj dstore clone TICKET DIR [--prefix P] [--remote origin] [-b BRANCH]` | Create a repository and add the remote. Fetch its branches, tracking them, then check out `main`, `master` or `trunk` (or `-b`). |
| `ajj dstore remote add NAME TICKET [--prefix P]` | Add a remote. |
| `ajj dstore remote remove NAME`, `ajj dstore remote list` | Remove or list remotes. |
| `ajj dstore fetch [--remote R] [--no-track]` | Fetch the remote's branches and update the remote bookmarks. |
| `ajj dstore push [--remote R] [-b NAME]… [--all] [--deleted] [--dry-run]` | Move references to where the local bookmarks point. |

The network flags `--relay URL`, `--no-relay` and `--no-discovery` are stored with the remote and
mean what they mean to `dstore`. The remotes are kept in `.jj/repo/store/amber/remotes.json`.

**Fetch** does what `jj git fetch` does, on the references under the remote's prefix:
- Each reference naming a commit is a branch. The closure of each branch (the commit, its trees,
  all of its history) is copied into the local store.
- The remote bookmark `name@remote` is set to that commit, and a tracked one is merged into the
  local bookmark. If both moved, the local bookmark becomes conflicted.
- Commits that only the old remote position kept visible are abandoned.
- References that do not name a commit, such as trees from `dstore store push`, are skipped with a
  warning.
- New remote bookmarks are tracked unless you pass `--no-track`.

**Push** does what `jj git push` does:
- With no arguments it pushes the tracked bookmarks that moved. `-b NAME` pushes that bookmark,
  including a new or deleted one. `--all` pushes every local bookmark, and `--deleted` adds
  deletions of tracked bookmarks.
- Each reference moves only if it still names the commit last fetched or pushed. This is a
  compare-and-swap on the key. Otherwise the push fails with
  `dstore reference … changed on the remote`, and you fetch, resolve and push again.
- The upload is dstore's own push: the missing part of the commit's closure goes to the nodes that
  own it, and then the reference is written. A dstore node refuses a reference whose closure is not
  complete.

## The mapping

### The backend: `src/backend.rs`

Objects live in a core-rs packstore at `.jj/repo/store/amber/packstore`. Any number of jj
processes can share it. Every jj id is a 32-byte amber key:

| jj | amber |
| --- | --- |
| `FileId` | The file's root key: a `Blob`, or a `FileNode` over 1 MiB-max chunks, chunked exactly as `amber-store ingest` does (the same bytes give the same key). |
| `SymlinkId` | The key of a `Blob` holding the target. Tree entries keep the target inline (`S_IFLNK`, mode `0120777`). |
| `TreeId` | A `DirLeaf`, or a `DirNode` for large directories. Entries are `0100644` or `0100755` files and `040755` directories, with uid, gid and mtime zero. |
| `TreeValue::GitSubmodule(id)` | A directory entry holding a commit key, which amber reads as that commit's tree. |
| `CommitId` | A `Commit` key. The root commit is the virtual all-zero id and is never stored. |

Reading a tree that `ingest` built from a real filesystem works too. Devices, fifos and sockets
are left out, since jj has no values for them, and the metadata is ignored.

### The commit record: `src/convert.rs`

This follows core's `architecture/commits.md` table:

| jj `backend::Commit` | amber `Commit` |
| --- | --- |
| `parents` | Key 1. A commit whose only parent is the root commit has no parents. The root cannot be one parent of a merge. |
| `root_tree: Merge<TreeId>` `[A0, R0, A1, …]` | Key 0 is `A0`, and key 8 holds the rest. |
| `conflict_labels` | Key 9. It is absent when the tree is resolved or no term is labelled. |
| `change_id` | Key 7, 16 bytes. |
| `author`, `committer` | Keys 2 and 3. jj milliseconds become nanoseconds. |
| `description` | Key 4. |
| `secure_sig` | Key 5. jj's signer signs `Commit::signature_payload()`, the record without key 5, which is core's signing convention. |
| `predecessors` | Not stored. jj deprecates them and keeps them in its operation log. |

A commit that jj did not write (dstore's working copies write them) may have no change id, or one
of another length. jj sees 16 bytes derived from it, or from the commit key when there is none.
Every reader derives the same value. Nanoseconds that are not whole milliseconds round down.

## Not supported, or different from `jj git`

- Copy tracking: the copy methods report `Unsupported`, as jj's simple backend does.
- An empty symlink target cannot be stored. An amber directory entry's empty target is the absent
  one.
- Commit ids start with their type and length. An amber key begins with a type nibble and the
  length field, so ids look alike in their first hex digits (`5101…`), and jj's shortest unique
  prefixes are a few characters longer than with git.
- Push cost grows with history. dstore's push walks the commit's whole local closure and asks the
  cluster about every key, then uploads only what is missing. Fetch prunes subtrees that are
  already complete locally.
- `jj gc` leaves the local packstore alone. It is a cache of every object jj wrote. On the cluster,
  objects live as long as a reference reaches them.
- There are no tags, and nothing is colocated with git (jj's `git` feature is off).

## Building and testing

The Nix flake's dev shell has the toolchain (Rust 1.95, Go for the end-to-end test):

```sh
nix develop -c cargo build --release         # target/release/ajj
nix develop -c cargo test                    # backend round trips; sync against an in-memory dstore
nix develop -c bash tests/e2e.sh             # against a live Go dstore v0.1.11 node, with Go working copies
```

- `tests/backend.rs` checks that every object round-trips, that file keys match core's `ingest`,
  and that stored commits decode with core's strict decoder. It also checks the footprint length,
  signatures and derived change ids.
- `tests/sync.rs` runs the reference compare-and-swap, fetching history into a fresh store, and two
  jj repositories sharing a branch through fetch and push, including a conflicted bookmark, a
  deletion and abandoned commits. It uses dstore-testkit's fake cluster.
- `tests/e2e.sh` starts a one-node Go cluster and runs ajj alongside Go dstore's working copies.
  Set `DSTORE_GO_BIN` to skip `go install`.

## Dependencies

- jj-cli and jj-lib `=0.45.1`, without the git feature.
- core-rs `amber-store-core` v0.7.0 (core v0.0.10: the jj commit fields and the footprint length).
- dstore-client-rs v0.2.0 (dstore v0.1.11), at pinned revisions. The iroh 1.2.0 patch that
  dstore-client-rs applies for go-iroh interop is repeated in `[patch.crates-io]`, since a patch
  does not reach dependents.
- `.cargo/config.toml` fetches git dependencies with the git CLI.

## Licence

LGPL-3.0-only, like core-rs and dstore-client-rs. jj is Apache-2.0.
