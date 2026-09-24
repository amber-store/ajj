//! ajj: jj (Jujutsu) whose commits are amber-store objects, with bookmarks shared through dstore.
//!
//! - [`backend`]: the jj `Backend` over a local core-rs packstore; every jj id is an amber key.
//! - [`convert`]: jj's commit record to amber's Commit object and back.
//! - [`dstore`]: remotes, dialing a cluster, listing branches, moving closures and references.
//! - [`bookmarks`]: remote-tracking bookmarks updated by fetch and push, as `jj git` keeps them.
//! - [`progress`]: the one-line transfer display.
//! - [`cli`]: the `ajj` binary.

pub mod backend;
pub mod bookmarks;
pub mod cli;
pub mod convert;
pub mod dstore;
pub mod progress;
