#!/usr/bin/env bash
# Qualify a PR in the lane that actually gates it.
# Learned from a PR GitHub gave ZERO pull_request runs for (not skipped: absent), while
# gh pr checks kept reporting "no checks reported" even after manual dispatch, because
# workflow_dispatch runs attach check-runs to the HEAD COMMIT, not to the PR check-suite.
#
# Usage: pr-qualify.sh <owner/repo> <pr> [workflow.yml ...]
#   workflows = names to dispatch when the head has no check-runs (default: ci.yml).
# Env: QUALIFY_TIMEOUT (default 3600s), QUALIFY_POLL (default 45s).
# Exit: 0 all check-runs green, 1 any failure, 3 timeout with legs pending, 4 API error.
#
# Lesson baked in: an earlier version swallowed `gh api` stderr, so one bad flag
# ("--per-page", which gh api does not have) read as "no check-runs exist" and dispatched a
# workflow for no reason. API errors are fatal here, never silence.
set -uo pipefail
repo="${1:?usage: pr-qualify.sh <owner/repo> <pr> [workflow.yml ...]}"
pr="${2:?missing PR number}"
shift 2
workflows=("$@")
[ ${#workflows[@]} -eq 0 ] && workflows=(ci.yml)

head=$(gh pr view "$pr" --repo "$repo" --json headRefOid,headRefName --jq '"\(.headRefOid)\t\(.headRefName)"') || exit 4
sha=$(printf '%s' "$head" | cut -f1)
ref=$(printf '%s' "$head" | cut -f2)
[ -n "$sha" ] || { echo "no head sha for PR #$pr" >&2; exit 2; }
echo "PR #$pr head ${sha:0:10} ref $ref"

# Echoes "name<TAB>status<TAB>conclusion"; returns non-zero on API trouble (never silent).
runs_on_head() {
  local err rc
  err=$(mktemp)
  gh api "repos/$repo/commits/$sha/check-runs" --paginate \
    --jq '.check_runs[] | "\(.name)\t\(.status)\t\(.conclusion // "-")"' 2>"$err"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "API ERROR (rc=$rc): $(head -3 "$err" | tr '\n' ' ')" >&2
    rm -f "$err"; return 1
  fi
  rm -f "$err"; return 0
}

table=""
collect() {
  local out
  out=$(runs_on_head) || { echo "aborting: check-runs API is not answering" >&2; exit 4; }
  table=$(printf '%s' "$out" | sort)
}

# Grace period for the normal pull_request event to show up.
for _ in 1 2 3 4 5 6; do
  collect
  n=$(printf '%s\n' "$table" | grep -c . || true)
  [ "$n" -gt 0 ] && break
  sleep 20
done

if [ "$n" = 0 ]; then
  echo "no check-runs on head after grace -> GitHub created no PR runs; dispatching ${workflows[*]}"
  for w in "${workflows[@]}"; do
    if out=$(gh workflow run "$w" --repo "$repo" --ref "$ref" 2>&1); then
      echo "  dispatched $w"
    else
      echo "  dispatch $w FAILED: $(printf '%s' "$out" | head -1)  (no workflow_dispatch trigger?)"
    fi
  done
fi

deadline=$(( $(date +%s) + ${QUALIFY_TIMEOUT:-3600} ))
while :; do
  collect
  total=$(printf '%s\n' "$table" | grep -c . || true)
  pending=$(printf '%s\n' "$table" | awk -F'\t' '$2!="completed"' | grep -c . || true)
  failed=$(printf '%s\n' "$table" | awk -F'\t' '$3=="failure" || $3=="cancelled" || $3=="timed_out"' | grep -c . || true)
  echo "[$(date +%H:%M:%S)] runs=$total pending=$pending failed=$failed"
  [ "$total" = 0 ] && { echo "still no check-runs on head"; }
  if [ "$total" -gt 0 ] && [ "$pending" = 0 ]; then
    printf '%s\n' "$table"
    [ "$failed" = 0 ] && { echo "QUALIFIED  all $total check-runs green on ${sha:0:10}"; exit 0; }
    printf '%s\n' "$table" | awk -F'\t' '$3!="success" {print "RED  " $0}'
    exit 1
  fi
  [ "$(date +%s)" -ge "$deadline" ] && { printf '%s\n' "$table"; echo "TIMEOUT with $pending pending"; exit 3; }
  sleep ${QUALIFY_POLL:-45}
done
