#!/usr/bin/env bash
# Runs the remote half embedded in deploy-remote.sh against a scratch $HOME and a stub server.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
deploy_script="${script_dir}/deploy-remote.sh"
work="$(mktemp -d)"
trap 'if [[ -f "${work}/home/.zed-web/server.pid" ]]; then kill "$(cat "${work}/home/.zed-web/server.pid")" 2>/dev/null; fi; rm -rf "${work}"' EXIT

# shellcheck disable=SC2016 # the pattern is a literal sed regex, not a shell expansion
sed -n '/^remote_script="\$(cat <<.REMOTE.$/,/^REMOTE$/p' "${deploy_script}" | sed '1d;$d' >"${work}/remote.sh"
[[ -s "${work}/remote.sh" ]] || { echo "could not extract the remote script from ${deploy_script}" >&2; exit 1; }

export HOME="${work}/home"
mkdir -p "${HOME}/.zed-web/dist/bin" "${HOME}/.zed-web/dist/static" "${HOME}/proj" "${HOME}/-dash" "${HOME}/with space"
cat >"${HOME}/.zed-web/dist/bin/zed-web-server" <<'STUB'
#!/usr/bin/env bash
echo "serving Zed Web"
sleep 30
STUB
chmod +x "${HOME}/.zed-web/dist/bin/zed-web-server"
cd "${HOME}" || exit 1

failures=0
run_remote() {
    output="$(bash -s -- "$1" 8090 "$2" <"${work}/remote.sh" 2>&1)"
    status=$?
}
expect() {
    local name="$1" want_status="$2" want_text="$3"
    if [[ "${status}" -eq "${want_status}" && "${output}" == *"${want_text}"* ]]; then
        echo "ok   ${name}"
    else
        echo "FAIL ${name}"
        echo "       expected: exit ${want_status}, output containing '${want_text}'"
        echo "       actual:   exit ${status}, output: ${output}"
        failures=$((failures + 1))
    fi
}

run_remote "proj" 0
expect "starts on a plain relative path" 0 "zed-web-deploy:project_root=${HOME}/proj"
run_remote "proj" 0
expect "second run without --restart refuses" 3 "already running"
run_remote "with space" 1
expect "--restart replaces the server, path with a space" 0 "zed-web-deploy:project_root=${HOME}/with space"
run_remote "-dash" 1
expect "a relative path starting with a dash is a directory, not an option" 0 "zed-web-deploy:project_root=${HOME}/-dash"
run_remote "nonexistent" 1
expect "a missing path is reported" 4 "not a directory"

[[ "${failures}" -eq 0 ]]
