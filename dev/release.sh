#!/usr/bin/env bash
# Starts a release: sets the version, commits, tags and pushes. GitHub
# Actions then tests, builds, packs, signs and publishes it
# (.github/workflows/release.yml).
#
# Usage: dev/release.sh 0.2.0
set -euo pipefail
cd "$(dirname "$0")/.."

version=${1:?usage: dev/release.sh VERSION (like 0.2.0)}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like 0.2.0" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "commit or stash your changes first" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/v$version" >/dev/null && { echo "tag v$version exists already" >&2; exit 1; }
[ -s packaging/minisign.pub ] || { echo "no release key yet, see crates/anything-update/examples/release-key.rs" >&2; exit 1; }

# The workspace version, the first "version =" in Cargo.toml.
sed -i "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
cargo update --workspace --quiet
cargo test --quiet
git add Cargo.toml Cargo.lock
git commit --quiet -m "Anything $version"
git tag -a "v$version" -m "Anything $version"
git push --quiet
git push --quiet origin "v$version"
echo "v$version pushed; the release workflow takes it from here"
