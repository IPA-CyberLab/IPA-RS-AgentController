#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS native smoke test must run on macOS" >&2
  exit 2
fi

agentfs="${AGENTFS:-"$HOME/.agentfs-smoke"}"
bin_dir="${AGENT_BIN_DIR:-}"
agentctl="${AGENTCTL:-${bin_dir:+$bin_dir/}agentctl}"
forkd="${AGENT_FORKD:-${bin_dir:+$bin_dir/}agent-forkd}"
viewd="${AGENT_VIEWD:-$(command -v agent-viewd || true)}"
overlayfs="${AGENT_OVERLAYFS:-$(command -v agent-overlayfs || true)}"
daemon_log=""
cwd_out=""
new_cwd_out=""
command_timeout="${AGENT_SMOKE_COMMAND_TIMEOUT:-90}"

require_executable() {
  local name="$1"
  local path="$2"
  if [[ -z "$path" || ! -x "$path" ]]; then
    echo "$name is not executable; set AGENT_BIN_DIR or $name-specific env vars" >&2
    exit 2
  fi
}

require_executable AGENTCTL "$agentctl"
require_executable AGENT_FORKD "$forkd"
require_executable AGENT_VIEWD "$viewd"
require_executable AGENT_OVERLAYFS "$overlayfs"
require_executable NC /usr/bin/nc

run_with_timeout() {
  local seconds="$1"
  shift
  echo "running (${seconds}s timeout): $*" >&2
  local stdout_file stderr_file
  stdout_file="$(mktemp "${TMPDIR:-/tmp}/ipa-rs-smoke-stdout.XXXXXX")"
  stderr_file="$(mktemp "${TMPDIR:-/tmp}/ipa-rs-smoke-stderr.XXXXXX")"
  "$@" >"$stdout_file" 2>"$stderr_file" &
  local pid="$!"
  (
    trap 'kill "$sleep_pid" >/dev/null 2>&1 || true; exit 0' TERM INT
    sleep "$seconds" &
    local sleep_pid="$!"
    wait "$sleep_pid"
    if kill -0 "$pid" >/dev/null 2>&1; then
      echo "command timed out after ${seconds}s: $*" >&2
      kill "$pid" >/dev/null 2>&1 || true
      sleep 2
      kill -KILL "$pid" >/dev/null 2>&1 || true
    fi
  ) &
  local timer_pid="$!"
  local status=0
  wait "$pid" || status="$?"
  kill "$timer_pid" >/dev/null 2>&1 || true
  wait "$timer_pid" >/dev/null 2>&1 || true
  cat "$stdout_file" || true
  cat "$stderr_file" >&2 || true
  rm -f "$stdout_file" "$stderr_file"
  return "$status"
}

echo "agentctl=$agentctl"
echo "agent-forkd=$forkd"
echo "agent-viewd=$viewd"
echo "agent-overlayfs=$overlayfs"

path_viewd="$(command -v agent-viewd || true)"
path_overlayfs="$(command -v agent-overlayfs || true)"
if [[ -z "$path_viewd" || -z "$path_overlayfs" ]]; then
  echo "agent-viewd and agent-overlayfs must be visible on PATH after install" >&2
  exit 2
fi
if [[ "$(stat -L -f '%d:%i' "$path_viewd")" != "$(stat -L -f '%d:%i' "$viewd")" ]]; then
  echo "PATH agent-viewd resolves to $path_viewd, but smoke uses $viewd" >&2
  exit 2
fi
if [[ "$(stat -L -f '%d:%i' "$path_overlayfs")" != "$(stat -L -f '%d:%i' "$overlayfs")" ]]; then
  echo "PATH agent-overlayfs resolves to $path_overlayfs, but smoke uses $overlayfs" >&2
  exit 2
fi
echo "verified helpers are visible on PATH"

if [[ ! -d /Library/Filesystems/macfuse.fs && ! -x /usr/local/bin/macfuse ]]; then
  echo "macFUSE is not installed; install macFUSE before running this smoke test" >&2
  exit 2
fi
if ! ls /dev/fuse /dev/macfuse* /dev/osxfuse* >/dev/null 2>&1; then
  echo "macFUSE device is not available; load and approve the macFUSE kernel extension before running this smoke test" >&2
  exit 2
fi

viewd_owner="$(stat -L -f %u "$viewd")"
viewd_mode="$(stat -L -f %Sp "$viewd")"
if [[ "$viewd_owner" != "0" || "${viewd_mode:3:1}" != "s" ]]; then
  echo "agent-viewd must resolve to a root-owned setuid helper; got owner=$viewd_owner mode=$viewd_mode path=$viewd" >&2
  exit 2
fi
echo "verified agent-viewd owner=$viewd_owner mode=$viewd_mode"

"$overlayfs" check
echo "verified agent-overlayfs check"

