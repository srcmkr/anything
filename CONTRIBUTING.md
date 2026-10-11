# Contributing

Bug reports, ideas, translations and pull requests are welcome. For bigger
changes, please open an issue first so we can talk about the approach.

## Reporting a bug

Open an [issue](https://github.com/srcmkr/anything/issues). *Settings → About →
Copy info* puts the version and system details on the clipboard, without any
file names; paste that into the issue.

## Building from source

You need [Rust](https://rustup.rs/) 1.90 or newer and a few libraries. On
Debian, Ubuntu and Linux Mint:

```sh
sudo apt install build-essential libxkbcommon-x11-dev libxcb1-dev \
    libwayland-dev libfontconfig-dev libvulkan1 bubblewrap
git clone https://github.com/srcmkr/anything
cd anything
cargo build --release
./target/release/anything --show
```

This creates three programs next to each other in `target/release`:
`anything` (the app), `anythingd` (the service) and `any` (the command line
client). Keep them together: *Settings → Service → Install* takes `anythingd`
from there.

`ANYTHING_DEMO=1` searches 5 million made-up entries instead of your disks.
For preview pictures in the grid, generate the demo images and videos once
with `dev/make-demo.sh` (needs ffmpeg) and search for `leuchtturm`.

Before a pull request, run both of these; they should pass without warnings:

```sh
cargo test
cargo clippy --all-targets
```

## Translations

All texts live in
[`crates/anything-gui/locales/app.yml`](crates/anything-gui/locales/app.yml),
the texts of the UI library in `components.yml` next to it. To fix a
translation, edit the line. To add a language, add a line with its code to
every entry and the language to `crates/anything-gui/src/i18n.rs`. Native
speakers checking the existing translations are very welcome.

## Where things are

| Path | Contents |
|---|---|
| [`crates/anything-core`](crates/anything-core) | The index: scanning, search, sorting, live updates, permissions, snapshots |
| [`crates/anything-gui`](crates/anything-gui) | The app: window, tray, list and grid, previews, settings, translations |
| [`crates/anythingd`](crates/anythingd) | The system service and its install, start, stop and update actions |
| [`crates/anything-proto`](crates/anything-proto) | Messages between the service and its clients |
| [`crates/any`](crates/any) | The command line client |
| [`crates/anything-update`](crates/anything-update) | Finding, verifying and unpacking releases; key and signing tools |
| [`packaging`](packaging) | systemd unit, service settings, polkit rules, menu entry, release key, package build |
| [`vendor`](vendor) | A patched copy of a dependency, with the reason in its README |
| [`.github/workflows`](.github/workflows) | Tests for every push, releases for every tag |
| [`dev`](dev) | Demo data, helper scripts, the release script |
| [`docs`](docs) | Performance, security, screenshots, background notes |

## Releases

`dev/release.sh 0.2.0` sets the version, runs the tests, commits, tags and
pushes. The tag starts the release workflow: it tests, builds on Ubuntu 22.04
(so the programs run on older systems too), packs the archive, the Debian
package and the AppImage with `packaging/build-packages.sh`, signs all three
and publishes them as the latest release.

The signing key is made once with
`cargo run -p anything-update --example release-key -- <secret key file>`.
Its public half goes into `packaging/minisign.pub` and is built into every
copy of Anything; the secret half goes into the repository secret
`MINISIGN_SECRET_KEY`. Keep a backup of the secret key: without it, installed
copies cannot verify new releases.
