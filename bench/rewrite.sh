#!/usr/bin/env bash
# Benchmark history rewrites on a mirror clone: gitreforge vs. BFG Repo-Cleaner
# vs. git filter-repo.
#
# Usage: bench/rewrite.sh [SCENARIO]
#   tokio-folder    remove tokio-util/ (top level only, ~2k of 24k commits)   [default]
#   tokio-heavy     remove tokio/ at any depth (~13.4k commits)
#   tokio-wildcard  remove top-level tokio-*/ (27 crates, ~6.1k commits)
#   git-docs        git/git: remove Documentation/ (top level only)
#   git-rename      git/git: rewrite all 7 "Linus Torvalds" identities (no BFG,
#                   it cannot rewrite authors/committers)
# Env:   BENCH_DIR  work dir for tools, mirrors and scratch copies (default: target/bench)
#        RUNS       hyperfine runs per tool (default: 5)
#        WARMUP     hyperfine warmup runs per tool (default: 1)
#        VERIFY     set to 0 to skip comparing the results of the tools (default: 1)
#
# All tools only rewrite history; none of them gc/repack (filter-repo runs with
# --no-gc), and empty commits are kept everywhere so the results are identical.
# The scenarios are picked so that all tools match the same paths, except that
# BFG cannot anchor a pattern to the root and also removes tokio/tokio-shim in
# tokio-wildcard (a single commit, only reachable from refs/pull/7616).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
SELF=$ROOT/bench/$(basename "$0")
BENCH_DIR=$(mkdir -p "${BENCH_DIR:-$ROOT/target/bench}" && cd "${BENCH_DIR:-$ROOT/target/bench}" && pwd)
RUNS=${RUNS:-5}
WARMUP=${WARMUP:-1}
VERIFY=${VERIFY:-1}

TOOLS=$BENCH_DIR/tools
WORK=$BENCH_DIR/work
GITREFORGE=$ROOT/target/release/gitreforge
FILTER_REPO_VERSION=v2.47.0
BFG_VERSION=1.15.0
HYPERFINE_VERSION=v1.19.0

LINUS_IDS=(torvalds@evo.osdl.org torvalds@g5.osdl.org torvalds@linux-foundation.org torvalds@osdl.org
    'torvalds@ppc970.osdl.org.(none)' torvalds@ppc970.osdl.org torvalds@woody.linux-foundation.org)
LINUS_NEW='Linus Renamed <linus@example.com>'

if [ "${1:-}" = --run ]; then
    MODE=run
    shift
fi
SCENARIO=${1:-tokio-folder}

case $SCENARIO in
tokio-folder)
    REPO_URL=https://github.com/tokio-rs/tokio.git
    DESC="remove tokio-util/"
    KIND=remove
    GITREFORGE_ARGS=(remove --directory /tokio-util)
    BFG_ARGS=(--delete-folders tokio-util)
    FILTER_REPO_ARGS=(--invert-paths --path tokio-util/)
    LEFTOVER_REGEX='(^|/)tokio-util/'
    ;;
tokio-heavy)
    REPO_URL=https://github.com/tokio-rs/tokio.git
    DESC="remove every folder named tokio/"
    KIND=remove
    GITREFORGE_ARGS=(remove --directory tokio)
    BFG_ARGS=(--delete-folders tokio)
    FILTER_REPO_ARGS=(--invert-paths --path-regex '(^|/)tokio/')
    LEFTOVER_REGEX='(^|/)tokio/'
    ;;
tokio-wildcard)
    REPO_URL=https://github.com/tokio-rs/tokio.git
    DESC="remove top-level tokio-*/ folders"
    KIND=remove
    GITREFORGE_ARGS=(remove --directory '/tokio-*')
    BFG_ARGS=(--delete-folders 'tokio-*')
    FILTER_REPO_ARGS=(--invert-paths --path-glob 'tokio-*')
    LEFTOVER_REGEX='^tokio-[^/]*/'
    ;;
git-docs)
    REPO_URL=https://github.com/git/git.git
    DESC="remove Documentation/"
    KIND=remove
    GITREFORGE_ARGS=(remove --directory /Documentation)
    BFG_ARGS=(--delete-folders Documentation)
    FILTER_REPO_ARGS=(--invert-paths --path Documentation/)
    LEFTOVER_REGEX='(^|/)Documentation/'
    ;;
git-rename)
    REPO_URL=https://github.com/git/git.git
    DESC="rename Linus Torvalds (7 identities) to $LINUS_NEW"
    KIND=rename
    for email in "${LINUS_IDS[@]}"; do
        echo "Linus Torvalds <$email> = $LINUS_NEW"
    done >"$BENCH_DIR/linus-gitreforge.txt"
    for email in "${LINUS_IDS[@]}"; do
        echo "$LINUS_NEW Linus Torvalds <$email>"
    done >"$BENCH_DIR/linus-mailmap.txt"
    GITREFORGE_ARGS=(contributor rewrite)
    GITREFORGE_STDIN=$BENCH_DIR/linus-gitreforge.txt
    FILTER_REPO_ARGS=(--mailmap "$BENCH_DIR/linus-mailmap.txt")
    ;;
*)
    echo "unknown scenario: $SCENARIO" >&2
    exit 1
    ;;
