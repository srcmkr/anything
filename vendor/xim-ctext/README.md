# xim-ctext 0.3.0, patched

A copy of [xim-ctext](https://github.com/Riey/xim-rs) 0.3.0 (MIT, see
`LICENSE`), used through `[patch.crates-io]` in the workspace `Cargo.toml`.

The one change: compound text without escape sequences is ISO 8859-1
(Latin-1) by the X11 standard, not UTF-8. Input methods such as ibus send
characters like `ü` that way; the original turned that into an error, which
GPUI's XIM client (`zed-xim`) answers with a panic, closing the program on the
first umlaut typed. Now such text is taken as UTF-8 if it is valid UTF-8 (some
input methods send that), and as Latin-1 otherwise.

Drop this copy once a fixed release of xim-ctext is used.
