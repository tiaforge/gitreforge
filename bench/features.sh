#!/usr/bin/env bash
# Benchmark the gitreforge features that bench/rewrite.sh does not cover, on
# the git/git mirror. Run bench/rewrite.sh first: it downloads the tools and
# the mirror into BENCH_DIR.
#
# Usage: bench/features.sh [SCENARIO...]
#   list      contributor list vs. git log | sort -u (read only)
#   dry-run   remove Documentation/ with --dry-run
#   prune     prune-empty after removing Documentation/, vs. git filter-repo
#             --prune-empty always
# Env:   BENCH_DIR, RUNS, WARMUP as in bench/rewrite.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BENCH_DIR=$(cd "${BENCH_DIR:-$ROOT/target/bench}" && pwd)
RUNS=${RUNS:-5}
WARMUP=${WARMUP:-1}
TOOLS=$BENCH_DIR/tools
WORK=$BENCH_DIR/work
MIRROR=$BENCH_DIR/git.git
NODOCS=$BENCH_DIR/git-nodocs.git
GITREFORGE=$ROOT/target/release/gitreforge

[ -x "$TOOLS/hyperfine" ] && [ -d "$MIRROR" ] || {
    echo "run bench/rewrite.sh git-docs first" >&2
    exit 1
}
(cd "$ROOT" && cargo build --release --quiet)
SCENARIOS=("$@")
[ ${#SCENARIOS[@]} -gt 0 ] || SCENARIOS=(list dry-run prune)

hf() {
    local name=$1
    shift
    "$TOOLS/hyperfine" --runs "$RUNS" --warmup "$WARMUP" \
        --export-markdown "$BENCH_DIR/results-feature-$name.md" "$@"
}
fresh() { echo "rm -rf '$WORK' && cp -a '$1' '$WORK' && sync"; }

for scenario in "${SCENARIOS[@]}"; do
    echo "== $scenario"
    case $scenario in
    list)
        hf list \
            -n gitreforge "'$GITREFORGE' '$MIRROR' contributor list >/dev/null" \
            -n 'git log' "git -C '$MIRROR' log --all --format='%an <%ae>%n%cn <%ce>' | LC_ALL=C sort -u >/dev/null"
        ;;
    dry-run)
        hf dry-run --prepare "$(fresh "$MIRROR")" \
            -n 'gitreforge --dry-run' "cd '$WORK' && '$GITREFORGE' --dry-run . remove --directory /Documentation" \
            -n gitreforge "cd '$WORK' && '$GITREFORGE' . remove --directory /Documentation"
        ;;
    prune)
        if [ ! -d "$NODOCS" ]; then
            cp -a "$MIRROR" "$NODOCS"
            (cd "$NODOCS" && "$GITREFORGE" . remove --directory /Documentation >/dev/null)
        fi
        hf prune --prepare "$(fresh "$NODOCS")" \
            -n gitreforge "cd '$WORK' && '$GITREFORGE' . prune-empty" \
            -n git-filter-repo "cd '$WORK' && '$TOOLS/git-filter-repo' --force --no-gc --prune-empty always >/dev/null 2>&1"
        for tool in gitreforge git-filter-repo; do
            eval "$(fresh "$NODOCS")"
            if [ $tool = gitreforge ]; then
                (cd "$WORK" && "$GITREFORGE" . prune-empty >/dev/null)
            else
                (cd "$WORK" && "$TOOLS/git-filter-repo" --force --no-gc --prune-empty always >/dev/null 2>&1)
            fi
            echo "$tool: $(git -C "$NODOCS" rev-list --all | wc -l) -> $(git -C "$WORK" rev-list --all | wc -l) commits"
        done
        ;;
    *)
        echo "unknown scenario: $scenario" >&2
        exit 1
        ;;
    esac
done
rm -rf "$WORK"
