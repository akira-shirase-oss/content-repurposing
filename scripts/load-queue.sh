#!/usr/bin/env bash
# Runs the job-queue scenario (docs/END_GOAL.md, M1's half: ingest -> analyse) and keeps the
# evidence: sources through fetch, extract and the recorded model while worker generations
# are killed at seeded points, then checked against a kill-free reference digest.
#
#   MODE=smoke scripts/load-queue.sh   100,000 sources (the default)
#   MODE=full  scripts/load-queue.sh   1,000,000 sources: CI or a quiet machine
#
# KILLS (default 10) and SEED fix the kill points. Thresholds are written in the harness
# (crates/repurpose-sources/examples/queue-load.rs) and were fixed before the first run.
# A miss fails the run and is kept, never re-run until green. Writes the machine (never
# its name), the commit and whether the tree was dirty, the load average before and after,
# and the result to perf/results (full) or target/perf (smoke).
#
# Needs REPURPOSE_TEST_DATABASE_URL (scripts/stack.sh env) and jq.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
[ $# -eq 0 ] || { echo "load-queue.sh: takes no arguments (see --help)" >&2; exit 2; }
cd "$(dirname "$0")/.."
: "${REPURPOSE_TEST_DATABASE_URL:?is not set: scripts/stack.sh up; eval \"\$(scripts/stack.sh env)\"}"
mode="${MODE:-smoke}"
case "$mode" in
  smoke) total=100000; outdir=target/perf ;;
  full) total=1000000; outdir=perf/results ;;
  *) echo "load-queue.sh: MODE is smoke or full" >&2; exit 2 ;;
esac

cargo build --locked --release -q -p repurpose-sources --features test-support --example queue-load
load_before="$(uptime | sed 's/.*load averages*: *//')"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
set +e
result="$(TOTAL="$total" KILLS="${KILLS:-10}" SEED="${SEED:-20261120}" target/release/examples/queue-load)"
status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: *//')"
cpu="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"; cores="$(nproc)"
mem_gb="$(awk '/MemTotal/ {printf "%.1f", $2/1048576}' /proc/meminfo)"
os="$(. /etc/os-release && echo "$PRETTY_NAME")"
commit="$(git rev-parse HEAD)"
dirty=false; git diff --quiet HEAD -- . ':!perf/results' || dirty=true
mkdir -p "$outdir"
out="$outdir/job-queue-$mode-$stamp.json"
[ -n "$result" ] || result='{"pass":false,"error":"the harness printed nothing"}'
jq -n --arg mode "$mode" --arg stamp "$stamp" --arg commit "$commit" --argjson dirty "$dirty" \
  --arg cpu "$cpu" --arg cores "$cores" --arg mem_gb "$mem_gb" --arg os "$os" \
  --arg load_before "$load_before" --arg load_after "$load_after" --argjson result "$result" \
  '{ scenario: "job-queue", mode: $mode, started_utc: $stamp, commit: $commit, tree_dirty: $dirty,
     machine: { cpu: $cpu, cores: ($cores|tonumber), memory_gb: ($mem_gb|tonumber), os: $os,
                load_average_before: $load_before, load_average_after: $load_after },
     result: $result }' >"$out"
jq -c '{mode, commit, pass: .result.pass, total: .result.total, kills: .result.kills, lost: .result.lost, failed: .result.failed, jobs_per_second: .result.jobs_per_second, unsettled_calls: .result.unsettled_calls}' "$out"
echo "written: $out"
exit "$status"
