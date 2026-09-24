# Amber forge frontend

A responsive, dependency-free design prototype for a forge based on ajj.

Run with Node.js 22 or later:

```sh
cd web
npm start
# Open http://localhost:3000
```

Use `PORT=3001 npm start` to choose another port. The preview binds to localhost.

The interface includes repository browsing, file search, source viewing, change
reviews with status filters and illustrative diffs, bookmark navigation, activity,
and a copyable ajj clone command. Hash routes support direct links and browser
back/forward. The clone dialog supports keyboard focus and Escape dismissal.

The code browser contains a public source snapshot of this repository. Refresh it
with `node snapshot.mjs` from this directory. Review history, bookmark positions,
authors, and diffs are explicitly sample data. Selecting a sample feature bookmark
opens its review; it does not claim to load a real revision's file tree.

## Design

Warm neutral surfaces, amber change IDs, and olive actions echo amber-store without
making the code compete for attention. A workspace rail frames a repository header;
code and review content occupy the primary column, with repository context alongside.
At narrow widths the rail and secondary context give way to the primary workflows.

ajj's stable change IDs are the review identity. Bookmarks are named pointers mapped
to dstore references, and divergence is represented as a conflict. There are no Git
pull-request, tag, or colocation assumptions.

## Integration boundary

This is a frontend prototype, not a hosted forge service. No authentication,
remote cluster connection, review persistence, or repository mutation is implemented.
A production adapter should expose repository trees and blobs at commit keys,
changes keyed by stable change ID, bookmark local/remote/conflict state, and paginated
activity. Review discussion and approval records need their own persistent model.
Cluster tickets must stay server-side; a browser must never receive stored tickets.
Bookmark updates must preserve the existing compare-and-swap semantics in ajj.

Validation: `npm run check` checks JavaScript syntax. The UI can be served by any
static HTTP server; there are no build steps, external fonts, or CDN dependencies.