tmp="${TMPDIR:-/tmp}/ipa-rs-native-smoke.$$"
source_dir="$tmp/source"
mkdir -p "$source_dir/nested"
printf 'source-ok\n' > "$source_dir/nested/source.txt"

daemon_log="$tmp/agent-forkd.log"
mkdir -p "$agentfs"
AGENT_VIEWD="$viewd" "$forkd" --agentfs "$agentfs" >"$daemon_log" 2>&1 &
daemon_pid=$!

server_pid=""
dump_failure() {
  status="$?"
  echo "macOS native smoke test failed with status $status" >&2
  if [[ -n "$cwd_out" && -f "$cwd_out" ]]; then
    echo "---- cwd command output ----" >&2
    cat "$cwd_out" >&2 || true
  fi
  if [[ -n "$new_cwd_out" && -f "$new_cwd_out" ]]; then
    echo "---- new command output ----" >&2
    cat "$new_cwd_out" >&2 || true
  fi
  if [[ -n "$daemon_log" && -f "$daemon_log" ]]; then
    echo "---- agent-forkd log ----" >&2
    cat "$daemon_log" >&2 || true
  fi
  exit "$status"
}

cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
  fi
  kill "$daemon_pid" >/dev/null 2>&1 || true
  wait "$daemon_pid" >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap dump_failure ERR
trap cleanup EXIT

for _ in {1..80}; do
  if "$agentctl" --agentfs "$agentfs" init >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done
"$agentctl" --agentfs "$agentfs" init >/dev/null

env_id="mac-smoke-$$"
net_host_id="mac-smoke-net-host-$$"
net_none_id="mac-smoke-net-none-$$"

new_cwd_out="$tmp/new-cwd.out"
(
  cd "$source_dir/nested"
  run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" new -t "$env_id" --from "$source_dir" -- /bin/zsh -fc '
    set -e
    printf "new-pwd=%s\n" "$PWD"
    cat source.txt
  '
) | tee "$new_cwd_out"

grep -F "new-pwd=$source_dir/nested" "$new_cwd_out" >/dev/null
grep -F "source-ok" "$new_cwd_out" >/dev/null
echo "verified new command preserved cwd and source visibility"

cwd_out="$tmp/cwd.out"
(
  cd "$source_dir/nested"
  run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc '
    set -e
    printf "pwd=%s\n" "$PWD"
    /usr/bin/env true
    test -x /bin/zsh
    test -e /usr/lib/dyld
    test -d /System/Library
    cat source.txt
  '
) | tee "$cwd_out"

grep -F "pwd=$source_dir/nested" "$cwd_out" >/dev/null
grep -F "source-ok" "$cwd_out" >/dev/null
echo "verified preserved cwd, macOS runtime paths, and source visibility"

if [[ -e "$HOME/.ssh" && "$HOME/.ssh" != "$source_dir"* ]]; then
  (
    cd "$source_dir/nested"
    run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc 'test ! -e "$HOME/.ssh"'
  )
  echo "verified host home secrets are not visible"
fi

if [[ -e /private/var/db ]]; then
  (
    cd "$source_dir/nested"
    run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc '
      test -d /private/var/tmp
      test ! -e /private/var/db
    '
  )
  echo "verified macOS fallback sibling isolation"
fi

if [[ -e /usr/local ]]; then
  (
    cd "$source_dir/nested"
    run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc 'test ! -e /usr/local'
  )
  echo "verified broad /usr/local host tree is not visible"
fi

if [[ -e "/Library/Application Support" ]]; then
  (
    cd "$source_dir/nested"
    run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc 'test ! -e "/Library/Application Support"'
  )
  echo "verified broad /Library application data is not visible"
fi

if [[ -e /private/etc/ssh ]]; then
  (
    cd "$source_dir/nested"
    run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc 'test ! -e /private/etc/ssh'
  )
  echo "verified broad /private/etc config tree is not visible"
fi

port="${AGENT_SMOKE_PORT:-38476}"
while true; do
  printf 'ok\n' | /usr/bin/nc -l 127.0.0.1 "$port" >/dev/null 2>&1 || true
done &
server_pid=$!
sleep 0.5

run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" new -t "$net_host_id" --network host --from "$source_dir" -- \
  /usr/bin/nc -G 2 -z 127.0.0.1 "$port"
echo "verified network=host reaches 127.0.0.1:$port"

if run_with_timeout "$command_timeout" "$agentctl" --agentfs "$agentfs" new -t "$net_none_id" --network none --from "$source_dir" -- \
  /usr/bin/nc -G 2 -z 127.0.0.1 "$port"; then
  echo "network=none unexpectedly reached 127.0.0.1:$port" >&2
  exit 1
fi
echo "verified network=none cannot reach 127.0.0.1:$port"

echo "macOS native smoke test passed"
