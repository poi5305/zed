#!/usr/bin/env bash
# Copies web/dist to a remote host over ssh and starts zed-web-server there (fallback "C" in
# docs/web-zed-remote-spec.md: browse a remote project by running the whole web build next to it).
#
# The server is started bound to 127.0.0.1 on purpose: the only way in is the ssh tunnel this
# script prints, so the remote host never exposes the editor (and its shell) to the network.
set -euo pipefail

die() {
    printf '%s\n' "$@" >&2
    exit 1
}

usage() {
    cat >&2 <<'USAGE'
Usage: deploy-remote.sh <ssh-destination> <remote-project-path> [--port N] [--restart]

  <ssh-destination>     anything `ssh` accepts (user@host, a Host from ~/.ssh/config)
  <remote-project-path> project directory on the remote host; `~/...` is expanded there
  --port N              port zed-web-server listens on, on the remote loopback (default 8090)
  --restart             stop a zed-web-server this script started earlier, then start a new one

Authenticate with a public key or ssh-agent; a password prompt works but repeats for each
ssh/rsync call this script makes.
USAGE
    exit 2
}

web_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist_dir="${web_dir}/dist"
server_binary="${dist_dir}/bin/zed-web-server"

destination=""
project_path=""
port="8090"
restart="0"

while [[ $# -gt 0 ]]; do
    case "$1" in
    --port)
        [[ $# -ge 2 ]] || usage
        port="$2"
        shift 2
        ;;
    --port=*)
        port="${1#--port=}"
        shift
        ;;
    --restart)
        restart="1"
        shift
        ;;
    -h | --help)
        usage
        ;;
    -*)
        printf 'Unknown option: %s\n' "$1" >&2
        usage
        ;;
    *)
        if [[ -z "${destination}" ]]; then
            destination="$1"
        elif [[ -z "${project_path}" ]]; then
            project_path="$1"
        else
            printf 'Unexpected argument: %s\n' "$1" >&2
            usage
        fi
        shift
        ;;
    esac
done

[[ -n "${destination}" && -n "${project_path}" ]] || usage
# A leading dash would be parsed as an ssh/rsync option instead of a host.
[[ "${destination}" != -* ]] || die "Invalid ssh destination: ${destination}"
if ! [[ "${port}" =~ ^[0-9]+$ ]] || ((10#${port} < 1 || 10#${port} > 65535)); then
    die "Invalid port: ${port}"
fi
port="$((10#${port}))"

for tool in ssh rsync file; do
    command -v "${tool}" >/dev/null 2>&1 || die "Required tool not found on this machine: ${tool}"
done

[[ -f "${server_binary}" ]] || die \
    "Missing ${server_binary}" \
    "Run web/build.sh first; it produces web/dist/bin/zed-web-server and web/dist/static."
[[ -d "${dist_dir}/static" ]] || die "Missing ${dist_dir}/static; run web/build.sh first."

echo "Checking the remote platform on ${destination}..."
remote_platform="$(ssh -- "${destination}" uname -sm)" ||
    die "Could not run 'uname -sm' on ${destination} over ssh."
remote_platform="$(printf '%s' "${remote_platform}" | tr -d '\r')"

# `file` reports ELF as "x86-64" but Mach-O as "x86_64", so each platform gets its own pattern.
case "${remote_platform}" in
"Linux x86_64") expected_pattern='ELF.*x86-64' ;;
"Linux aarch64" | "Linux arm64") expected_pattern='ELF.*aarch64' ;;
"Darwin x86_64") expected_pattern='Mach-O.*x86_64' ;;
"Darwin arm64") expected_pattern='Mach-O.*arm64' ;;
*) die "Unsupported remote platform: '${remote_platform}' (supported: Linux/Darwin on x86_64/aarch64/arm64)." ;;
esac

binary_description="$(file -b "${server_binary}")"
if ! printf '%s' "${binary_description}" | grep -Eq "${expected_pattern}"; then
    die \
        "Architecture mismatch: ${destination} is '${remote_platform}', but ${server_binary} is:" \
        "  ${binary_description}" \
        "web/build.sh builds the server for this machine only. Build web/dist on a machine of the" \
        "same platform as ${destination}, or run deploy-remote.sh from one."
fi

echo "Copying ${dist_dir}/ to ${destination}:~/.zed-web/dist/ ..."
ssh -- "${destination}" 'mkdir -p "$HOME/.zed-web/dist"' ||
    die "Could not create ~/.zed-web/dist on ${destination}."
