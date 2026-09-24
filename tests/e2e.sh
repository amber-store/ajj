#!/usr/bin/env bash
# End to end against a live Go dstore node: ajj clones, fetches and pushes, and Go dstore's working
# copies read and extend the same branches.
#
#   nix develop -c bash tests/e2e.sh
#
# Inputs: AJJ (default: target/debug/ajj, built if missing); DSTORE_GO_BIN, else a Go dstore v0.1.11
# built with `go install` into the temporary directory. Everything lives in one mktemp directory that
# the exit trap removes, with the node process.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
W=$(mktemp -d "${TMPDIR:-/tmp}/ajj-e2e.XXXXXX")
NODE_PID=
cleanup() {
	[ -n "$NODE_PID" ] && kill "$NODE_PID" 2>/dev/null && wait "$NODE_PID" 2>/dev/null
	rm -rf "$W"
}
trap cleanup EXIT

AJJ=${AJJ:-$REPO/target/debug/ajj}
[ -x "$AJJ" ] || (cd "$REPO" && cargo build --quiet)
if [ -z "${DSTORE_GO_BIN:-}" ]; then
	GOBIN=$W/bin CGO_ENABLED=0 go install github.com/amber-store/dstore/cmd/dstore@v0.1.11
	DSTORE_GO_BIN=$W/bin/dstore
fi
G=$DSTORE_GO_BIN

# jj leaves new files over 1 MiB untracked by default; big.bin is 3 MB.
printf 'snapshot.max-new-file-size = "10MiB"\n' >"$W/jj.toml"
export JJ_CONFIG=$W/jj.toml DSTORE_LOG_LEVEL=warn QUIC_GO_DISABLE_RECEIVE_BUFFER_WARNING=1
FAILS=0
pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { # check NAME COMMAND...
	local name=$1; shift
	if "$@" >"$W/last.out" 2>&1; then pass "$name"; else fail "$name"; sed 's/^/    /' "$W/last.out"; fi
}
as() { # as USER: sets the jj identity
	export JJ_USER=$1 JJ_EMAIL=$(echo "$1" | tr '[:upper:]' '[:lower:]')@example.com
}
contains() { grep -qF -- "$2" <<<"$1"; }

# A one-node cluster on loopback.
cd "$W"
"$G" cluster init --store n1 --replicas 1 --min-replicas 1 --allow-unsafe --weight 100 --no-relay \
	--loopback >init.out 2>&1
T=$(awk '/^cluster ticket:/ { print $3 }' init.out)
[ -n "$T" ] || { cat init.out; exit 2; }
"$G" serve --store n1 --no-relay --loopback --gc-interval 1m >n1.log 2>&1 &
NODE_PID=$!
for _ in $(seq 60); do
	DSTORE_TICKET=$T "$G" cluster status --no-relay >/dev/null 2>&1 && break
	sleep 1
done
export DSTORE_TICKET=$T

# 1. Ann creates a repo, commits files of every kind, and pushes main.
as Ann
"$AJJ" dstore init ann >/dev/null 2>&1
cd "$W/ann"
echo hello >a.txt
mkdir sub && printf '#!/bin/sh\n' >sub/x.sh && chmod +x sub/x.sh
ln -s a.txt link
head -c 3000000 /dev/urandom >big.bin
"$AJJ" commit -m first >/dev/null 2>&1
# Ann's remote stores no ticket: fetch and push take $DSTORE_TICKET when they run.
env -u DSTORE_TICKET "$AJJ" dstore remote add origin --prefix demo/ --no-relay
if contains "$("$AJJ" dstore remote list)" 'origin $DSTORE_TICKET prefix=demo/'; then
	pass "remote without a stored ticket"
else fail "remote without a stored ticket"; fi
if OUT=$(env -u DSTORE_TICKET "$AJJ" dstore fetch 2>&1); then fail "no ticket anywhere is an error"; else
	if contains "$OUT" "no ticket"; then pass "no ticket anywhere is an error"; else fail "no ticket anywhere: $OUT"; fi
fi
check "--ticket for one run" env -u DSTORE_TICKET "$AJJ" dstore fetch --ticket "$T"
"$AJJ" bookmark create main -r @- >/dev/null 2>&1
check "push a new bookmark" "$AJJ" dstore push -b main
MAIN=$("$AJJ" log --no-graph -r main -T commit_id 2>/dev/null)
REF=$("$G" ref get --no-relay demo/main 2>/dev/null || true)
if contains "$REF" "$MAIN"; then pass "demo/main names the jj commit"; else fail "demo/main names the jj commit: $REF"; fi

# 2. Bob clones with ajj: same files, same commit, same change id.
as Bob
cd "$W"
check "clone with the ticket from \$DSTORE_TICKET" "$AJJ" dstore clone bob --prefix demo/ --no-relay
if cmp -s ann/big.bin bob/big.bin && [ "$(readlink bob/link)" = a.txt ] && [ -x bob/sub/x.sh ]; then
	pass "cloned files, symlink, exec bit"
