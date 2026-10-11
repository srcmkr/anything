//! In-memory file name index and search.
//!
//! Entries point into a table of distinct names; four sort orders are kept
//! ready so any sorted result is a filtered copy of one of them.
//!
//! Memory per entry is kept small: 20 bytes for the entry itself, folders'
//! details (exact time, owner, mode) in a table of their own, the rare file
//! over 4 GiB in a side map, and names stored once, lower-cased for
//! searching, with the capital letters as a bit mask.

pub mod access;
pub mod name;
mod query;
pub mod update;
pub mod scan;
mod snapshot;
mod scope;
mod synthetic;

use std::{borrow::Cow, collections::HashMap, hash::BuildHasher as _};

use rayon::prelude::*;

pub use query::Query;
pub use update::{Patch, UpdateStats};
pub use scope::{CACHES, NODE_MODULES, Scope, exclude_is_outside, excludes, tag as scope_tag};
pub use synthetic::{RealEntry, synthetic_index, synthetic_index_with};

/// Sentinel parent of the root entry.
pub const NO_PARENT: u32 = u32::MAX;

const FLAG_DIR: u8 = 1;
/// Kept only so paths below it can be built; never a search result.
const FLAG_CONTEXT: u8 = 2;
/// The folder's owner, group and mode are known (see [`Owner`]).
const FLAG_OWNER: u8 = 4;
/// The folder has a POSIX access ACL.
const FLAG_ACL: u8 = 8;
/// Removed by an in-place update; kept so ids stay stable until the next
/// full rebuild. Never a result.
const FLAG_DEAD: u8 = 16;
/// The file is 4 GiB or larger; its size is in [`Store::big`].
const FLAG_BIG: u8 = 32;

/// Who owns a folder and who may do what in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    /// Permission bits (`0o7777`).
    pub mode: u16,
    /// Has a POSIX ACL, so the mode bits alone do not tell the whole story.
    pub acl: bool,
}

const NANOS: i64 = 1_000_000_000;

/// 20 bytes.
#[derive(Clone, Copy, Debug)]
struct Entry {
    parent: u32,
    /// Id in the name table.
    name: u32,
    /// Files: size in bytes (see [`FLAG_BIG`]). Folders: slot in
    /// [`Store::folders`].
    size: u32,
    /// Seconds since the Unix epoch, for showing and sorting.
    mtime: u32,
    flags: u8,
}

/// What only folders have.
#[derive(Clone, Copy, Debug, Default)]
struct Folder {
    /// Exact modification time in nanoseconds, to notice changes.
    mtime_ns: i64,
    uid: u32,
    gid: u32,
    mode: u16,
}

/// Entries and what belongs to them, shared by [`IndexBuilder`] and
/// [`Index`] so both add entries the same way.
#[derive(Clone, Default)]
struct Store {
    entries: Vec<Entry>,
    folders: Vec<Folder>,
    big: HashMap<u32, u64>,
}

fn seconds(mtime_ns: i64) -> u32 {
    mtime_ns.div_euclid(NANOS).clamp(0, u32::MAX as i64) as u32
}

impl Store {
    fn push(&mut self, parent: u32, name: u32, flags: u8, size: u64, mtime_ns: i64, owner: Option<Owner>) -> u32 {
        let id = self.entries.len() as u32;
        let mut entry = Entry { parent, name, size: 0, mtime: seconds(mtime_ns), flags: flags & !(FLAG_BIG | FLAG_OWNER | FLAG_ACL) };
        if flags & FLAG_DIR != 0 {
            entry.size = self.folders.len() as u32;
            self.folders.push(Folder { mtime_ns, ..Default::default() });
        } else if size > u32::MAX as u64 {
            entry.flags |= FLAG_BIG;
            self.big.insert(id, size);
        } else {
            entry.size = size as u32;
        }
        self.entries.push(entry);
        if let Some(owner) = owner {
            self.set_owner(id, owner);
        }
        id
    }

    fn is_dir(&self, id: u32) -> bool {
        self.entries[id as usize].flags & FLAG_DIR != 0
    }

    fn folder(&self, id: u32) -> Option<&Folder> {
        let e = &self.entries[id as usize];
        (e.flags & FLAG_DIR != 0).then(|| &self.folders[e.size as usize])
    }

