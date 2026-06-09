#!/bin/sh
set -eu

REPO="${AGENT_REPO:-IPA-CyberLab/IPA-RS-IsolatedAgent}"
VERSION="${AGENT_VERSION:-latest}"
INSTALL_DIR="${AGENT_INSTALL_DIR:-$HOME/.local/bin}"
DRY_RUN="${AGENT_INSTALL_DRY_RUN:-0}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: required command not found: $1" >&2
    exit 1
  }
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

target="$(detect_target)"
asset="ipa-rs-isolated-agent-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

echo "Target: $target"
echo "Release: $VERSION"
echo "Install dir: $INSTALL_DIR"

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
mkdir -p "$INSTALL_DIR"
if [ "$target" = "${target%pc-windows-msvc}" ]; then
  install -m 0755 "$payload_dir/bin/agentctl" "$INSTALL_DIR/agentctl"
  install -m 0755 "$payload_dir/bin/agent-forkd" "$INSTALL_DIR/agent-forkd"
else
  cp "$payload_dir/bin/agentctl.exe" "$INSTALL_DIR/agentctl.exe"
  cp "$payload_dir/bin/agent-forkd.exe" "$INSTALL_DIR/agent-forkd.exe"
fi

ensure_shell_path
case "$target" in
  *pc-windows-msvc) ensure_windows_path ;;
esac

echo "Installed agentctl and agent-forkd to $INSTALL_DIR"
echo "Restart your shell or run: export PATH=\"$INSTALL_DIR:\$PATH\""
