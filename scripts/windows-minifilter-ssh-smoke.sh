#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: scripts/windows-minifilter-ssh-smoke.sh user@host [remote-repo-path]

Runs the Windows minifilter smoke test on a real Windows machine over SSH.

Defaults:
  key:              ./.key
  remote repo path: C:\Users\mizuame\Desktop\script\IPA-RS-IsolatedAgent
  branch:           current local git branch

The remote checkout must be clean. The script fetches origin, checks out the
current branch, fast-forwards it, and runs scripts\windows-minifilter-smoke.ps1
from an elevated/admin-capable SSH session.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit $([[ $# -lt 1 || $# -gt 2 ]] && echo 2 || echo 0)
fi

target="$1"
remote_repo="${2:-C:\\Users\\mizuame\\Desktop\\script\\IPA-RS-IsolatedAgent}"
key="${AGENT_WINDOWS_SSH_KEY:-.key}"
branch="${AGENT_WINDOWS_BRANCH:-$(git branch --show-current)}"

if [[ -z "$branch" ]]; then
  echo "Could not determine current git branch; set AGENT_WINDOWS_BRANCH." >&2
  exit 2
fi
if [[ ! -f "$key" ]]; then
  echo "SSH key not found: $key" >&2
  exit 2
fi

ssh_opts=(
  -i "$key"
  -o BatchMode=yes
  -o StrictHostKeyChecking=accept-new
  -o ServerAliveInterval=30
  -o ServerAliveCountMax=4
)

ps_quote() {
  local value="$1"
  value="${value//\'/\'\'}"
  printf "'%s'" "$value"
}

remote_repo_ps="$(ps_quote "$remote_repo")"
branch_ps="$(ps_quote "$branch")"

remote_script=$(cat <<'POWERSHELL'
$ErrorActionPreference = "Stop"
$repo = __REMOTE_REPO__
$branch = __BRANCH__

if (-not (Test-Path $repo)) {
    throw "Remote repo path does not exist: $repo"
}

Set-Location $repo
$dirty = git status --porcelain
if ($dirty) {
    throw "Remote checkout is dirty; refusing to overwrite:`n$dirty"
}

git fetch origin $branch
git checkout $branch
git pull --ff-only origin $branch

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "SSH session is not elevated/admin; Windows minifilter smoke needs admin rights."
}

powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\windows-minifilter-smoke.ps1
POWERSHELL
)
remote_script="${remote_script/__REMOTE_REPO__/$remote_repo_ps}"
remote_script="${remote_script/__BRANCH__/$branch_ps}"

echo "Running Windows minifilter smoke on $target"
echo "Remote repo: $remote_repo"
echo "Branch: $branch"

printf '%s' "$remote_script" | ssh "${ssh_opts[@]}" "$target" \
  "powershell.exe -NoProfile -ExecutionPolicy Bypass -Command -"