else fail "cloned files, symlink, exec bit"; fi
CH_ANN=$(cd ann && "$AJJ" log --no-graph -r main -T change_id 2>/dev/null)
CH_BOB=$(cd bob && "$AJJ" log --no-graph -r main -T change_id 2>/dev/null)
if [ "$CH_ANN" = "$CH_BOB" ]; then pass "change id survives"; else fail "change id: $CH_ANN vs $CH_BOB"; fi

# 3. Go dstore's working copy clones the branch and pushes a commit on it.
check "go clone of the jj branch" "$G" clone --no-relay demo/main wc
if cmp -s ann/big.bin wc/big.bin; then pass "go working copy has the files"; else fail "go working copy has the files"; fi
echo "from go" >wc/go.txt
check "go push on the branch" bash -c "cd wc && '$G' push --no-relay -m 'go commit'"

# Bob's clone stored the ticket; fetch needs no environment.
check "stored ticket from clone" bash -c "cd '$W/bob' && env -u DSTORE_TICKET '$AJJ' dstore fetch"

# 4. Bob fetches Go's commit, commits on top, pushes by default (main is tracked); Go pulls it.
cd "$W/bob"
check "fetch a Go commit" "$AJJ" dstore fetch
if [ "$("$AJJ" log --no-graph -r main -T description 2>/dev/null)" = "go commit" ]; then
	pass "Go commit reads as a jj commit"
else fail "Go commit reads as a jj commit"; fi
"$AJJ" new main >/dev/null 2>&1
echo bob >bob.txt
"$AJJ" commit -m "bob on go" >/dev/null 2>&1
"$AJJ" bookmark move main --to @- >/dev/null 2>&1
check "push a tracked bookmark by default" "$AJJ" dstore push
check "go pulls the jj commit" bash -c "cd '$W/wc' && '$G' pull --no-relay && test \"\$(cat bob.txt)\" = bob"

# 5. Ann's stale push is refused; fetch conflicts her bookmark as jj git fetch does.
as Ann
cd "$W/ann"
"$AJJ" new main >/dev/null 2>&1
echo ann >ann.txt
"$AJJ" commit -m "ann work" >/dev/null 2>&1
"$AJJ" bookmark move main --to @- >/dev/null 2>&1
if OUT=$("$AJJ" dstore push 2>&1); then fail "stale push refused"; else
	if contains "$OUT" "changed on the remote"; then pass "stale push refused"; else fail "stale push refused: $OUT"; fi
fi
"$AJJ" dstore fetch >/dev/null 2>&1
if contains "$("$AJJ" bookmark list 2>&1)" "main (conflicted)"; then pass "fetch conflicts the bookmark"; else fail "fetch conflicts the bookmark"; fi
"$AJJ" rebase -s 'description(substring:"ann work")' -d main@origin >/dev/null 2>&1
"$AJJ" bookmark set main -r 'description(substring:"ann work")' >/dev/null 2>&1
check "push after rebase" "$AJJ" dstore push

# 6. A conflicted commit travels with its terms and labels.
echo "ann line" >a.txt
"$AJJ" commit -m "ann edits a" >/dev/null 2>&1
"$AJJ" bookmark set feature -r @- >/dev/null 2>&1
"$AJJ" dstore push -b feature >/dev/null 2>&1
as Bob
cd "$W/bob"
"$AJJ" dstore fetch >/dev/null 2>&1
"$AJJ" new 'description(substring:"bob on go")' >/dev/null 2>&1
echo "bob line" >a.txt
"$AJJ" commit -m "bob edits a" >/dev/null 2>&1
"$AJJ" rebase -r 'description(substring:"bob edits a")' -d feature@origin >/dev/null 2>&1
"$AJJ" bookmark create conflicted -r 'description(substring:"bob edits a")' >/dev/null 2>&1
check "push a conflicted commit" "$AJJ" dstore push -b conflicted
as Ann
cd "$W/ann"
"$AJJ" dstore fetch >/dev/null 2>&1
SHOWN=$("$AJJ" file show -r conflicted@origin a.txt 2>&1)
if contains "$SHOWN" "<<<<<<< conflict 1 of 1" && contains "$SHOWN" "rebase destination"; then
	pass "conflict and labels fetched"
else fail "conflict and labels fetched: $SHOWN"; fi

# 7. Deleting a bookmark and pushing the deletion removes the reference.
"$AJJ" bookmark delete conflicted >/dev/null 2>&1
check "push a deletion" "$AJJ" dstore push --deleted
if "$G" ref get --no-relay demo/conflicted >/dev/null 2>&1; then fail "reference deleted"; else pass "reference deleted"; fi

# 8. The node's own check: every reference is complete (gc why walks the closure).
check "gc why on the branch" "$G" gc why --no-relay "$("$AJJ" log --no-graph -r main@origin -T commit_id 2>/dev/null)"

echo
if [ "$FAILS" -eq 0 ]; then echo "all checks passed"; else echo "$FAILS checks failed"; exit 1; fi
