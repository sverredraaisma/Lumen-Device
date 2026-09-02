#!/usr/bin/env bash
# APPLIES TO: any repo in this project with a Cargo workspace.
#
# PreToolUse (Bash) hook: rewrites verbose cargo commands so only the interesting
# lines reach the context window. A green `cargo test` on a workspace prints a
# block per crate to say nothing happened; a coverage run prints a table per
# file. The failures are a few dozen lines.
#
# Advisory: on anything unexpected it emits {} and the original command runs
# unchanged.
#
# Register in .claude/settings.json under hooks.PreToolUse with matcher "Bash".

set -uo pipefail

# shellcheck source=_json.sh
source "$(dirname "${BASH_SOURCE[0]}")/_json.sh"

input=$(cat)
if ! cmd=$(json_field "$input" 'tool_input.command'); then
    json_no_parser_warning
    echo '{}'
    exit 0
fi
[[ -z "$cmd" ]] && { echo '{}'; exit 0; }

# Only rewrite a single, simple invocation. A command already piped, redirected or
# chained is one the caller shaped deliberately; rewriting it changes its meaning.
case "$cmd" in
    *"|"*|*">"*|*"&&"*|*";"*) echo '{}'; exit 0 ;;
esac

# Already asking for narrower or noisier output? Leave it alone.
case "$cmd" in
    *--verbose*|*" -v"*|*--nocapture*|*--json*|*--message-format*) echo '{}'; exit 0 ;;
esac

emit() {
    # Claude Code reads the rewritten command from updatedInput.command. It contains
    # quotes and backslashes, so it must be escaped by a real JSON encoder.
    if command -v jq >/dev/null 2>&1; then
        jq -nc --arg c "$1" '{
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "allow",
                updatedInput: { command: $c }
            }
        }'
        return
    fi
    for py in python3 python py; do
        if command -v "$py" >/dev/null 2>&1; then
            "$py" -c '
import json, sys
print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "allow",
        "updatedInput": {"command": sys.argv[1]},
    }
}))' "$1"
            return
        fi
    done
    # No encoder available: emit the no-op object rather than malformed JSON.
    echo '{}'
}

# Line caps. Enough to diagnose several failures at once, small enough that a fully
# broken workspace cannot flood the window. Raise deliberately, not reflexively.
TEST_TAIL=150
COV_TAIL=60

case "$cmd" in
    # ---- Coverage: the summary table and the total line are the whole point --------
    *"cargo llvm-cov"*|*"cargo tarpaulin"*)
        emit "$cmd 2>&1 | grep -E '(^Filename|^TOTAL|% *coverage|error|warning: .*coverage|panicked)' | head -${COV_TAIL}"
        ;;

    # ---- Tests: failures, panics, and the per-crate result lines -------------------
    *"cargo test"*|*"cargo nextest"*)
        emit "$cmd 2>&1 | grep -E '(^error|^warning|FAILED|panicked at|^failures:|^ *[A-Za-z0-9_:]+ *\\.\\.\\. FAILED|assertion .*failed|left:|right:|^test result:)' | head -${TEST_TAIL}"
        ;;

    # ---- Clippy: the diagnostics, not the 200 lines of Compiling ------------------
    *"cargo clippy"*)
        emit "$cmd 2>&1 | grep -E '(^error|^warning|^ *--> |^ *= (note|help):)' | head -${TEST_TAIL}"
        ;;

    *)
        echo '{}'
        ;;
esac

exit 0