rsync -a "${dist_dir}/" "${destination}:.zed-web/dist/" ||
    die "rsync to ${destination} failed."

# Runs on the remote host. The project path travels as an argument (never spliced into the script)
# so that spaces and quotes in it cannot change what runs.
remote_script="$(cat <<'REMOTE'
set -eu
project_path=$1
port=$2
restart=$3

case "$project_path" in
"~") project_path=$HOME ;;
"~/"*) project_path=$HOME/${project_path#"~/"} ;;
esac
[ -d "$project_path" ] || { echo "Remote project path is not a directory: $project_path" >&2; exit 4; }
project_root=$(cd -- "$project_path" && pwd -P)

state_dir=$HOME/.zed-web
pid_file=$state_dir/server.pid
log_file=$state_dir/server.log
binary=$state_dir/dist/bin/zed-web-server

is_our_server() {
    ps -p "$1" -o args= 2>/dev/null | grep -q "zed-web-server"
}

if [ -f "$pid_file" ]; then
    old_pid=$(cat "$pid_file")
    if [ -n "$old_pid" ] && is_our_server "$old_pid"; then
        if [ "$restart" != 1 ]; then
            echo "zed-web-server is already running (pid $old_pid); rerun with --restart to replace it." >&2
            exit 3
        fi
        kill "$old_pid"
        waited=0
        while is_our_server "$old_pid" && [ "$waited" -lt 50 ]; do
            sleep 0.1
            waited=$((waited + 1))
        done
        if is_our_server "$old_pid"; then
            echo "The previous zed-web-server (pid $old_pid) did not stop after 5 seconds." >&2
            exit 5
        fi
    fi
fi

nohup "$binary" "$project_root" "$state_dir/dist/static" --port "$port" >"$log_file" 2>&1 </dev/null &
new_pid=$!
echo "$new_pid" >"$pid_file"

waited=0
while [ "$waited" -lt 100 ]; do
    if ! kill -0 "$new_pid" 2>/dev/null; then
        echo "zed-web-server exited during startup. Last log lines:" >&2
        tail -n 20 "$log_file" >&2
        exit 1
    fi
    if grep -q "serving Zed Web" "$log_file" 2>/dev/null; then
        break
    fi
    sleep 0.2
    waited=$((waited + 1))
done
if [ "$waited" -ge 100 ]; then
    echo "zed-web-server is running (pid $new_pid) but has not logged that it is serving yet; check $log_file." >&2
fi
printf "zed-web-deploy:project_root=%s\n" "$project_root"
printf "zed-web-deploy:pid=%s\n" "$new_pid"
REMOTE
)"

echo "Starting zed-web-server on ${destination} (port ${port})..."
# The remote login shell parses this command line, so every argument goes through %q.
remote_command="bash -s -- $(printf '%q %q %q' "${project_path}" "${port}" "${restart}")"
start_status=0
start_output="$(printf '%s\n' "${remote_script}" | ssh -- "${destination}" "${remote_command}" 2>&1)" ||
    start_status=$?
if [[ "${start_status}" -ne 0 ]]; then
    printf '%s\n' "${start_output}" >&2
    die "Starting zed-web-server on ${destination} failed (exit ${start_status})."
fi

remote_root="$(printf '%s\n' "${start_output}" | sed -n 's/^zed-web-deploy:project_root=//p' | tail -n 1)"
remote_pid="$(printf '%s\n' "${start_output}" | sed -n 's/^zed-web-deploy:pid=//p' | tail -n 1)"
printf '%s\n' "${start_output}" | grep -v '^zed-web-deploy:' >&2 || true
[[ -n "${remote_root}" ]] || die "The remote start script did not report the project root; output was:" "${start_output}"

cat <<EOF

zed-web-server is running on ${destination} (pid ${remote_pid}), listening on 127.0.0.1:${port} there.

1. Open the tunnel on this machine and leave it running:
     ssh -N -L ${port}:127.0.0.1:${port} ${destination}
2. Browse to http://127.0.0.1:${port}/
3. Sign in with the token in this file on ${destination}:
     ${remote_root}/.zed/web-auth-token
   (read it with: ssh ${destination} cat '${remote_root}/.zed/web-auth-token')

Server log on ${destination}: ~/.zed-web/server.log
Stop it with: ssh ${destination} 'kill \$(cat ~/.zed-web/server.pid)'
EOF
