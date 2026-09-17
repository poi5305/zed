#!/usr/bin/env bash
# Every RPC method zed_web_server dispatches must have a caller somewhere else.
#
# WHY THIS EXISTS. Three times in this port, a server handler was written to a
# design whose client half was never built, and nothing failed until something
# executed it -- which, for a path nobody had exercised, was never:
#
#   * `smol_wasm/src/rpc.rs`   -- a stand-in whose `call` returned "not wired
#                                 yet", so every Fs::* and Process::* call in
#                                 the browser failed. (Phase 4, handoff §1b)
#   * `Sql::bootstrap_kvp`     -- implemented and tested server-side, with no
#                                 client caller at all. It was the loader the
#                                 synchronous key-value reads needed, so every
#                                 panel attached and silently lost its state.
#   * `Terminal::{open,write,resize,bind,attach,close}` -- the whole terminal
#                                 surface, complete with portable-pty,
#                                 scrollback and its own tests. `RemotePty`,
#                                 the client that was meant to call it, existed
#                                 only inside three `panic!` strings. The
#                                 terminal was dead in the browser, and so was
#                                 tmux attach.
#
# `cargo check` cannot see any of this: both halves compile perfectly on their
# own. The link between them is a string, and nothing type-checks a string.
# This grep is the only thing that can notice, and it costs a second.
#
# An entry belongs in the allowlist ONLY if it is genuinely one-sided by
# design, and the reason is mandatory -- it is the whole value of the file.

set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
allowlist="${repo_dir}/web/one-sided-rpc.allowlist"
failures=0

server_methods="$(
    grep -rhoE '"[A-Z][A-Za-z]+::[a-z_]+"' "${repo_dir}/crates/zed_web_server/src/" \
        | tr -d '"' | sort -u
)"

if [[ -z "${server_methods}" ]]; then
    printf 'FAIL could not extract any RPC method names from crates/zed_web_server/src/\n'
    printf '     the extraction pattern has drifted from the source; fix this script\n'
    exit 1
fi

# Methods the client builds by concatenation rather than as a literal would be
# false positives. There are none today; if one appears, the fix is to name it
# here, not to loosen the check.
one_sided=""
while read -r method; do
    [[ -z "${method}" ]] && continue
    callers="$(
        grep -rl --include='*.rs' "\"${method}\"" "${repo_dir}/crates" "${repo_dir}/web" 2>/dev/null \
            | grep -v '/crates/zed_web_server/' || true
    )"
    if [[ -z "${callers}" ]]; then
        one_sided+="${method}"$'\n'
    fi
done <<< "${server_methods}"

allowed=""
if [[ -f "${allowlist}" ]]; then
    allowed="$(sed -e 's/#.*//' -e 's/[[:space:]]*$//' "${allowlist}" | grep -v '^$' || true)"
fi

while read -r method; do
    [[ -z "${method}" ]] && continue
    if ! grep -qxF "${method}" <<< "${allowed}"; then
        printf 'FAIL %s is dispatched by the server and called by nothing\n' "${method}"
        printf '     either write the client half, or add it to web/one-sided-rpc.allowlist with a reason\n'
        failures=$((failures + 1))
    fi
done <<< "${one_sided}"

# An allowlist entry that has since gained a caller is stale. Left alone it
# would hide the next regression of that same method, which is exactly the
# failure mode this script exists to catch.
while read -r method; do
    [[ -z "${method}" ]] && continue
    if ! grep -qxF "${method}" <<< "${one_sided}"; then
        printf 'FAIL %s is allowlisted as one-sided but now has a caller\n' "${method}"
        printf '     remove it from web/one-sided-rpc.allowlist\n'
        failures=$((failures + 1))
    fi
done <<< "${allowed}"

total="$(grep -c . <<< "${server_methods}")"
printf '\n%s server RPC methods checked, %d failures\n' "${total}" "${failures}"
[[ "${failures}" -eq 0 ]] && printf 'ONE-SIDED RPC OK\n'
[[ "${failures}" -eq 0 ]]
