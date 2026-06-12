#!/bin/sh
set -eu

REPO="${AGENT_REPO:-IPA-CyberLab/IPA-RS-IsolatedAgent}"
VERSION="${AGENT_VERSION:-latest}"
INSTALL_DIR="${AGENT_INSTALL_DIR:-$HOME/.local/bin}"
INSTALL_SERVICE="${AGENT_INSTALL_SERVICE:-0}"
DRY_RUN="${AGENT_INSTALL_DRY_RUN:-0}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: required command not found: $1" >&2
    exit 1
  }
}

sudo_cmd() {
  if [ "$(id -u)" -eq 0 ]; then
    return 0
  fi
  command -v sudo >/dev/null 2>&1 || {
    echo "error: sudo is required for this install target" >&2
    exit 1
  }
  printf 'sudo'
}

detect_target() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m | tr '[:upper:]' '[:lower:]')"

  case "$os" in
    linux*) os_part="unknown-linux-gnu" ;;
    darwin*) os_part="apple-darwin" ;;
    mingw*|msys*|cygwin*) os_part="pc-windows-msvc" ;;
    *)
      echo "error: unsupported operating system: $os" >&2
      exit 1
      ;;
  esac

  case "$arch" in
    x86_64|amd64) arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *)
      echo "error: unsupported architecture: $arch" >&2
      exit 1
      ;;
  esac

  printf '%s-%s' "$arch_part" "$os_part"
}

download() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --proto '=https' --tlsv1.2 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$out" "$url"
  else
    echo "error: curl or wget is required" >&2
    exit 1
  fi
}

profile_file() {
  shell_name="$(basename "${SHELL:-sh}")"
  case "$shell_name" in
    zsh) printf '%s/.zshrc' "$HOME" ;;
    bash) printf '%s/.bashrc' "$HOME" ;;
    *) printf '%s/.profile' "$HOME" ;;
  esac
}

path_contains_install_dir() {
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) return 0 ;;
    *) return 1 ;;
  esac
}

ensure_shell_path() {
  if path_contains_install_dir; then
    return 0
  fi

  profile="$(profile_file)"
  mkdir -p "$(dirname "$profile")"
  touch "$profile"
  if ! grep -F "export PATH=\"$INSTALL_DIR:\$PATH\"" "$profile" >/dev/null 2>&1; then
    {
      echo ""
      echo "# IPA-RS Isolated Agent"
      echo "export PATH=\"$INSTALL_DIR:\$PATH\""
    } >> "$profile"
  fi
  echo "Added $INSTALL_DIR to PATH in $profile"
}

ensure_windows_path() {
  command -v setx >/dev/null 2>&1 || return 0
  command -v cygpath >/dev/null 2>&1 || return 0
  win_install_dir="$(cygpath -w "$INSTALL_DIR")"
  current_path="$(cmd.exe /C echo %PATH% 2>/dev/null | tr -d '\r' || true)"
  case ";$current_path;" in
    *";$win_install_dir;"*) return 0 ;;
  esac
  setx PATH "$current_path;$win_install_dir" >/dev/null
  echo "Added $win_install_dir to the Windows user PATH"
}

install_macos_service() {
  need launchctl
  agentfs="${AGENTFS:-$HOME/.agentfs}"
  label="com.ipa-cyberlab.agent-forkd"
  plist_dir="$HOME/Library/LaunchAgents"
  plist="$plist_dir/$label.plist"
  mkdir -p "$plist_dir" "$agentfs"
  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$INSTALL_DIR/agent-forkd</string>
    <string>--agentfs</string>
    <string>$agentfs</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$agentfs/agent-forkd.out.log</string>
  <key>StandardErrorPath</key>
  <string>$agentfs/agent-forkd.err.log</string>
</dict>
</plist>
EOF
  uid="$(id -u)"
  launchctl bootout "gui/$uid" "$plist" >/dev/null 2>&1 || true
  launchctl bootstrap "gui/$uid" "$plist"
  launchctl enable "gui/$uid/$label"
  launchctl kickstart -k "gui/$uid/$label"
  echo "Installed and started launch agent: $label"
}

install_macos_privileged_helpers() {
  helper_dir="${AGENT_PRIVILEGED_HELPER_DIR:-/usr/local/libexec/ipa-rs-isolated-agent}"
  SUDO="$(sudo_cmd)"
  $SUDO mkdir -p "$helper_dir"
  $SUDO install -o root -g wheel -m 4755 "$payload_dir/bin/agent-viewd" "$helper_dir/agent-viewd"
  if [ -f "$payload_dir/bin/agent-overlayfs" ]; then
    $SUDO install -o root -g wheel -m 0755 "$payload_dir/bin/agent-overlayfs" "$helper_dir/agent-overlayfs"
  fi
  $ln_cmd "$helper_dir/agent-viewd" "$INSTALL_DIR/agent-viewd"
  echo "Installed macOS privileged helper: $helper_dir/agent-viewd"
}