    fn size(&self, id: u32) -> u64 {
        let e = &self.entries[id as usize];
        if e.flags & FLAG_DIR != 0 {
            0
        } else if e.flags & FLAG_BIG != 0 {
            self.big.get(&id).copied().unwrap_or(0)
        } else {
            e.size as u64
        }
    }

    /// Nanoseconds: exact for folders, whole seconds for files.
    fn mtime_ns(&self, id: u32) -> i64 {
        match self.folder(id) {
            Some(f) => f.mtime_ns,
            None => self.entries[id as usize].mtime as i64 * NANOS,
        }
    }

    fn owner(&self, id: u32) -> Option<Owner> {
        let e = &self.entries[id as usize];
        if e.flags & (FLAG_DIR | FLAG_OWNER) != FLAG_DIR | FLAG_OWNER {
            return None;
        }
        let f = &self.folders[e.size as usize];
        Some(Owner { uid: f.uid, gid: f.gid, mode: f.mode, acl: e.flags & FLAG_ACL != 0 })
    }

    fn set_owner(&mut self, id: u32, owner: Owner) {
        let e = &mut self.entries[id as usize];
        if e.flags & FLAG_DIR == 0 {
            return;
        }
        e.flags |= FLAG_OWNER;
        e.flags = if owner.acl { e.flags | FLAG_ACL } else { e.flags & !FLAG_ACL };
        let f = &mut self.folders[e.size as usize];
        (f.uid, f.gid, f.mode) = (owner.uid, owner.gid, owner.mode);
    }

    fn set_file(&mut self, id: u32, size: u64, mtime_ns: i64) {
        let e = &mut self.entries[id as usize];
        e.mtime = seconds(mtime_ns);
        if size > u32::MAX as u64 {
            e.flags |= FLAG_BIG;
            self.big.insert(id, size);
        } else {
            e.flags &= !FLAG_BIG;
            e.size = size as u32;
            self.big.remove(&id);
        }
    }

    fn set_folder_mtime(&mut self, id: u32, mtime_ns: i64) {
        let e = &mut self.entries[id as usize];
        e.mtime = seconds(mtime_ns);
        let slot = e.size as usize;
        self.folders[slot].mtime_ns = mtime_ns;
    }

    /// Copies entry `id` of `from` (whose names this store shares).
    fn copy(&mut self, parent: u32, from: &Store, id: u32, mtime_ns: Option<i64>) -> u32 {
        let e = from.entries[id as usize];
        let mtime = mtime_ns.unwrap_or_else(|| from.mtime_ns(id));
        self.push(parent, e.name, e.flags, from.size(id), mtime, from.owner(id))
    }

