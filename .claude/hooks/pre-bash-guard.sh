#!/usr/bin/env bash
# APPLIES TO: all repos.
#
# PreToolUse (Bash) hook: blocks destructive shell commands until a human confirms.
# Exit 2 = intentional block (Claude Code convention). Exit 0 = allow.
#
# This is the one hook in the catalogue that blocks rather than warns. Keep the pattern
# list tight — a guard that fires on safe commands gets disabled, and then it guards
# nothing.
#
# Register in .claude/settings.json under hooks.PreToolUse with matcher "Bash".

set -uo pipefail

# shellcheck source=_json.sh
source "$(dirname "${BASH_SOURCE[0]}")/_json.sh"

input=$(cat)
if ! cmd=$(json_field "$input" 'tool_input.command'); then
    json_no_parser_warning
    exit 0
fi
[[ -z "$cmd" ]] && exit 0

# Scan the first line only. Heredoc bodies, commit messages, and MR descriptions land on
# later lines and routinely contain phrases like "rm -rf" as prose — matching those would
# block writing about a dangerous command, which is not the point.
first_line=$(printf '%s' "$cmd" | head -1)

# Patterns that destroy work irrecoverably, or that rewrite shared history.
patterns=(
    "rm -rf"
    "rm -fr"
    "rm -r "
    "git reset --hard"
    "git clean -f"
    "git checkout -- "
    "git checkout ."
    "git restore ."
    "git push --force"
    "git push -f"
    "git rebase"
    "git filter-branch"
    "DROP TABLE"
    "DROP DATABASE"
    "TRUNCATE"
    "docker compose down -v"
    "docker volume rm"
    "docker system prune"
    "chmod -R 777"
)

# Patterns where the CAPITAL carries the meaning, so they are matched case-sensitively
# against the raw line. `git branch -D` force-deletes a branch whose work may exist
# nowhere else; `git branch -d` refuses exactly that case and is safe. Lowercasing both
# collapses them into one, which blocks routine tidying — and a guard that blocks safe
# work is a guard people switch off.
patterns_cs=(
    "git branch -D"
)

block() {
    {
        echo "BLOCKED: destructive command detected ('$1')."
        echo ""
        echo "Command: $cmd"
        echo ""
        echo "Explain to the user what this will do and why it is needed, and ask them to"
        echo "confirm or run it themselves. Do not work around this hook."
    } >&2
    exit 2
}

# Pure-bash case-insensitive substring matching. Deliberately not grep:
#   - this runs on every Bash tool call, and ~20 subprocesses per call is a real tax;
#   - `grep -qiF` segfaults on GNU grep 3.0 under Git Bash on Windows, which would turn
#     the guard into a no-op on exactly the platform most of the team uses.
haystack="${first_line,,}"

for pattern in "${patterns[@]}"; do
    [[ "$haystack" == *"${pattern,,}"* ]] && block "$pattern"
done

for pattern in "${patterns_cs[@]}"; do
    [[ "$first_line" == *"$pattern"* ]] && block "$pattern"
done

# Deleting a remote branch is written `git push origin --delete <branch>` — the remote
# sits in the middle, so no single substring catches it, and a literal "git push --delete"
# pattern never fires. Both halves have to be present.
#
# Per COMMAND SEGMENT, not per line: a line that pushes a branch and then runs something
# whose text happens to contain a -d flag is two separate things, and judging them
# together blocks the innocent one. (Found the hard way — the first version of this check
# was blocked by its own MR command, which pushed a branch and then passed "-d from -D"
# to glab.)
IFS=';' read -ra _segments <<< "${haystack//&&/;}"
for _seg in "${_segments[@]}"; do
    [[ "$_seg" == *"git push"* ]] || continue
    if [[ "$_seg" == *"--delete"* || "$_seg" == *" -d "* || "$_seg" == *" -d" ]]; then
        block "git push --delete"
    fi
done

exit 0
