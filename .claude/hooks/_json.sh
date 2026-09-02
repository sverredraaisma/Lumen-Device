# Shared JSON field extraction for hooks. Source this, do not execute it.
#
# WHY THIS EXISTS
# ---------------
# Every hook in the Planaday estate opens with:
#
#     command -v jq >/dev/null 2>&1 || exit 0
#
# On a machine without jq — which includes a stock Git Bash install on Windows — that
# line turns every hook into a no-op. Silently. The security guard stops guarding and
# nothing says so.
#
# So: try jq, fall back to python3, and if neither exists say so on stderr rather than
# pretending everything is fine.
#
# Usage:
#     source "$(dirname "${BASH_SOURCE[0]}")/_json.sh"
#     file=$(json_field "$input" 'tool_input.file_path')

# Extract a dotted-path string field from JSON on stdin. Prints the value or nothing.
# Returns 0 if a parser was available, 1 if none was.
json_field() {
    local json="$1" path="$2"

    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$json" | jq -r ".${path} // empty" 2>/dev/null
        return 0
    fi

    for py in python3 python py; do
        if command -v "$py" >/dev/null 2>&1; then
            printf '%s' "$json" | "$py" -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for key in sys.argv[1].split("."):
    if not isinstance(data, dict):
        sys.exit(0)
    data = data.get(key)
    if data is None:
        sys.exit(0)
print(data if isinstance(data, str) else json.dumps(data))
' "$path" 2>/dev/null
            return 0
        fi
    done

    return 1
}

# Call when json_field returns 1. Warns once per session rather than on every tool call,
# so a missing parser is visible without being deafening.
json_no_parser_warning() {
    local marker="${TMPDIR:-/tmp}/claude-hook-nojson-${CLAUDE_SESSION_ID:-noseesion}"
    [ -e "$marker" ] && return 0
    : > "$marker" 2>/dev/null || true
    echo "Claude Code hooks: neither jq nor python is on PATH, so hooks cannot read their input and are disabled. Install jq (https://jqlang.github.io/jq/) to re-enable them." >&2
}