    fn shrink(&mut self) {
        self.entries.shrink_to_fit();
        self.folders.shrink_to_fit();
        self.big.shrink_to_fit();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

/// How a name is stored.
const NAME_LOWER: u8 = 0;
/// ASCII with capitals: lower-cased text plus one bit per byte in `case`.
const NAME_CASED: u8 = 1;
/// Other text whose lower-case form differs: the original is in `original`.
const NAME_ORIGINAL: u8 = 2;

#[derive(Clone, Copy, Debug)]
struct Span {
    folded: u32,
    len: u16,
    kind: u8,
    /// Offset into `case` or `original`, depending on `kind`.
    aux: u32,
}

/// Every distinct name once, lower-cased for searching. On a typical disk
/// there are less than half as many names as entries, and nearly all are
/// ASCII, so the original spelling costs one bit per letter.
#[derive(Clone, Default)]
struct Names {
    folded: Vec<u8>,
    case: Vec<u8>,
    /// Length (u16) and bytes of names that are neither of the above.
    original: Vec<u8>,
    spans: Vec<Span>,
}

impl Names {
    fn len(&self) -> usize {
        self.spans.len()
    }

    fn folded(&self, id: u32) -> &[u8] {
        let s = self.spans[id as usize];
        &self.folded[s.folded as usize..s.folded as usize + s.len as usize]
    }

    fn folded_str(&self, id: u32) -> &str {
        // Only ever filled from &str (or checked when loaded).
        unsafe { std::str::from_utf8_unchecked(self.folded(id)) }
    }

    fn original(&self, at: usize) -> &str {
        let len = u16::from_le_bytes([self.original[at], self.original[at + 1]]) as usize;
        unsafe { std::str::from_utf8_unchecked(&self.original[at + 2..at + 2 + len]) }
    }

    /// The name as written.
    fn text(&self, id: u32) -> Cow<'_, str> {
        let s = self.spans[id as usize];
        match s.kind {
            NAME_CASED => {
                let mut out = String::with_capacity(s.len as usize);
                self.write_cased(s, &mut out);
                Cow::Owned(out)
            }
            NAME_ORIGINAL => Cow::Borrowed(self.original(s.aux as usize)),
            _ => Cow::Borrowed(self.folded_str(id)),
        }
    }

    fn write_cased(&self, s: Span, out: &mut String) {
        let folded = &self.folded[s.folded as usize..s.folded as usize + s.len as usize];
        let bits = &self.case[s.aux as usize..];
        out.extend(folded.iter().enumerate().map(|(i, &b)| {
            if bits[i / 8] & (1 << (i % 8)) != 0 { b.to_ascii_uppercase() as char } else { b as char }
        }));
    }

    /// Appends the name as written, without an allocation of its own.
    fn write_text(&self, id: u32, out: &mut String) {
        let s = self.spans[id as usize];
        match s.kind {
            NAME_CASED => self.write_cased(s, out),
            NAME_ORIGINAL => out.push_str(self.original(s.aux as usize)),
            _ => out.push_str(self.folded_str(id)),
        }
    }

    /// Whether name `id` is spelled exactly `name`.
    fn is(&self, id: u32, name: &str) -> bool {
        let s = self.spans[id as usize];
        match s.kind {
            NAME_CASED => {
                let bits = &self.case[s.aux as usize..];
                name.len() == s.len as usize
                    && name.bytes().zip(self.folded(id)).enumerate().all(|(i, (c, &f))| {
                        let upper = bits[i / 8] & (1 << (i % 8)) != 0;
                        c.to_ascii_lowercase() == f && c.is_ascii_uppercase() == upper
                    })
            }
            NAME_ORIGINAL => self.original(s.aux as usize) == name,
            _ => self.folded_str(id) == name,
        }
    }

    fn add(&mut self, name: &str) -> u32 {
        let folded = name.to_lowercase();
        let id = self.spans.len() as u32;
        let (kind, aux) = if folded == name {
            (NAME_LOWER, 0)
        } else if name.is_ascii() {
            let aux = self.case.len() as u32;
            let mut bits = vec![0u8; name.len().div_ceil(8)];
            for (i, b) in name.bytes().enumerate() {
                if b.is_ascii_uppercase() {
                    bits[i / 8] |= 1 << (i % 8);
                }
            }
            self.case.extend_from_slice(&bits);
            (NAME_CASED, aux)
        } else {
            let aux = self.original.len() as u32;
            self.original.extend_from_slice(&(name.len() as u16).to_le_bytes());
            self.original.extend_from_slice(name.as_bytes());
            (NAME_ORIGINAL, aux)
        };
        self.spans.push(Span { folded: self.folded.len() as u32, len: folded.len() as u16, kind, aux });
        self.folded.extend_from_slice(folded.as_bytes());
        id
    }

    fn shrink(&mut self) {
        self.folded.shrink_to_fit();
        self.case.shrink_to_fit();
        self.original.shrink_to_fit();
        self.spans.shrink_to_fit();
    }

    fn heap_bytes(&self) -> usize {
        self.folded.capacity() + self.case.capacity() + self.original.capacity() + self.spans.capacity() * std::mem::size_of::<Span>()
    }
}

/// Finds name ids by their text; `hash_one(text)` is the key.
struct Lookup {
    table: hashbrown::HashTable<u32>,
    hasher: hashbrown::DefaultHashBuilder,
}

impl Default for Lookup {
    fn default() -> Self {
        Lookup { table: hashbrown::HashTable::new(), hasher: Default::default() }
    }
}

impl Lookup {
    fn of(names: &Names) -> Lookup {
        let mut lookup = Lookup { table: hashbrown::HashTable::with_capacity(names.len()), ..Lookup::default() };
        for id in 0..names.len() as u32 {
            let hash = lookup.hasher.hash_one(names.text(id).as_ref());
            let (hasher, table) = (&lookup.hasher, &mut lookup.table);
            table.insert_unique(hash, id, |&other| hasher.hash_one(names.text(other).as_ref()));
        }
        lookup
    }

