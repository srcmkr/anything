//! The index on disk, so a restart does not have to scan again.
//!
//! Little-endian, versioned, written to a temporary file and renamed into
//! place. A `tag` chosen by the caller (e.g. a hash of the scan settings) is
//! stored too; loading with a different tag fails, so a snapshot made with
//! other settings is never used. Loading reads straight into the index's own
//! buffers, so it never needs memory for the whole file on top.

use std::{
    collections::HashMap,
    fs,
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
};

use crate::{Entry, Folder, Index, Names, Span, Store};

const MAGIC: &[u8; 8] = b"ANYIDX\0\0";
const VERSION: u32 = 1;
const ENTRY_BYTES: usize = 17;
const FOLDER_BYTES: usize = 18;
const SPAN_BYTES: usize = 11;

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("index snapshot: {what}"))
}

/// Writes fixed-size records through a reusable buffer.
fn write_records<T>(out: &mut impl Write, items: &[T], each: usize, encode: impl Fn(&T, &mut Vec<u8>)) -> io::Result<()> {
    let mut buf = Vec::with_capacity(each * 16 * 1024);
    for chunk in items.chunks(16 * 1024) {
        buf.clear();
        for item in chunk {
            encode(item, &mut buf);
        }
        out.write_all(&buf)?;
    }
    Ok(())
}

/// Reads `count` fixed-size records in chunks, straight into a vector.
fn read_records<T>(input: &mut impl Read, count: usize, each: usize, decode: impl Fn(&[u8]) -> T) -> io::Result<Vec<T>> {
    let mut out = Vec::with_capacity(count);
    let mut buf = vec![0u8; each * 16 * 1024];
    let mut left = count;
    while left > 0 {
        let take = left.min(16 * 1024);
        let bytes = &mut buf[..take * each];
        input.read_exact(bytes)?;
        out.extend(bytes.chunks_exact(each).map(&decode));
        left -= take;
    }
    Ok(out)
}

fn read_bytes(input: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; len];
    input.read_exact(&mut out)?;
    Ok(out)
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

