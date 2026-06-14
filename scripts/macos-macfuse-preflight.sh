#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macFUSE preflight must run on macOS" >&2
  exit 2
fi

if [[ ! -d /Library/Filesystems/macfuse.fs && ! -x /usr/local/bin/macfuse ]]; then
  echo "macFUSE is not installed; install macFUSE before running the macOS native backend" >&2
  exit 2
fi

if [[ "${AGENT_MACFUSE_TRY_LOAD:-1}" != "0" ]]; then
  if [[ -x /Library/Filesystems/macfuse.fs/Contents/Resources/load_macfuse ]]; then
    sudo /Library/Filesystems/macfuse.fs/Contents/Resources/load_macfuse || true
  fi
  for kext in /Library/Filesystems/macfuse.fs/Contents/Extensions/*/macfuse.kext /Library/Filesystems/macfuse.fs/Contents/Extensions/macfuse.kext; do
    if [[ -d "$kext" ]]; then
      sudo /usr/bin/kmutil load -p "$kext" || true
    fi
  done
fi

shopt -s nullglob
macfuse_devices=()
for candidate in /dev/fuse /dev/macfuse* /dev/osxfuse*; do
  [[ -e "$candidate" ]] && macfuse_devices+=("$candidate")
done
shopt -u nullglob
if (( ${#macfuse_devices[@]} == 0 )); then
  echo "macFUSE device is not available; approve and load the macFUSE kernel extension before running the macOS native backend" >&2
  exit 2
fi

ls -l "${macfuse_devices[@]}"