    fn intern(&mut self, names: &mut Names, name: &str) -> u32 {
        let hash = self.hasher.hash_one(name);
        if let Some(&id) = self.table.find(hash, |&id| names.is(id, name)) {
            return id;
        }
        let id = names.add(name);
        let (hasher, names) = (&self.hasher, &*names);
        self.table.insert_unique(hash, id, |&other| hasher.hash_one(names.text(other).as_ref()));
        id
    }
}

#[derive(Default)]
pub struct IndexBuilder {
    store: Store,
    names: Names,
    lookup: Lookup,
}

impl IndexBuilder {
    pub fn with_capacity(entries: usize) -> Self {
        Self { store: Store { entries: Vec::with_capacity(entries), ..Store::default() }, ..Self::default() }
    }

    /// A builder that already knows every name of `index`, so entries can be
    /// copied over by name id without hashing their names again.
    fn reusing_names(index: &Index) -> Self {
        let mut builder = Self::with_capacity(index.store.entries.len());
        builder.names = index.names.clone();
        builder.lookup = Lookup::of(&builder.names);
        builder
    }

    pub fn len(&self) -> usize {
        self.store.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.entries.is_empty()
    }

    /// Adds an entry and returns its id. `parent` must already exist, except
    /// for the root which passes [`NO_PARENT`]. `mtime` is in seconds.
    pub fn push(&mut self, parent: u32, name: &str, is_dir: bool, size: u64, mtime: i64) -> u32 {
        let flags = if is_dir { FLAG_DIR } else { 0 };
        self.push_entry(parent, name, flags, size, mtime.saturating_mul(NANOS), None)
    }

    /// Adds a folder; `context` folders are kept only for the paths below
    /// them and never show up as results. `mtime_ns` in nanoseconds.
    pub fn push_folder(&mut self, parent: u32, name: &str, mtime_ns: i64, context: bool) -> u32 {
        self.push_folder_owned(parent, name, mtime_ns, context, None)
    }

    /// Like [`Self::push_folder`], remembering who owns the folder.
    pub fn push_folder_owned(&mut self, parent: u32, name: &str, mtime_ns: i64, context: bool, owner: Option<Owner>) -> u32 {
        let flags = FLAG_DIR | if context { FLAG_CONTEXT } else { 0 };
        self.push_entry(parent, name, flags, 0, mtime_ns, owner)
    }

    /// Adds a file with its modification time in nanoseconds.
    pub fn push_file(&mut self, parent: u32, name: &str, size: u64, mtime_ns: i64) -> u32 {
        self.push_entry(parent, name, 0, size, mtime_ns, None)
    }

    fn push_entry(&mut self, parent: u32, name: &str, flags: u8, size: u64, mtime_ns: i64, owner: Option<Owner>) -> u32 {
        let name = self.lookup.intern(&mut self.names, name);
        self.store.push(parent, name, flags, size, mtime_ns, owner)
    }

    pub fn build(mut self) -> Index {
        // The buffers were sized by guess; give back what is unused.
        self.store.shrink();
        self.names.shrink();
        let mut index = Index {
            store: self.store,
            names: self.names,
            by_name: Vec::new(),
            by_path: Vec::new(),
            by_size: Vec::new(),
            by_modified: Vec::new(),
            lookup: Some(self.lookup),
            dead: 0,
            tree: None,
        };
        index.build_orders();
        index
    }
}

/// A search result as a bit per position in one of the sort orders; see
/// [`Index::select`]. Valid until the index changes.
pub struct Selection {
    key: SortKey,
    descending: bool,
    bits: Vec<u64>,
    /// Set bits in all words before each word.
    before: Vec<u32>,
    total: usize,
}

impl Selection {
    pub fn len(&self) -> usize {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

pub struct Index {
    store: Store,
    names: Names,
    by_name: Vec<u32>,
    by_path: Vec<u32>,
    by_size: Vec<u32>,
    by_modified: Vec<u32>,
    /// Name lookup for adding entries in place; built when first needed.
    lookup: Option<Lookup>,
    /// Entries marked [`FLAG_DEAD`].
    dead: usize,
    /// Children of every folder, for in-place updates; see [`Index::prepare`].
    tree: Option<Tree>,
}

/// Children by folder: a compact list for the entries an index was built
/// with (by folder slot), plus lists for children appended later.
struct Tree {
    start: Vec<u32>,
    children: Vec<u32>,
    extra: HashMap<u32, Vec<u32>>,
}

impl Tree {
    fn of<'a>(&'a self, store: &Store, id: u32) -> impl Iterator<Item = u32> + 'a {
        let built = if store.is_dir(id) {
            let slot = store.entries[id as usize].size as usize;
            match (self.start.get(slot), self.start.get(slot + 1)) {
                (Some(&a), Some(&b)) => &self.children[a as usize..b as usize],
                _ => &[][..],
            }
        } else {
            &[][..]
        };
        built.iter().copied().chain(self.extra.get(&id).into_iter().flatten().copied())
    }
}

/// Gives freed memory back to the system. After a scan or rebuild the
/// allocator otherwise keeps hundreds of megabytes it no longer uses.
pub fn give_back_memory() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

impl Index {
    /// An index holding only the root, e.g. before the first scan.
    pub fn empty() -> Index {
        let mut b = IndexBuilder::default();
        b.push(NO_PARENT, "", true, 0, 0);
        b.build()
    }

