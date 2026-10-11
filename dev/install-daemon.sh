#!/usr/bin/env bash
# Installs the service from this repository as a systemd unit, or removes it.
# The same as the buttons under Settings → Service.
#
# Build first (as yourself):  cargo build --release -p anythingd
# Then:                       sudo dev/install-daemon.sh            (install or update)
#                             sudo dev/install-daemon.sh --remove   (uninstall, removes the snapshot)
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }

binary=target/release/anythingd
[ -x "$binary" ] || { echo "build it first: cargo build --release -p anythingd" >&2; exit 1; }
if [ "${1:-}" = "--remove" ]; then
    exec "$binary" --uninstall
fi
"$binary" --install
echo "installed; status: systemctl status anythingd   log: journalctl -u anythingd -f"
