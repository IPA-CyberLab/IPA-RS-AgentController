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

if [[ ! -d /Library/Filesystems/macfuse.fs && ! -x /usr/local/bin/macfuse ]]; then
  echo "macFUSE is not installed; install macFUSE before running this smoke test" >&2
  exit 2
fi

viewd_owner="$(stat -L -f %u "$viewd")"
viewd_mode="$(stat -L -f %Sp "$viewd")"
if [[ "$viewd_owner" != "0" || "${viewd_mode:3:1}" != "s" ]]; then
  echo "agent-viewd must resolve to a root-owned setuid helper; got owner=$viewd_owner mode=$viewd_mode path=$viewd" >&2
  exit 2
fi

"$overlayfs" check

tmp="${TMPDIR:-/tmp}/ipa-rs-native-smoke.$$"
source_dir="$tmp/source"
mkdir -p "$source_dir/nested"
printf 'source-ok\n' > "$source_dir/nested/source.txt"

daemon_log="$tmp/agent-forkd.log"
mkdir -p "$agentfs"
AGENT_VIEWD="$viewd" "$forkd" --agentfs "$agentfs" >"$daemon_log" 2>&1 &
daemon_pid=$!

server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
  fi
  kill "$daemon_pid" >/dev/null 2>&1 || true
  wait "$daemon_pid" >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
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

"$agentctl" --agentfs "$agentfs" new -t "$env_id" --from "$source_dir" -- /bin/zsh -fc 'true'

cwd_out="$tmp/cwd.out"
(
  cd "$source_dir/nested"
  "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc '
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

if [[ -e "$HOME/.ssh" && "$HOME/.ssh" != "$source_dir"* ]]; then
  (
    cd "$source_dir/nested"
    "$agentctl" --agentfs "$agentfs" exec "$env_id" -- /bin/zsh -fc 'test ! -e "$HOME/.ssh"'
  )
fi

port="${AGENT_SMOKE_PORT:-38476}"
while true; do
  printf 'ok\n' | /usr/bin/nc -l 127.0.0.1 "$port" >/dev/null 2>&1 || true
done &
server_pid=$!
sleep 0.5

"$agentctl" --agentfs "$agentfs" new -t "$net_host_id" --network host --from "$source_dir" -- \
  /usr/bin/nc -G 2 -z 127.0.0.1 "$port"

if "$agentctl" --agentfs "$agentfs" new -t "$net_none_id" --network none --from "$source_dir" -- \
  /usr/bin/nc -G 2 -z 127.0.0.1 "$port"; then
  echo "network=none unexpectedly reached 127.0.0.1:$port" >&2
  exit 1
fi

echo "macOS native smoke test passed"
