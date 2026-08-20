#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/release/nost"
MODE="system"
DO_BUILD=1

usage() {
    cat <<'USAGE'
Usage: ./install.sh [--system|--user] [--no-build]

Install nost as both:
  • the `nost` command in your shell
  • a desktop application named "nost GPU Monitor"

Modes:
  --system    Install to /usr/local/bin and /usr/local/share/applications.
              This is the default and uses sudo when needed.
  --user      Install only for the current user under ~/.local.
  --no-build  Install an already-built target/release/nost.
  -h, --help  Show this help.
USAGE
}

while (($#)); do
    case "$1" in
        --system) MODE="system" ;;
        --user) MODE="user" ;;
        --no-build) DO_BUILD=0 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "nost install: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

if (( DO_BUILD )); then
    "$HERE/build.sh"
fi

if [[ ! -x "$BIN" ]]; then
    echo "nost install: missing release binary: $BIN" >&2
    echo "Run ./build.sh first, or omit --no-build." >&2
    exit 1
fi

run_root() {
    if [[ "$(id -u)" -eq 0 ]]; then
        "$@"
    elif command -v sudo >/dev/null 2>&1; then
        sudo "$@"
    else
        echo "nost install: system install needs root privileges and sudo is unavailable." >&2
        echo "Use ./install.sh --user for a per-user install." >&2
        exit 1
    fi
}

if [[ "$MODE" == "system" ]]; then
    PREFIX="/usr/local"
    APP_DIR="/usr/local/share/applications"
    INSTALL_CMD=(run_root install)
else
    PREFIX="${HOME}/.local"
    APP_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/applications"
    INSTALL_CMD=(install)
fi

CLI_PATH="$PREFIX/bin/nost"
DESKTOP_PATH="$APP_DIR/nost.desktop"
TMP_DESKTOP="$(mktemp)"
trap 'rm -f "$TMP_DESKTOP"' EXIT

cat > "$TMP_DESKTOP" <<EOF_DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=nost GPU Monitor
GenericName=NVIDIA GPU Monitor
Comment=Minimal direct-NVML NVIDIA GPU monitor
Exec=$CLI_PATH
TryExec=$CLI_PATH
Icon=utilities-system-monitor
Terminal=true
Categories=System;Monitor;
Keywords=NVIDIA;GPU;NVML;VRAM;monitor;
StartupNotify=false
EOF_DESKTOP

"${INSTALL_CMD[@]}" -Dm755 "$BIN" "$CLI_PATH"
"${INSTALL_CMD[@]}" -Dm644 "$TMP_DESKTOP" "$DESKTOP_PATH"

# Refresh the desktop application cache when the host provides the freedesktop helper.
if command -v update-desktop-database >/dev/null 2>&1; then
    if [[ "$MODE" == "system" ]]; then
        run_root update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    else
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
fi

printf '\nnost installed successfully.\n'
printf '  CLI: %s\n' "$CLI_PATH"
printf '  App: %s\n' "$DESKTOP_PATH"

if [[ "$MODE" == "user" && ":$PATH:" != *":$PREFIX/bin:"* ]]; then
    printf '\n%s is not currently in PATH.\n' "$PREFIX/bin"
    printf 'Add this to your shell startup file, then open a new terminal:\n\n'
    printf '  export PATH="%s/bin:$PATH"\n' "$PREFIX"
else
    printf '\nRun from any terminal with:\n\n  nost\n'
fi

printf '\nYou can also launch "nost GPU Monitor" from your desktop application menu.\n'
