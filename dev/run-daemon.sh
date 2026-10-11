#!/usr/bin/env bash
# Runs the service from this repository in the foreground, as root, for
# testing: live updates, every local disk, the real socket. Ctrl+C stops it
# (the snapshot is saved). The log has numbers only, never file names.
#
# Build first (as yourself):  cargo build --release -p anythingd
# Then:                       sudo dev/run-daemon.sh
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(id -u)" -eq 0 ] || { echo "run with sudo" >&2; exit 1; }
binary=target/release/anythingd
[ -x "$binary" ] || { echo "build it first: cargo build --release -p anythingd" >&2; exit 1; }
log=/tmp/anythingd-dev.log
echo "log also in $log"
# tee -i ignores Ctrl+C, so the service gets to stop cleanly and say so.
"$binary" --socket /run/anything/anything.sock --database /var/lib/anything 2>&1 | tee -i "$log"
