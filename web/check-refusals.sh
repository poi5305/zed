#!/usr/bin/env bash
# Asserts docs/web-zed-plan.md §5.3 post-merge refusal invariants.
#
# Four changes from zed-web that must never be merged into our tree:
# 1. crates/zed/RELEASE_CHANNEL: must be 'dev' (zed-web changed to 'stable').
# 2. crates/terminal/src/terminal.rs: Shift+Click selection extension must exist (zed-web deleted it).
# 3. crates/recent_projects/src/recent_projects.rs: open_local_project PathPromptOptions.files must be true (zed-web changed to false).
# 4. crates/remote_server/src/server.rs: MultiWrite::flush must use send_blocking (zed-web changed to try_send).
#
# Run from anywhere: ./web/check-refusals.sh
# Exits non-zero if any assertion fails; all assertions run to completion.

set -uo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

failures=0
checks=0

pass() {
    checks=$((checks + 1))
    printf 'ok   %s\n' "$1"
}

fail() {
    checks=$((checks + 1))
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    shift
    while [[ $# -gt 0 ]]; do
        printf '       %s\n' "$1"
        shift
    done
}

expect_equal() {
    local label="$1" expected="$2" actual="$3"
    if [[ "${expected}" == "${actual}" ]]; then
        pass "${label}"
    else
        fail "${label}" "expected: ${expected}" "actual:   ${actual}"
    fi
}

# ---------------------------------------------------------------------------
# §5.3.1 — crates/zed/RELEASE_CHANNEL must be dev (zed-web changed to stable)
# ---------------------------------------------------------------------------
channel_file="${repo_dir}/crates/zed/RELEASE_CHANNEL"
if [[ -f "${channel_file}" ]]; then
    actual_channel="$(tr -d '[:space:]' < "${channel_file}")"
else
    actual_channel="<crates/zed/RELEASE_CHANNEL missing>"
fi
expect_equal "§5.3.1 RELEASE_CHANNEL is dev" "dev" "${actual_channel}"

# ---------------------------------------------------------------------------
# §5.3.2 — crates/terminal/src/terminal.rs Shift+Click selection extension exists
# (zed-web deleted the extend-existing branch from #25143 / #60880)
# ---------------------------------------------------------------------------
terminal_file="${repo_dir}/crates/terminal/src/terminal.rs"
terminal_shift_click_status=$(python3 - "${terminal_file}" <<'PY'
import os, re, sys

path = sys.argv[1]
if not os.path.isfile(path):
    print("<crates/terminal/src/terminal.rs missing>")
    sys.exit(0)

content = open(path).read()
pattern = r'if\s+selection_type\s*==\s*Some\(SelectionType::Simple\)\s*&&\s*e\.modifiers\.shift'
match = re.search(pattern, content)
if not match:
    print("<shift-click handler missing>")
    sys.exit(0)

brace_start = content.find("{", match.end())
if brace_start == -1:
    print("<malformed shift-click handler: opening brace not found>")
    sys.exit(0)

depth = 1
idx = brace_start + 1
while idx < len(content) and depth > 0:
    if content[idx] == "{":
        depth += 1
    elif content[idx] == "}":
        depth -= 1
    idx += 1

body = content[brace_start:idx]

has_selection_check = "self.last_content.selection.is_some()" in body
has_update_selection = "InternalEvent::UpdateSelection" in body
has_set_selection = "InternalEvent::SetSelection" in body

if has_selection_check and has_update_selection and has_set_selection:
    print("present (last_content.selection.is_some ? UpdateSelection : SetSelection)")
elif has_update_selection and not has_selection_check:
    print("deleted (unconditional UpdateSelection without selection.is_some check)")
else:
    print(f"unexpected body: {' '.join(body.split())}")
PY
)
expect_equal "§5.3.2 terminal Shift+Click selection extension exists" \
    "present (last_content.selection.is_some ? UpdateSelection : SetSelection)" \
    "${terminal_shift_click_status}"

# ---------------------------------------------------------------------------
# §5.3.3 — crates/recent_projects/src/recent_projects.rs: open_local_project
# PathPromptOptions.files must be true (zed-web changed to false).
# Scoped to open_local_project to avoid false positive from WSL PathPromptOptions.
# ---------------------------------------------------------------------------
recent_projects_file="${repo_dir}/crates/recent_projects/src/recent_projects.rs"
open_local_files_status=$(python3 - "${recent_projects_file}" <<'PY'
import os, re, sys

path = sys.argv[1]
if not os.path.isfile(path):
    print("<crates/recent_projects/src/recent_projects.rs missing>")
    sys.exit(0)

content = open(path).read()
pos = content.find("fn open_local_project(")
if pos == -1:
    print("<fn open_local_project not found>")
    sys.exit(0)

brace_start = content.find("{", pos)
if brace_start == -1:
    print("<fn open_local_project malformed: opening brace not found>")
    sys.exit(0)

depth = 1
idx = brace_start + 1
while idx < len(content) and depth > 0:
    if content[idx] == "{":
        depth += 1
    elif content[idx] == "}":
        depth -= 1
    idx += 1

fn_body = content[brace_start:idx]
match = re.search(r'PathPromptOptions\s*\{([^}]+)\}', fn_body)
if not match:
    print("<PathPromptOptions not found in open_local_project>")
    sys.exit(0)

options_block = match.group(1)
files_match = re.search(r'\bfiles\s*:\s*(true|false)\b', options_block)
if files_match:
    print(files_match.group(1))
else:
    print(f"<files field not found in PathPromptOptions: {options_block.strip()}>")
PY
)
expect_equal "§5.3.3 recent_projects open_local_project PathPromptOptions.files is true" \
    "true" \
    "${open_local_files_status}"

# ---------------------------------------------------------------------------
# §5.3.4 — crates/remote_server/src/server.rs MultiWrite::flush must use send_blocking
# (zed-web changed to try_send)
# ---------------------------------------------------------------------------
remote_server_file="${repo_dir}/crates/remote_server/src/server.rs"
server_flush_status=$(python3 - "${remote_server_file}" <<'PY'
import os, re, sys

path = sys.argv[1]
if not os.path.isfile(path):
    print("<crates/remote_server/src/server.rs missing>")
    sys.exit(0)

content = open(path).read()
pos = content.find("impl Write for MultiWrite")
if pos == -1:
    print("<impl Write for MultiWrite not found>")
    sys.exit(0)

brace_start = content.find("{", pos)
if brace_start == -1:
    print("<impl Write for MultiWrite malformed: opening brace not found>")
    sys.exit(0)

depth = 1
idx = brace_start + 1
while idx < len(content) and depth > 0:
    if content[idx] == "{":
        depth += 1
    elif content[idx] == "}":
        depth -= 1
    idx += 1

impl_body = content[brace_start:idx]
pos_flush = impl_body.find("fn flush(")
if pos_flush == -1:
    print("<fn flush not found in MultiWrite>")
    sys.exit(0)

flush_brace_start = impl_body.find("{", pos_flush)
if flush_brace_start == -1:
    print("<fn flush malformed: opening brace not found>")
    sys.exit(0)

depth = 1
idx = flush_brace_start + 1
while idx < len(impl_body) and depth > 0:
    if impl_body[idx] == "{":
        depth += 1
    elif impl_body[idx] == "}":
        depth -= 1
    idx += 1

flush_body = impl_body[flush_brace_start:idx]
match = re.search(r'self\.channel\s*\.\s*([a-zA-Z0-9_]+)\s*\(', flush_body)
if match:
    print(match.group(1))
else:
    print(f"<channel send method not found: {flush_body.strip()}>")
PY
)
expect_equal "§5.3.4 remote_server MultiWrite::flush uses send_blocking" \
    "send_blocking" \
    "${server_flush_status}"

# ---------------------------------------------------------------------------
printf '\n%d checks, %d failures\n' "${checks}" "${failures}"
[[ "${failures}" -eq 0 ]]
