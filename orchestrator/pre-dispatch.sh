#!/usr/bin/env bash
# Pre-dispatch dedupe: refuse to spend worker budget on work an open PR already covers.
# Learned from a 90-call worker dispatched at an issue whose fix was already a green open PR.
#
# Usage: pre-dispatch.sh <owner/repo> <issue> --gate <command> --workspace <path> [path ...]
# Reviewer/exploration: --preset reviewer|general (no red gate required).
#   paths = files the brief will touch; overlap with an open PR is reported as a stop.
# Exit: 0 = clear to dispatch, 10 = STOP (overlap found), 2 = usage/API problem.
set -euo pipefail

repo="${1:?usage: pre-dispatch.sh <owner/repo> <issue> [path ...]}"
issue="${2:?missing issue number}"
shift 2
preset=builder
gate=""
workspace=.
paths=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --gate) gate="${2:?missing gate command}"; shift 2 ;;
    --workspace) workspace="${2:?missing workspace}"; shift 2 ;;
    --preset) preset="${2:?missing preset}"; shift 2 ;;
    *) paths+=("$1"); shift ;;
  esac
done
case "$preset" in
  builder|test-writer)
    [ -n "$gate" ] || { echo "STOP: builder/test-writer dispatch requires --gate <failing command>" >&2; exit 11; }
    set +e
    (cd "$workspace" && bash -c "$gate")
    gate_status=$?
    set -e
    if [ "$gate_status" = 0 ]; then
      echo "STOP: baseline gate is green; there is no demonstrated failing check" >&2; exit 11
    fi
    if [ "$gate_status" = 126 ] || [ "$gate_status" = 127 ]; then
      echo "STOP: gate could not execute; this is not evidence of a failing check" >&2; exit 2
    fi
    note_gate="red baseline gate (exit $gate_status)"
    ;;
  reviewer|general) note_gate="read-only/exploration policy" ;;
  *) echo "unknown preset: $preset" >&2; exit 2 ;;
esac
printf 'POLICY  %s\n' "$note_gate"

command -v gh >/dev/null || { echo "gh required" >&2; exit 2; }

state=0
note() { echo "$@"; }

# 1) PRs that GitHub links as closing this issue.
mapfile -t linked < <(gh api "repos/$repo/issues/$issue/timeline" --paginate \
  --jq '.[] | select(.event=="cross-referenced" and (.source.issue.pull_request != null))
        | "\(.source.issue.number)\t\(.source.issue.state)\t\(.source.issue.title)"' 2>/dev/null || true)
closing=$(gh pr list --repo "$repo" --state all --search "resolves #$issue in:title,body" \
  --json number,state,title --jq '.[] | "\(.number)\t\(.state)\t\(.title)"' 2>/dev/null || true)

# 2) Open PRs whose title/branch/body mention the issue.
mention=$(gh pr list --repo "$repo" --state open --limit 100 --json number,title,headRefName \
  --jq --arg n "$issue" '.[] | select((.title | test("#" + $n)) or (.headRefName | test($n)))
        | "\(.number)\topen\t\(.title) [\(.headRefName)]"' 2>/dev/null || true)

for line in "${linked[@]:-}" $closing $mention; do
  [ -n "$line" ] || continue
  num=$(printf '%s' "$line" | cut -f1)
  st=$(printf '%s' "$line" | cut -f2)
  title=$(printf '%s' "$line" | cut -f3-)
  [ -z "$num" ] && continue
  # Open candidates are the dangerous ones: the work may already be reviewed and green.
  if [ "$st" = "open" ] || [ "$st" = "OPEN" ]; then
    checks=$(gh pr checks "$num" --repo "$repo" 2>/dev/null \
      | awk -F'\t' '{c[$2]++} END {for (k in c) printf "%s=%d ", k, c[k]}' || echo "checks=n/a")
    note "OVERLAP  PR #$num  $checks  $title"
    state=10
  fi
done

# 3) File overlap: does any open PR already touch the paths this brief will touch?
if [ ${#paths[@]} -gt 0 ]; then
  while IFS=$'\t' read -r num head title; do
    [ -n "$num" ] || continue
    touched=$(gh pr view "$num" --repo "$repo" --json files --jq '.files[].path' 2>/dev/null || true)
    for p in "${paths[@]}"; do
      if printf '%s\n' "$touched" | grep -qxF "$p"; then
        note "FILE-OVERLAP  PR #$num ($head) already touches $p  -- $title"
        state=10
      fi
    done
  done < <(gh pr list --repo "$repo" --state open --limit 100 --json number,headRefName,title \
             --jq '.[] | "\(.number)\t\(.headRefName)\t\(.title)"' 2>/dev/null || true)
fi

# 4) Issue body check: an acceptance box with no linked implementation is the green light.
labels=$(gh issue view "$issue" --repo "$repo" --json labels,state --jq '"state=\(.state) labels=\([.labels[].name] | join(","))"' 2>/dev/null || echo "state=?")
note "issue #$issue: $labels"
if [ "$state" = 0 ]; then
  note "CLEAR  no open PR covers issue #$issue"
else
  note "STOP  dispatching a worker here risks duplicating reviewed work"
fi
exit "$state"
