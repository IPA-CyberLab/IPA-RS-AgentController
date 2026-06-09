#!/usr/bin/env bash
set -u

failures=0

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

pass() {
  printf 'PASS: %s\n' "$*"
}

check_command() {
  if command -v "$1" >/dev/null 2>&1; then
    pass "command $1 is available"
  else
    fail "command $1 is missing"
  fi
}

check_root() {
  if [ "$(id -u)" = "0" ]; then
    pass "running as root"
  else
    fail "run as root; privileged_sequence invokes chroot and Btrfs inspection commands"
  fi
}

check_systemd() {
  local pid1
  pid1="$(ps -p 1 -o comm= 2>/dev/null || true)"
  if [ "$pid1" = "systemd" ]; then
    pass "PID 1 is systemd"
  else
    fail "PID 1 is $pid1, expected systemd"
  fi
}

check_cgroup_v2() {
  local fs_type
  fs_type="$(stat -fc %T /sys/fs/cgroup 2>/dev/null || true)"
  if [ "$fs_type" = "cgroup2fs" ]; then
    pass "cgroup v2 is mounted"
  else
    fail "/sys/fs/cgroup type is $fs_type, expected cgroup2fs"
  fi
}

check_userns() {
  local value
  value="$(sysctl -n kernel.unprivileged_userns_clone 2>/dev/null || true)"
  if [ "$value" = "1" ] || [ -z "$value" ]; then
    pass "user namespace sysctl is compatible"
  else
    fail "kernel.unprivileged_userns_clone is $value, expected 1 or unavailable"
  fi
}

check_btrfs_target() {
  local target="$1"
  local fs_type
  fs_type="$(findmnt -n -o FSTYPE --target "$target" 2>/dev/null || true)"
  if [ "$fs_type" = "btrfs" ]; then
    pass "$target is on Btrfs"
  else
    fail "$target is on $fs_type, expected btrfs"
  fi
}

check_subvolume() {
  local path="$1"
  if btrfs subvolume show "$path" >/dev/null 2>&1; then
    pass "$path is a Btrfs subvolume"
  else
    fail "$path is not a Btrfs subvolume"
  fi
}

check_systemd_unit_active() {
  local unit="$1"
  if systemctl is-active --quiet "$unit"; then
    pass "$unit is active"
  else
    fail "$unit is not active"
  fi
}

check_systemd_unit_available() {
  local unit="$1"
  if systemctl cat "$unit" >/dev/null 2>&1; then
    pass "$unit is available"
  else
    fail "$unit is not available"
  fi
}

check_socket() {
  local path="$1"
  if [ -S "$path" ]; then
    pass "$path socket exists"
  else
    fail "$path socket is missing"
  fi
}

check_disk_space() {
  local target="/agentfs"
  local available_kib
  available_kib="$(df -Pk "$target" 2>/dev/null | awk 'NR == 2 { print $4 }')"
  if [ -z "$available_kib" ]; then
    fail "could not inspect free space for $target"
    return
  fi
  if [ "$available_kib" -ge 125829120 ]; then
    pass "$target has at least 120 GiB free"
  else
    fail "$target has less than 120 GiB free"
  fi
}

main() {
  check_root
  check_systemd
  check_cgroup_v2
  check_userns

  for program in \
    btrfs \
    cargo \
    chroot \
    codex \
    dpkg \
    findmnt \
    git \
    machinectl \
    rustc \
    sudo \
    systemctl \
    systemd-nspawn \
    systemd-run \
    tee \
    tmux
  do
    check_command "$program"
  done

  if command -v apt >/dev/null 2>&1 || command -v apt-get >/dev/null 2>&1; then
    pass "apt or apt-get is available"
  else
    fail "apt or apt-get is missing"
  fi

  if [ -x /bin/bash ]; then
    pass "/bin/bash is executable"
  else
    fail "/bin/bash is missing or not executable"
  fi

  check_btrfs_target /
  check_btrfs_target /agentfs
  check_subvolume /
  check_disk_space
  check_systemd_unit_available systemd-machined
  check_systemd_unit_active systemd-networkd
  check_systemd_unit_active agent-forkd
  check_socket /agentfs/runtime/sockets/agent-forkd.sock

  if [ "$failures" -eq 0 ]; then
    printf 'privileged test environment preflight passed\n'
    exit 0
  fi

  printf 'privileged test environment preflight failed with %s issue(s)\n' "$failures" >&2
  exit 1
}

main "$@"