impl Index {
    pub fn save(&self, path: &Path, tag: u64) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut out = BufWriter::with_capacity(1 << 20, fs::File::create(&tmp)?);
            out.write_all(MAGIC)?;
            out.write_all(&VERSION.to_le_bytes())?;
            out.write_all(&tag.to_le_bytes())?;
            let store = &self.store;
            let names = &self.names;
            for len in [
                store.entries.len(),
                store.folders.len(),
                store.big.len(),
                names.spans.len(),
                names.folded.len(),
                names.case.len(),
                names.original.len(),
                self.by_name.len(),
                self.by_path.len(),
                self.by_size.len(),
                self.by_modified.len(),
            ] {
                out.write_all(&(len as u64).to_le_bytes())?;
            }
            write_records(&mut out, &store.entries, ENTRY_BYTES, |e, buf| {
                buf.extend_from_slice(&e.parent.to_le_bytes());
                buf.extend_from_slice(&e.name.to_le_bytes());
                buf.extend_from_slice(&e.size.to_le_bytes());
                buf.extend_from_slice(&e.mtime.to_le_bytes());
                buf.push(e.flags);
            })?;
            write_records(&mut out, &store.folders, FOLDER_BYTES, |f, buf| {
                buf.extend_from_slice(&f.mtime_ns.to_le_bytes());
                buf.extend_from_slice(&f.uid.to_le_bytes());
                buf.extend_from_slice(&f.gid.to_le_bytes());
                buf.extend_from_slice(&f.mode.to_le_bytes());
            })?;
            let mut big: Vec<(u32, u64)> = store.big.iter().map(|(&k, &v)| (k, v)).collect();
            big.sort_unstable();
            write_records(&mut out, &big, 12, |(id, size), buf| {
                buf.extend_from_slice(&id.to_le_bytes());
                buf.extend_from_slice(&size.to_le_bytes());
            })?;
            write_records(&mut out, &names.spans, SPAN_BYTES, |s, buf| {
                buf.extend_from_slice(&s.folded.to_le_bytes());
                buf.extend_from_slice(&s.len.to_le_bytes());
                buf.push(s.kind);
                buf.extend_from_slice(&s.aux.to_le_bytes());
            })?;
            out.write_all(&names.folded)?;
            out.write_all(&names.case)?;
            out.write_all(&names.original)?;
            for order in [&self.by_name, &self.by_path, &self.by_size, &self.by_modified] {
                write_records(&mut out, order, 4, |id, buf| buf.extend_from_slice(&id.to_le_bytes()))?;
            }
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// Loads a snapshot saved with the same `tag`.
    pub fn load(path: &Path, tag: u64) -> io::Result<Index> {
        let file = fs::File::open(path)?;
        let file_len = file.metadata()?.len() as usize;
        let mut input = BufReader::with_capacity(1 << 20, file);
        let header = read_bytes(&mut input, 8 + 4 + 8 + 11 * 8).map_err(|_| bad("truncated"))?;
        if &header[..8] != MAGIC {
            return Err(bad("not an index"));
        }
        if u32_at(&header, 8) != VERSION {
            return Err(bad("other version"));
        }
        if u64_at(&header, 12) != tag {
            return Err(bad("made with other settings"));
        }
        let mut lens = [0usize; 11];
        for (i, len) in lens.iter_mut().enumerate() {
            *len = usize::try_from(u64_at(&header, 20 + i * 8)).map_err(|_| bad("too large"))?;
        }
        let [entries, folders, big, spans, folded, case, original, by_name, by_path, by_size, by_modified] = lens;
        // Refuse sizes the file cannot hold before allocating for them.
        let needed = entries
            .checked_mul(ENTRY_BYTES)
            .and_then(|n| n.checked_add(folders.checked_mul(FOLDER_BYTES)?))
            .and_then(|n| n.checked_add(big.checked_mul(12)?))
            .and_then(|n| n.checked_add(spans.checked_mul(SPAN_BYTES)?))
            .and_then(|n| n.checked_add(folded)?.checked_add(case)?.checked_add(original))
            .and_then(|n| n.checked_add((by_name + by_path + by_size + by_modified).checked_mul(4)?))
            .ok_or_else(|| bad("too large"))?;
        if needed + header.len() != file_len {
            return Err(bad("truncated or damaged"));
        }

        let map_err = |_| bad("truncated");
        let entries = read_records(&mut input, entries, ENTRY_BYTES, |b| Entry {
            parent: u32_at(b, 0),
            name: u32_at(b, 4),
            size: u32_at(b, 8),
            mtime: u32_at(b, 12),
            flags: b[16],
        })
        .map_err(map_err)?;
        let folders = read_records(&mut input, folders, FOLDER_BYTES, |b| Folder {
            mtime_ns: u64_at(b, 0) as i64,
            uid: u32_at(b, 8),
            gid: u32_at(b, 12),
            mode: u16_at(b, 16),
        })
        .map_err(map_err)?;
        let big: HashMap<u32, u64> =
            read_records(&mut input, big, 12, |b| (u32_at(b, 0), u64_at(b, 4))).map_err(map_err)?.into_iter().collect();
        let spans = read_records(&mut input, spans, SPAN_BYTES, |b| Span {
            folded: u32_at(b, 0),
            len: u16_at(b, 4),
            kind: b[6],
            aux: u32_at(b, 7),
        })
        .map_err(map_err)?;
        let names = Names {
            folded: read_bytes(&mut input, folded).map_err(map_err)?,
            case: read_bytes(&mut input, case).map_err(map_err)?,
            original: read_bytes(&mut input, original).map_err(map_err)?,
            spans,
        };
        let mut order = |len: usize| read_records(&mut input, len, 4, |b| u32_at(b, 0)).map_err(map_err);
        let (by_name, by_path, by_size, by_modified) = (order(by_name)?, order(by_path)?, order(by_size)?, order(by_modified)?);

        // Everything must point inside the snapshot, and names must be text.
        for s in &names.spans {
            let end = s.folded as usize + s.len as usize;
            if end > names.folded.len() || std::str::from_utf8(&names.folded[s.folded as usize..end]).is_err() {
                return Err(bad("corrupt name"));
            }
            let aux = s.aux as usize;
            match s.kind {
                crate::NAME_LOWER => {}
                crate::NAME_CASED if aux + (s.len as usize).div_ceil(8) <= names.case.len() => {}
                crate::NAME_ORIGINAL if aux + 2 <= names.original.len() => {
                    let len = u16_at(&names.original, aux) as usize;
                    let text = names.original.get(aux + 2..aux + 2 + len).ok_or_else(|| bad("corrupt name"))?;
                    if std::str::from_utf8(text).is_err() {
                        return Err(bad("corrupt name"));
                    }
                }
                _ => return Err(bad("corrupt name")),
            }
        }
        let n = entries.len();
        for (id, e) in entries.iter().enumerate() {
            let parent_ok = e.parent == crate::NO_PARENT || (e.parent as usize) < id;
            let folder_ok = e.flags & crate::FLAG_DIR == 0 || (e.size as usize) < folders.len();
            let big_ok = e.flags & crate::FLAG_BIG == 0 || big.contains_key(&(id as u32));
            if !parent_ok || !folder_ok || !big_ok || e.name as usize >= names.len() {
                return Err(bad("corrupt entry"));
            }
        }
        if [&by_name, &by_path, &by_size, &by_modified].iter().any(|o| o.iter().any(|&id| id as usize >= n)) {
            return Err(bad("corrupt order"));
        }
        let dead = entries.iter().filter(|e| e.flags & crate::FLAG_DEAD != 0).count();
        let index = Index {
            store: Store { entries, folders, big },
            names,
            by_name,
            by_path,
            by_size,
            by_modified,
            lookup: None,
            dead,
            tree: None,
        };
        crate::give_back_memory();
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Index, IndexBuilder, NO_PARENT, Query, SortKey, synthetic_index};

