//! File names that are not valid UTF-8.
//!
//! Linux allows any bytes in a name but `/` and NUL; the index keeps text.
//! Names are carried as text losslessly: every byte that is not part of
//! valid UTF-8 becomes one of the 256 last code points of Unicode
//! (`U+10FF00` + byte, private use). So that this can always be undone, a
//! real name that already contains such a code point has its UTF-8 bytes
//! escaped the same way. Names without either stay exactly as they are.

use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

const BASE: u32 = 0x10FF00;

/// Whether `c` stands for a single byte of a name.
pub fn is_raw_byte(c: char) -> bool {
    c as u32 >= BASE
}

fn raw(byte: u8) -> char {
    char::from_u32(BASE + byte as u32).unwrap()
}

/// All code points from `U+100000` start with byte F4 in UTF-8.
fn may_have_raw(bytes: &[u8]) -> bool {
    memchr::memchr(0xF4, bytes).is_some()
}

/// A name (or path) as it is kept in the index.
pub fn from_os(name: &OsStr) -> Cow<'_, str> {
    let bytes = name.as_bytes();
    if let Ok(text) = std::str::from_utf8(bytes)
        && (!may_have_raw(bytes) || !text.chars().any(is_raw_byte))
    {
        return Cow::Borrowed(text);
    }
    let mut text = String::with_capacity(bytes.len() + 8);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if is_raw_byte(c) {
                let mut buf = [0; 4];
                text.extend(c.encode_utf8(&mut buf).bytes().map(raw));
            } else {
                text.push(c);
            }
        }
        text.extend(chunk.invalid().iter().copied().map(raw));
    }
    Cow::Owned(text)
}

/// The name (or path) on disk for text from the index.
pub fn to_os(text: &str) -> Cow<'_, OsStr> {
    if !may_have_raw(text.as_bytes()) || !text.chars().any(is_raw_byte) {
        return Cow::Borrowed(OsStr::new(text));
    }
    let mut bytes = Vec::with_capacity(text.len());
    for c in text.chars() {
        if is_raw_byte(c) {
            bytes.push((c as u32 - BASE) as u8);
        } else {
            let mut buf = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    Cow::Owned(OsString::from_vec(bytes))
}

/// A path from the index as a path on disk.
pub fn to_path(text: &str) -> PathBuf {
    PathBuf::from(to_os(text).into_owned())
}

/// A path on disk as text for the index.
pub fn path_text(path: &Path) -> Cow<'_, str> {
    from_os(path.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8]) -> String {
        let name = OsStr::from_bytes(bytes);
        let text = from_os(name).into_owned();
        assert_eq!(to_os(&text).as_bytes(), bytes, "round trip of {bytes:?}");
        text
    }

    #[test]
    fn keeps_every_name() {
        assert_eq!(round_trip(b"Rechnung.pdf"), "Rechnung.pdf");
        assert_eq!(round_trip("Grüße.txt".as_bytes()), "Grüße.txt");
        // Latin-1 "Grüße", as old archives create it.
        let text = round_trip(b"Gr\xfc\xdfe.txt");
        assert_eq!(text.chars().filter(|&c| is_raw_byte(c)).count(), 2);
        assert!(text.starts_with("Gr") && text.ends_with("e.txt"));
        // A real name using the reserved code points is escaped, not mixed up.
        let real = "a\u{10FF41}b";
        let text = round_trip(real.as_bytes());
        assert_ne!(text, real);
        assert_ne!(to_os(&text).as_bytes(), b"aAb");
        round_trip(b"\xf4\x8f\xbd");
        round_trip(b"\xff\xfe\x00x");
        for byte in 0..=255u8 {
            round_trip(&[b'x', byte, b'y']);
        }
    }
}