    pub fn len(&self) -> usize {
        self.store.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.entries.is_empty()
    }

    /// The name as written; borrowed where it is stored that way.
    pub fn name(&self, id: u32) -> Cow<'_, str> {
        self.names.text(self.store.entries[id as usize].name)
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.store.is_dir(id)
    }

    /// Size in bytes; 0 for folders.
    pub fn size(&self, id: u32) -> u64 {
        self.store.size(id)
    }

    /// Owner, group and mode of a folder, if they were recorded.
    pub fn owner(&self, id: u32) -> Option<Owner> {
        self.store.owner(id)
    }

    /// Modification time in seconds since the Unix epoch.
    pub fn modified(&self, id: u32) -> i64 {
        self.store.entries[id as usize].mtime as i64
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        let parent = self.store.entries[id as usize].parent;
        (parent != NO_PARENT).then_some(parent)
    }

    /// Full path of the directory containing `id`.
    pub fn dir_path(&self, id: u32) -> String {
        match self.parent(id) {
            Some(parent) => self.path(parent),
            None => String::new(),
        }
    }

    /// Full path of `id`.
    pub fn path(&self, id: u32) -> String {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if self.store.entries[c as usize].parent != NO_PARENT {
                chain.push(c);
            }
            cur = self.parent(c);
        }
        let mut path = String::new();
        for &c in chain.iter().rev() {
            path.push('/');
            self.names.write_text(self.store.entries[c as usize].name, &mut path);
        }
        if path.is_empty() {
            path.push('/');
        }
        path
    }

    fn order(&self, key: SortKey) -> &[u32] {
        match key {
            SortKey::Name => &self.by_name,
            SortKey::Path => &self.by_path,
            SortKey::Size => &self.by_size,
            SortKey::Modified => &self.by_modified,
        }
    }

    /// Bit per name id: set if the name matches `query`.
    fn matching_names(&self, query: &Query) -> Vec<u64> {
        const CHUNK: usize = 64 * 1024;
        let names = self.names.len();
        let mut bitmap = vec![0u64; names.div_ceil(64)];
        bitmap.par_chunks_mut(CHUNK / 64).enumerate().for_each(|(chunk, words)| {
            let base = chunk * CHUNK;
            for (w, word) in words.iter_mut().enumerate() {
                for bit in 0..64 {
                    let id = base + w * 64 + bit;
                    if id < names && query.matches(self.names.folded(id as u32)) {
                        *word |= 1 << bit;
                    }
                }
            }
        });
        bitmap
    }

    /// Returns the ids matching `query` in the requested order.
    pub fn search(&self, query: &Query, key: SortKey, descending: bool) -> Vec<u32> {
        let order = self.order(key);
        let mut hits: Vec<u32> = if query.is_empty() {
            order.to_vec()
        } else {
            // Match every distinct name once, reading the name buffer
            // sequentially, then pick the entries out of the sort order
            // through a bitmap over name ids.
            let bitmap = self.matching_names(query);
            order
                .par_chunks(64 * 1024)
                .map(|chunk| {
                    chunk
                        .iter()
                        .copied()
                        .filter(|&id| {
                            let name = self.store.entries[id as usize].name;
                            bitmap[name as usize / 64] & (1 << (name % 64)) != 0
                        })
                        .collect::<Vec<u32>>()
                })
                .collect::<Vec<_>>()
                .concat()
        };
        if descending {
            hits.reverse();
        }
        hits
    }

    /// Like [`Self::search`], but keeps the result as one bit per position
    /// in the sort order instead of a list of ids, and leaves out entries
    /// not set in `mask` (see [`access::mask`]). For results that are a
    /// large part of the index that is far smaller: 1 bit per entry instead
    /// of 4 bytes per hit.
    pub fn select(&self, query: &Query, key: SortKey, descending: bool, mask: Option<&[u64]>) -> Selection {
        const CHUNK: usize = 64 * 1024;
        let order = self.order(key);
        let names = (!query.is_empty()).then(|| self.matching_names(query));
        let has = |bits: &[u64], i: u32| bits.get(i as usize / 64).is_some_and(|w| w & (1 << (i % 64)) != 0);
        let mut bits = vec![0u64; order.len().div_ceil(64)];
        bits.par_chunks_mut(CHUNK / 64).enumerate().for_each(|(chunk, words)| {
            let base = chunk * CHUNK;
            for (w, word) in words.iter_mut().enumerate() {
                for bit in 0..64 {
                    let Some(&id) = order.get(base + w * 64 + bit) else { break };
                    let name_ok = names.as_ref().is_none_or(|n| has(n, self.store.entries[id as usize].name));
                    if name_ok && mask.is_none_or(|m| has(m, id)) {
                        *word |= 1 << bit;
                    }
                }
            }
        });
        let mut before = Vec::with_capacity(bits.len());
        let mut total = 0u32;
        for word in &bits {
            before.push(total);
            total += word.count_ones();
        }
        Selection { key, descending, bits, before, total: total as usize }
    }

    /// Rows `start..start + count` of a selection, as ids.
    pub fn selected(&self, selection: &Selection, start: usize, count: usize) -> Vec<u32> {
        let total = selection.total;
        let start = start.min(total);
        let count = count.min(total - start);
        if count == 0 {
            return Vec::new();
        }
        // Rank in ascending order of the first wanted row.
        let first = if selection.descending { total - start - count } else { start };
        let order = self.order(selection.key);
        let word = selection.before.partition_point(|&b| b as usize <= first) - 1;
        let mut skip = first - selection.before[word] as usize;
        let mut out = Vec::with_capacity(count);
        'words: for (w, &bits) in selection.bits.iter().enumerate().skip(word) {
            let mut bits = bits;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                out.push(order[w * 64 + bit]);
                if out.len() == count {
                    break 'words;
                }
            }
        }
        if selection.descending {
            out.reverse();
        }
        out
    }

    fn build_orders(&mut self) {
        let n = self.store.entries.len();
        let entries = &self.store.entries;

        // Sort the distinct names once; entries then sort by number.
        let mut name_order: Vec<u32> = (0..self.names.len() as u32).collect();
        name_order.par_sort_unstable_by(|&a, &b| self.names.folded(a).cmp(self.names.folded(b)).then(a.cmp(&b)));
        let mut name_rank = vec![0u32; self.names.len()];
        for (rank, &name) in name_order.iter().enumerate() {
            name_rank[name as usize] = rank as u32;
        }
        drop(name_order);

        // Every entry below the root, in name order; the path order needs
        // all of them, including folders kept only for their paths.
        let mut all_by_name: Vec<u32> = (0..n as u32).filter(|&id| entries[id as usize].parent != NO_PARENT).collect();
        all_by_name.par_sort_unstable_by_key(|&id| (name_rank[entries[id as usize].name as usize], id));
        drop(name_rank);
        // The root has no name of its own and is never a search result, nor
        // are folders kept only for their paths.
        let by_name: Vec<u32> = all_by_name.iter().copied().filter(|&id| self.is_result(id)).collect();

        // Rank in name order breaks ties for size and date.
        let mut rank = vec![0u32; n];
        for (r, &id) in by_name.iter().enumerate() {
            rank[id as usize] = r as u32;
        }

        let mut by_size = by_name.clone();
        by_size.par_sort_unstable_by_key(|&id| (self.store.size(id), rank[id as usize]));

        let mut by_modified = by_name.clone();
        by_modified.par_sort_unstable_by_key(|&id| (entries[id as usize].mtime, rank[id as usize]));
        drop(rank);

        self.by_path = self.path_order(&all_by_name);
        self.by_name = by_name;
        self.by_size = by_size;
        self.by_modified = by_modified;
    }

    /// For each entry, where its children start in the returned list.
    fn children(&self, in_order: &[u32]) -> (Vec<u32>, Vec<u32>) {
        let n = self.store.entries.len();
        let mut child_start = vec![0u32; n + 1];
        for e in &self.store.entries {
            if e.parent != NO_PARENT {
                child_start[e.parent as usize + 1] += 1;
            }
        }
        for i in 0..n {
            child_start[i + 1] += child_start[i];
        }
        let mut fill = child_start.clone();
        let mut children = vec![0u32; child_start[n] as usize];
        for &id in in_order {
            let parent = self.store.entries[id as usize].parent;
            if parent != NO_PARENT {
                children[fill[parent as usize] as usize] = id;
                fill[parent as usize] += 1;
            }
        }
        (child_start, children)
    }

    /// Depth-first walk with children in name order, which equals sorting by
    /// full path.
    fn path_order(&self, by_name: &[u32]) -> Vec<u32> {
        let n = self.store.entries.len();
        // Filling in name order leaves every child list sorted by name.
        let (child_start, children) = self.children(by_name);
        let mut order = Vec::with_capacity(self.by_name.len().max(n / 2));
        let mut stack: Vec<u32> = (0..n as u32).filter(|&id| self.store.entries[id as usize].parent == NO_PARENT).collect();
        while let Some(id) = stack.pop() {
            if self.is_result(id) {
                order.push(id);
            }
            let (start, end) = (child_start[id as usize] as usize, child_start[id as usize + 1] as usize);
            stack.extend(children[start..end].iter().rev());
        }
        order.shrink_to_fit();
        order
    }

    /// Number of entries that can show up as results.
    pub fn searchable_len(&self) -> usize {
        self.by_name.len()
    }

    fn is_result(&self, id: u32) -> bool {
        let e = &self.store.entries[id as usize];
        e.parent != NO_PARENT && e.flags & (FLAG_CONTEXT | FLAG_DEAD) == 0
    }

    fn is_dead(&self, id: u32) -> bool {
        self.store.entries[id as usize].flags & FLAG_DEAD != 0
    }

    fn flags(&self, id: u32) -> u8 {
        self.store.entries[id as usize].flags
    }

    /// Share of entries removed in place since the last full build.
    pub fn dead_share(&self) -> f64 {
        self.dead as f64 / self.store.entries.len().max(1) as f64
    }

    /// A copy holding only what `scope` covers, plus the folders leading to it.
    /// Ids are not stable across this.
    pub fn restrict(&self, scope: &Scope) -> Index {
        let cover = access::coverage(self, scope);
        let n = self.store.entries.len();
        // Folders leading to included entries stay as context.
        let mut keep = vec![false; n];
        for (id, &cover) in cover.iter().enumerate() {
            if cover == access::Cover::Inside {
                let mut cur = id;
                while !keep[cur] {
                    keep[cur] = true;
                    match self.store.entries[cur].parent {
                        NO_PARENT => break,
                        parent => cur = parent as usize,
                    }
                }
            }
        }
        let mut builder = IndexBuilder::reusing_names(self);
        let mut new_id = vec![NO_PARENT; n];
        for id in 0..n {
            if !keep[id] {
                continue;
            }
            let parent = match self.store.entries[id].parent {
                NO_PARENT => NO_PARENT,
                parent => new_id[parent as usize],
            };
            new_id[id] = builder.store.copy(parent, &self.store, id as u32, None);
            if cover[id] != access::Cover::Inside {
                builder.store.entries[new_id[id] as usize].flags |= FLAG_CONTEXT;
            }
        }
        if builder.is_empty() {
            builder.push(NO_PARENT, "", true, 0, 0);
        }
        builder.build()
    }

    /// Approximate heap size of the index in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.store.entries.capacity() * std::mem::size_of::<Entry>()
            + self.store.folders.capacity() * std::mem::size_of::<Folder>()
            + self.store.big.capacity() * 16
            + self.names.heap_bytes()
            + (self.by_name.capacity() + self.by_path.capacity() + self.by_size.capacity() + self.by_modified.capacity()) * 4
            + self.tree.as_ref().map_or(0, |t| (t.start.capacity() + t.children.capacity()) * 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut b = IndexBuilder::default();
        let root = b.push(NO_PARENT, "", true, 0, 0);
        let home = b.push(root, "home", true, 0, 10);
        let docs = b.push(home, "Docs", true, 0, 20);
        b.push(docs, "Report.PDF", false, 300, 30);
        b.push(docs, "notes.txt", false, 100, 40);
        b.push(home, "a.txt", false, 200, 5);
        b.build()
    }

    fn names(index: &Index, ids: &[u32]) -> Vec<String> {
        ids.iter().map(|&id| index.name(id).into_owned()).collect()
    }

    #[test]
    fn paths() {
        let index = sample();
        assert_eq!(index.path(0), "/");
        assert_eq!(index.path(3), "/home/Docs/Report.PDF");
        assert_eq!(index.dir_path(3), "/home/Docs");
    }

    #[test]
    fn search_ignores_case_and_requires_all_terms() {
        let index = sample();
        let hits = index.search(&Query::parse("REP pdf"), SortKey::Name, false);
        assert_eq!(names(&index, &hits), ["Report.PDF"]);
        let hits = index.search(&Query::parse("txt"), SortKey::Name, false);
        assert_eq!(names(&index, &hits), ["a.txt", "notes.txt"]);
    }

    #[test]
    fn real_entries_keep_their_paths() {
        let real = [
            RealEntry { path: "/home/me/demo/a.jpg".into(), is_dir: false, size: 5, mtime: 7 },
            RealEntry { path: "/home/me/demo".into(), is_dir: true, size: 0, mtime: 7 },
        ];
        let index = synthetic_index_with(2_000, 1, &real);
        let hit = index.search(&Query::parse("a.jpg"), SortKey::Name, false);
        let paths: Vec<String> = hit.iter().map(|&id| index.path(id)).filter(|p| p.starts_with("/home/me")).collect();
        assert_eq!(paths, ["/home/me/demo/a.jpg"]);
        // The fake home folder shares /home with the real entries.
        let homes = index.search(&Query::parse("home"), SortKey::Name, false);
        assert_eq!(homes.iter().filter(|&&id| index.name(id) == "home").count(), 1);
    }

    #[test]
    fn restrict_to_scope() {
        let index = sample();
        let all = Query::parse("");
        let names = |index: &Index| names(index, &index.search(&all, SortKey::Path, false));

        let docs = index.restrict(&Scope::new(["/home/Docs"], std::iter::empty::<&str>()));
        assert_eq!(names(&docs), ["Docs", "notes.txt", "Report.PDF"]);
        // Context folders still give full paths.
        let hit = docs.search(&Query::parse("notes"), SortKey::Name, false)[0];
        assert_eq!(docs.path(hit), "/home/Docs/notes.txt");

        let no_docs = index.restrict(&Scope::new(["/"], ["/home/*"]));
        assert_eq!(names(&no_docs), ["home"]);
        let no_txt = index.restrict(&Scope::new(["/"], ["*.txt"]));
        assert_eq!(names(&no_txt), ["home", "Docs", "Report.PDF"]);
        let nothing = index.restrict(&Scope::new(["/srv"], std::iter::empty::<&str>()));
        assert!(names(&nothing).is_empty());
    }

    #[test]
    fn selections_match_searches() {
        let index = synthetic_index(30_000, 5);
        let mask: Vec<u64> = (0..index.len().div_ceil(64)).map(|i| if i % 3 == 0 { 0 } else { u64::MAX ^ (i as u64) }).collect();
        let has = |id: u32| mask[id as usize / 64] & (1 << (id % 64)) != 0;
        for query in ["", "rechnung", "*.pdf", "zzzz"] {
            for key in [SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Modified] {
                for descending in [false, true] {
                    let q = Query::parse(query);
                    let mut want = index.search(&q, key, descending);
                    want.retain(|&id| has(id));
                    let sel = index.select(&q, key, descending, Some(&mask));
                    assert_eq!(sel.len(), want.len());
                    assert_eq!(index.selected(&sel, 0, want.len()), want);
                    for start in [0, 1, 63, 64, 65, want.len() / 2, want.len().saturating_sub(3)] {
                        let end = (start + 70).min(want.len());
                        assert_eq!(index.selected(&sel, start, 70), want[start.min(end)..end], "{query} {key:?} {descending} {start}");
                    }
                }
            }
        }
    }

    #[test]
    fn orders() {
        let index = sample();
        let all = Query::parse("");
        assert_eq!(
            names(&index, &index.search(&all, SortKey::Path, false)),
            ["home", "a.txt", "Docs", "notes.txt", "Report.PDF"]
        );
        assert_eq!(
            names(&index, &index.search(&Query::parse("t"), SortKey::Size, true)),
            ["Report.PDF", "a.txt", "notes.txt"]
        );
        assert_eq!(
            names(&index, &index.search(&Query::parse("."), SortKey::Modified, false)),
            ["a.txt", "Report.PDF", "notes.txt"]
        );
    }
}