    #[test]
    fn round_trip() {
        let index = synthetic_index(20_000, 3);
        let path = std::env::temp_dir().join(format!("anything-snapshot-{}.idx", std::process::id()));
        index.save(&path, 42).unwrap();
        let loaded = Index::load(&path, 42).unwrap();
        for key in [SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Modified] {
            let q = Query::parse("rechnung *.pdf");
            assert_eq!(index.search(&q, key, false), loaded.search(&q, key, false));
        }
        let hit = loaded.search(&Query::parse("rechnung"), SortKey::Path, false)[0];
        assert_eq!(index.path(hit), loaded.path(hit));
        assert_eq!(index.modified(hit), loaded.modified(hit));
        // Other settings, or a damaged file, are refused.
        assert!(Index::load(&path, 43).is_err());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 10);
        std::fs::write(&path, bytes).unwrap();
        assert!(Index::load(&path, 42).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn keeps_spelling_big_files_and_owners() {
        let mut b = IndexBuilder::default();
        let root = b.push(NO_PARENT, "", true, 0, 0);
        let dir = b.push_folder_owned(root, "Ünïcode Ordner", 1_234_567_890_123, false, Some(crate::Owner { uid: 7, gid: 8, mode: 0o750, acl: true }));
        b.push_file(dir, "MixedCase.TXT", 5_000_000_000, 1_700_000_000_000_000_000);
        b.push_file(dir, "lower.txt", 3, 0);
        let index = b.build();
        let path = std::env::temp_dir().join(format!("anything-snapshot-names-{}.idx", std::process::id()));
        index.save(&path, 1).unwrap();
        let loaded = Index::load(&path, 1).unwrap();
        std::fs::remove_file(path).unwrap();
        let all = loaded.search(&Query::parse(""), SortKey::Path, false);
        let paths: Vec<String> = all.iter().map(|&id| loaded.path(id)).collect();
        assert_eq!(paths, ["/Ünïcode Ordner", "/Ünïcode Ordner/lower.txt", "/Ünïcode Ordner/MixedCase.TXT"]);
        assert_eq!(loaded.size(all[2]), 5_000_000_000);
        assert_eq!(loaded.modified(all[2]), 1_700_000_000);
        assert_eq!(loaded.owner(all[0]), Some(crate::Owner { uid: 7, gid: 8, mode: 0o750, acl: true }));
        assert_eq!(loaded.store.mtime_ns(all[0]), 1_234_567_890_123);
    }
}
