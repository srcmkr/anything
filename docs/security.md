# Privacy and safety

File names and file contents are input that anyone can shape. Anything treats
them that way.

## Your file names stay on your machine

- The index lives on your disk: `~/.local/share/anything` for the app,
  `/var/lib/anything` (readable by root only) for the service.
- The only connection Anything makes is the update check (see below).
- The service checks every result against the asking user's permissions: you
  only see what you could list with `ls` yourself.
- *Copy info* in the settings, meant for bug reports, contains version and
  system details only, no file names.

## Previews are made in a sandbox

The programs that draw previews (thumbnailers) parse untrusted files. With
[bubblewrap](https://github.com/containers/bubblewrap) installed (the Debian
package recommends it), each runs without network, without your home folder,
and with only the one file it is meant to read. Anything only reads and writes
previews in its own folder, never in the desktop's shared thumbnail cache.

## Names are shown as they are

Control characters and invisible direction marks, which could make `fdp.exe`
look like `exe.pdf`, are shown as visible symbols. Names that are not valid
UTF-8 are indexed and opened correctly.

## Deleting is careful

*Move to Trash* uses the desktop's trash and can be undone from the note that
follows. Deleting for good always asks first, and says so when a whole folder
goes. A link is removed itself, never what it points to.

## The service does one thing

`anythingd` runs as root, locked down by systemd: it may read every folder but
write only its own state folder, has no network and cannot gain new
privileges. The password prompt (polkit) only allows fixed actions: install,
start, stop, restart, remove, update. None of them take paths or other input
from the caller.

## Updates are signed

Half a minute after the start and every six hours, Anything asks GitHub for
the latest release; GitHub sees your IP address, nothing else. *Settings →
About* switches the check off.

A new version is only offered, never installed on its own. *Install* asks for
the administrator password; then `anythingd --update` downloads the release
itself, checks its [minisign](https://jedisct1.github.io/minisign/) signature
against the key built into Anything, and only then installs it (through apt
for the Debian package). An AppImage checks the signature the same way before
replacing its own file.

A signature covers its file's name, so an older release cannot be passed off
as a newer one, and only a higher version is ever installed. No other program
can hand the update a file to install.