target="$(detect_target)"
asset="ipa-rs-isolated-agent-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

if [ "$INSTALL_SERVICE" = "1" ]; then
  case "$target" in
    *unknown-linux-gnu)
      INSTALL_DIR="${AGENT_INSTALL_DIR:-/usr/local/bin}"
      if [ "$INSTALL_DIR" != "/usr/local/bin" ]; then
        echo "error: AGENT_INSTALL_SERVICE=1 requires AGENT_INSTALL_DIR=/usr/local/bin on Linux" >&2
        exit 1
      fi
      ;;
    *apple-darwin) ;;
    *)
      echo "error: AGENT_INSTALL_SERVICE=1 is supported by install.sh only on Linux and macOS" >&2
      exit 1
      ;;
  esac
fi

echo "Target: $target"
echo "Release: $VERSION"
echo "Install dir: $INSTALL_DIR"
echo "Install service: $INSTALL_SERVICE"

if [ "$DRY_RUN" = "1" ]; then
  echo "Download URL: $url"
  exit 0
fi

need tar
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

archive="$tmp_dir/$asset"
download "$url" "$archive"
tar -xzf "$archive" -C "$tmp_dir"

payload_dir="$tmp_dir/ipa-rs-isolated-agent-$target"
if [ -w "$(dirname "$INSTALL_DIR")" ] || { [ -d "$INSTALL_DIR" ] && [ -w "$INSTALL_DIR" ]; }; then
  mkdir -p "$INSTALL_DIR"
  install_cmd="install"
  cp_cmd="cp"
  ln_cmd="ln -sf"
else
  SUDO="$(sudo_cmd)"
  $SUDO mkdir -p "$INSTALL_DIR"
  install_cmd="$SUDO install"
  cp_cmd="$SUDO cp"
  ln_cmd="$SUDO ln -sf"
fi

if [ "$target" = "${target%pc-windows-msvc}" ]; then
  $install_cmd -m 0755 "$payload_dir/bin/agentctl" "$INSTALL_DIR/agentctl"
  $install_cmd -m 0755 "$payload_dir/bin/agent-forkd" "$INSTALL_DIR/agent-forkd"
  if [ -f "$payload_dir/bin/agent-viewd" ] && [ "$target" != "${target%apple-darwin}" ]; then
    install_macos_privileged_helpers
  elif [ -f "$payload_dir/bin/agent-viewd" ]; then
    $install_cmd -m 0755 "$payload_dir/bin/agent-viewd" "$INSTALL_DIR/agent-viewd"
  fi
  if [ -f "$payload_dir/bin/agent-overlayfs" ] && [ "$target" = "${target%apple-darwin}" ]; then
    $install_cmd -m 0755 "$payload_dir/bin/agent-overlayfs" "$INSTALL_DIR/agent-overlayfs"
  fi
  $ln_cmd agentctl "$INSTALL_DIR/agctl"
else
  $cp_cmd "$payload_dir/bin/agentctl.exe" "$INSTALL_DIR/agentctl.exe"
  $cp_cmd "$payload_dir/bin/agentctl.exe" "$INSTALL_DIR/agctl.exe"
  $cp_cmd "$payload_dir/bin/agent-forkd.exe" "$INSTALL_DIR/agent-forkd.exe"
fi

if [ "$INSTALL_SERVICE" = "1" ]; then
  case "$target" in
    *unknown-linux-gnu)
      SUDO="$(sudo_cmd)"
      $SUDO install -d -m 0755 /etc/agent-forkd
      $SUDO install -m 0644 "$payload_dir/packaging/agent-forkd/config.json" /etc/agent-forkd/config.json
      $SUDO install -m 0644 "$payload_dir/packaging/systemd/agent-forkd.service" /etc/systemd/system/agent-forkd.service
      $SUDO mkdir -p /agentfs
      $SUDO systemctl daemon-reload
      $SUDO systemctl enable agent-forkd >/dev/null
      $SUDO systemctl restart agent-forkd
      echo "Installed and restarted agent-forkd.service"
      ;;
    *apple-darwin)
      install_macos_service
      ;;
  esac
fi

ensure_shell_path
case "$target" in
  *pc-windows-msvc) ensure_windows_path ;;
esac

echo "Installed agentctl, agctl, agent-forkd, and available helper binaries to $INSTALL_DIR"
echo "Restart your shell or run: export PATH=\"$INSTALL_DIR:\$PATH\""