esac
MIRROR=$BENCH_DIR/$(basename "$REPO_URL" .git).git
TOOL_LIST=(gitreforge git-filter-repo)
[ -n "${BFG_ARGS+x}" ] && TOOL_LIST+=(bfg)

run_tool() {
    case $1 in
    gitreforge) cd "$WORK" && "$GITREFORGE" "$WORK" "${GITREFORGE_ARGS[@]}" <"${GITREFORGE_STDIN:-/dev/null}" ;;
    bfg) cd "$WORK" && "$TOOLS/jre/bin/java" -jar "$TOOLS/bfg.jar" --no-blob-protection "${BFG_ARGS[@]}" . ;;
    git-filter-repo) cd "$WORK" && "$TOOLS/git-filter-repo" --force --no-gc --prune-empty never --prune-degenerate never "${FILTER_REPO_ARGS[@]}" ;;
    esac
}

if [ "${MODE:-}" = run ]; then
    run_tool "$2" >/dev/null 2>&1
    exit
fi

fetch_tools() {
    mkdir -p "$TOOLS"
    cd "$TOOLS"
    if [ ! -x git-filter-repo ]; then
        curl -sSfL -o git-filter-repo \
            "https://raw.githubusercontent.com/newren/git-filter-repo/$FILTER_REPO_VERSION/git-filter-repo"
        chmod +x git-filter-repo
    fi
    if [ ! -f bfg.jar ]; then
        curl -sSfL -o bfg.jar \
            "https://repo1.maven.org/maven2/com/madgag/bfg/$BFG_VERSION/bfg-$BFG_VERSION.jar"
    fi
    if [ ! -x jre/bin/java ]; then
        mkdir -p jre
        curl -sSfL "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jre/hotspot/normal/eclipse" |
            tar xz -C jre --strip-components=1
    fi
    if [ ! -x hyperfine ]; then
        curl -sSfL "https://github.com/sharkdp/hyperfine/releases/download/$HYPERFINE_VERSION/hyperfine-$HYPERFINE_VERSION-x86_64-unknown-linux-musl.tar.gz" |
            tar xz --strip-components=1 "hyperfine-$HYPERFINE_VERSION-x86_64-unknown-linux-musl/hyperfine"
    fi
    cd - >/dev/null
}

fetch_tools
(cd "$ROOT" && cargo build --release --quiet)
[ -d "$MIRROR" ] || git clone --quiet --mirror "$REPO_URL" "$MIRROR"

echo "Repository: $REPO_URL ($(du -sh "$MIRROR" | cut -f1), $(git -C "$MIRROR" rev-list --all | wc -l) commits, $(git -C "$MIRROR" for-each-ref | wc -l) refs)"
echo "Scenario:   $SCENARIO ($DESC)"
echo

PREPARE="rm -rf '$WORK' && cp -a '$MIRROR' '$WORK' && sync"
HYPERFINE_CMDS=()
for tool in "${TOOL_LIST[@]}"; do
    HYPERFINE_CMDS+=(-n "$tool" "'$SELF' --run '$SCENARIO' '$tool'")
done
"$TOOLS/hyperfine" --runs "$RUNS" --warmup "$WARMUP" --prepare "$PREPARE" \
    --export-markdown "$BENCH_DIR/results-$SCENARIO.md" --export-json "$BENCH_DIR/results-$SCENARIO.json" \
    "${HYPERFINE_CMDS[@]}"

[ "$VERIFY" = 0 ] && exit

# Verify all tools produce the same result: run each once more and record the
# tree (remove) or author/committer identities (rename) of the rewritten
# history. Commit ids differ because BFG/filter-repo also rewrite old hashes
# mentioned in commit messages.
echo
echo "Verifying results..."
snapshot() {
    if [ $KIND = remove ]; then
        # filter-repo turns refs/remotes/origin/* into refs/heads/*, so compare on that basis
        git -C "$WORK" for-each-ref --format='%(refname) %(objectname)' refs/heads refs/tags refs/remotes/origin |
            sed 's#^refs/remotes/origin/#refs/heads/#' |
            while read -r ref obj; do
                echo "$ref $(git -C "$WORK" rev-parse --verify --quiet "$obj^{tree}" || echo none)"
            done | LC_ALL=C sort
    else
        git -C "$WORK" log --all --format='%an <%ae> | %cn <%ce>' | LC_ALL=C sort
    fi
}
leftover() {
    if [ $KIND = remove ]; then
        echo "$(git -C "$WORK" rev-list --all --objects | cut -d' ' -f2- | grep -cE "$LEFTOVER_REGEX" || true) reachable paths matching /$LEFTOVER_REGEX/"
    else
        echo "$(grep -c '^Linus Torvalds\|| Linus Torvalds' "$1" || true) commits still by Linus Torvalds, $(grep -c 'Linus Renamed' "$1" || true) by Linus Renamed"
    fi
}
for tool in "${TOOL_LIST[@]}"; do
    eval "$PREPARE"
    "$SELF" --run "$SCENARIO" "$tool"
    out=$BENCH_DIR/verify-$SCENARIO-$tool.txt
    snapshot >"$out"
    echo "$tool: $(git -C "$WORK" rev-list --all | wc -l) commits, $(leftover "$out"), digest $(sha1sum <"$out" | cut -c1-12)"
done
rm -rf "$WORK"
