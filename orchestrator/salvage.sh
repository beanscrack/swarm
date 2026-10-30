#!/usr/bin/env bash
# Salvage a budget-killed worker's worktree and turn it into a decision, not a judgment call.
# Rule codified from this session: gates green on the RAW worker tree -> ship it; a small number
# of red tests -> dispatch a narrow repair brief whose brief text IS the red output; more than
# that -> finish by hand, because repair briefs re-orient expensively and drift.
#
# Usage: salvage.sh <worktree> <gate command> [gate command ...]
#   gate commands run with cwd = worktree. Quote them.
# Exit: 0 = SHIP, 1 = HAND, 2 = REPAIR (count printed), 3 = worktree has no diff.
set -euo pipefail
wt="${1:?usage: salvage.sh <worktree> <gate...>}"
shift
[ $# -ge 1 ] || { echo "give at least one gate command" >&2; exit 2; }
cd "$wt"

echo "== worktree $wt =="
git status --porcelain=v1 | sed 's/^/  /' | head -40
changed=$(git status --porcelain=v1 | grep -c . || true)
# Committed work counts as a diff. A salvage target is often a worktree whose commits were
# already pushed (upstream == HEAD), so compare against the base branch, not @{u}.
base="${SALVAGE_BASE:-origin/main}"
ahead=$(git rev-list --count "$base..HEAD" 2>/dev/null || echo 0)
if [ "$ahead" != 0 ]; then
  echo "== commits ahead of $base: $ahead =="
  git log --oneline "$base..HEAD" | sed 's/^/  /' | head -20
  git diff --stat "$base...HEAD" | tail -5 | sed 's/^/  /'
fi
if [ "$changed" = 0 ] && [ "$ahead" = 0 ]; then
  echo "NO DIFF  worker left nothing to salvage (nothing dirty, no commits ahead of $base)"; exit 3
fi

# Binary/junk artifacts workers routinely leave behind. Reported, not deleted (deletion is the
# caller's call; several of these were build outputs the next gate command still needs).
junk=$(git status --porcelain=v1 | awk '{print $2}' | grep -Ei '\.(exe|dll|obj|bin|diff|pyc)$|^(tmp_|zz_probe)' || true)
[ -n "$junk" ] && { echo "== junk artifacts (remove before commit) =="; echo "$junk" | sed 's/^/  /'; }

declare -a gate_status=()
red_tests=""
rc_all=0
for gate in "$@"; do
  echo "== GATE: $gate"
  out=$(bash -c "$gate" 2>&1) && rc=0 || rc=$?
  printf '%s\n' "$out" | tail -25
  gate_status+=("$rc :: $gate")
  rc_all=$(( rc_all + rc ))
  # Go / pytest / cargo red-test vocabulary, used to size the repair.
  found=$(printf '%s\n' "$out" | grep -Eo '^(--- FAIL: |FAILED |failures:)[A-Za-z0-9_:./ -]+' \
          | sed -E 's/^--- FAIL: //; s/^FAILED //' | sort -u || true)
  [ -n "$found" ] && red_tests="${red_tests}${found}"$'\n'
done

nred=$(printf '%s' "$red_tests" | grep -c . || true)
echo "== gate results =="
printf '%s\n' "${gate_status[@]}" | sed 's/^/  /'
echo "red tests: $nred"

if [ "$rc_all" = 0 ]; then
  echo "VERDICT SHIP  raw worker tree passes every gate - open the PR, do not re-implement"
  exit 0
fi
if [ "$nred" -gt 0 ] && [ "$nred" -le 2 ]; then
  echo "VERDICT REPAIR $nred  dispatch a narrow repair brief; paste these test names + their output into the brief"
  printf '%s\n' "$red_tests" | sed 's/^/  RED /'
  exit 2
fi
echo "VERDICT HAND  $nred red tests / non-test gate failures - finish by hand (repair briefs re-orient expensively)"
exit 1
