//! Changing an index in place, for live updates.
//!
//! [`plan`] reads the folders that were reported as changed and works out
//! what differs, without touching the index; [`Index::apply`] then makes
//! those changes quickly. Ids stay stable: removed entries are only marked
//! dead, new ones are appended, and the four sort orders get entries taken
//! out and merged in rather than being sorted again. A full rebuild (see
//! [`crate::scan::reconcile`]) clears the dead entries out now and then.

use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    FLAG_CONTEXT, FLAG_DEAD, FLAG_DIR, Index, Lookup, NO_PARENT, Owner, Scope,
    scan::{Folder, Progress, Subfolder, Walker, mtime_ns},
};

enum Added {
    File { name: String, size: u64, mtime: i64 },
    Folder(Folder),
}

/// What changed in some folders; made by [`plan`], used by [`Index::apply`].
#[derive(Default)]
pub struct Patch {
    /// Folders read again, with their current modification time.
    folder_times: Vec<(u32, i64)>,
    owners: Vec<(u32, Owner)>,
    /// Files whose size or time changed.
    files: Vec<(u32, u64, i64)>,
    /// Entries that are gone, with everything below them.
    removed: Vec<u32>,
    /// New entries and the folders they go into.
    added: Vec<(u32, Added)>,
    pub read_time: Duration,
    /// Entries in the largest folder read, files looked at one by one.
    pub largest_folder: usize,
    pub files_checked: usize,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty() && self.files.is_empty() && self.removed.is_empty() && self.added.is_empty()
    }

    /// Adds what `other` found.
    pub fn merge(&mut self, other: Patch) {
        self.folder_times.extend(other.folder_times);
        self.owners.extend(other.owners);
        self.files.extend(other.files);
        self.removed.extend(other.removed);
        self.added.extend(other.added);
        self.read_time += other.read_time;
        self.largest_folder = self.largest_folder.max(other.largest_folder);
        self.files_checked += other.files_checked;
    }
}

/// The deepest live folder on `path`, and its path.
fn deepest_folder(index: &Index, tree: &crate::Tree, root: u32, path: &Path) -> (u32, PathBuf) {
    let (mut id, mut found) = (root, PathBuf::from("/"));
    for part in path.components() {
        let Component::Normal(part) = part else { continue };
        let name = crate::name::from_os(part);
        let next = tree
            .of(&index.store, id)
            .filter(|&c| !index.is_dead(c))
            .find(|&c| index.is_dir(c) && index.names.is(index.store.entries[c as usize].name, &name));
        match next {
            Some(next) => {
                id = next;
                found.push(part);
            }
            None => break,
        }
    }
    (id, found)
}

/// For files that were written, nothing created, removed or moved: only
/// their size and time, one look each instead of reading their folders
/// again. Returns the files it could not do this for (unknown to the index,
/// or gone): their folders are to be read for them.
pub fn plan_files(index: &Index, files: &[PathBuf]) -> (Patch, Vec<PathBuf>) {
    let start = Instant::now();
    let local;
    let tree = match &index.tree {
        Some(tree) => tree,
        None => {
            local = index.build_tree();
            &local
        }
    };
    let mut patch = Patch::default();
    let mut folders = Vec::new();
    let Some(root) = (0..index.len() as u32).find(|&id| index.parent(id).is_none()) else {
        return (patch, folders);
    };
    for path in files {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { continue };
        let (folder, found) = deepest_folder(index, tree, root, parent);
        if found != parent {
            folders.push(path.clone());
            continue;
        }
        let name = crate::name::from_os(name);
        let entry = tree
            .of(&index.store, folder)
            .filter(|&c| !index.is_dead(c))
            .find(|&c| index.names.is(index.store.entries[c as usize].name, &name));
        let meta = std::fs::symlink_metadata(path);
        match (entry, meta) {
            (Some(id), Ok(meta)) if !index.is_dir(id) && !meta.is_dir() => {
                let mtime = mtime_ns(&meta);
                if index.size(id) != meta.len() || index.modified(id) != mtime.div_euclid(crate::NANOS) {
                    patch.files.push((id, meta.len(), mtime));
                }
            }
            // Folders take care of themselves; anything else is news for
            // the folder it is in.
            (Some(id), _) if index.is_dir(id) => {}
            _ => folders.push(path.clone()),
        }
    }
    patch.read_time = start.elapsed();
    patch.files_checked = files.len();
    (patch, folders)
}

#[derive(Debug, Clone, Default)]
pub struct UpdateStats {
    pub folders: usize,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    /// Some folder's owner, group or mode changed, so who may see what can
    /// have changed too.
    pub permissions_changed: bool,
    pub read_time: Duration,
    pub apply_time: Duration,
    pub largest_folder: usize,
    pub files_checked: usize,
}

/// Reads the reported folders and works out what changed. A path that is
/// not in the index stands for its nearest folder that is.
pub fn plan(index: &Index, scope: &Scope, skip: &[String], dirty: &[PathBuf]) -> Patch {
    let dirty: Vec<(PathBuf, Option<HashSet<String>>)> = dirty.iter().map(|p| (p.clone(), None)).collect();
    plan_named(index, scope, skip, &dirty)
}

/// Like [`plan`], with the names each folder's change was reported for, if
/// known. Entries the index has and the report does not name are kept as
/// they are, without looking at them; written files are reported apart
/// ([`plan_files`]). Names are those of the entries in the folder, new or
/// replaced; removed ones show by their absence.
pub fn plan_named(index: &Index, scope: &Scope, skip: &[String], dirty: &[(PathBuf, Option<HashSet<String>>)]) -> Patch {
    let start = Instant::now();
    let n = index.len();
    // Without a prepared tree, work one out for this call.
    let local;
    let tree = match &index.tree {
        Some(tree) => tree,
        None => {
            local = index.build_tree();
            &local
        }
    };
    let live_children = |id: u32| tree.of(&index.store, id).filter(|&c| !index.is_dead(c));
    let Some(root) = (0..n as u32).find(|&id| index.parent(id).is_none()) else {
        return Patch::default();
    };

    // Each reported path to the deepest live folder on it, with the names
    // reported there (`None`: look at everything).
    let mut folders: Vec<(u32, PathBuf, Option<HashSet<String>>)> = Vec::new();
    let mut at: HashMap<u32, usize> = HashMap::new();
    for (path, names) in dirty {
        let (id, found) = deepest_folder(index, tree, root, path);
        // A path that stands for a folder above it says nothing about names there.
        let names = if &found == path { names.clone() } else { None };
        match at.get(&id) {
            Some(&i) => match (&mut folders[i].2, names) {
                (Some(all), Some(more)) => all.extend(more),
                (all, _) => *all = None,
            },
            None => {
                at.insert(id, folders.len());
                folders.push((id, found, names));
            }
        }
    }

    let progress = Progress::default();
    let walker = Walker { scope, skip, progress: &progress };
    let mut patch = Patch::default();
    for (id, path, names) in folders {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // Gone meanwhile; its parent reports that.
            continue;
        };
        patch.folder_times.push((id, crate::scan::settled(mtime_ns(&meta))));
        let segments = index.segments(id);
        let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
        let included = if index.parent(id).is_none() { scope.is_included(&[]) } else { index.flags(id) & FLAG_CONTEXT == 0 };
        let mut known: HashMap<Cow<'_, str>, u32> = live_children(id).map(|c| (index.name(c), c)).collect();
        let listing = match &names {
            None => walker.read(&path, &refs, included),
            Some(names) => walker.read_known(&path, &refs, included, |name, is_dir| {
                !names.contains(name) && known.get(name).is_some_and(|&c| index.is_dir(c) == is_dir)
            }),
        };
        let Some(listing) = listing else { continue };
        for name in &listing.kept {
            known.remove(name.as_str());
        }
        patch.largest_folder = patch.largest_folder.max(listing.files.len() + listing.folders.len());

        for (name, file) in listing.files() {
            match known.remove(name) {
                Some(c) if !index.is_dir(c) => {
                    let secs = file.mtime.div_euclid(crate::NANOS);
                    if index.size(c) != file.size || index.modified(c) != secs {
                        patch.files.push((c, file.size, file.mtime));
                    }
                }
                other => {
                    // New, or a folder that became a file.
                    patch.removed.extend(other);
                    patch.added.push((id, Added::File { name: name.to_owned(), size: file.size, mtime: file.mtime }));
                }
            }
        }
        for sub in listing.folders {
            match known.remove(sub.name.as_str()) {
                Some(c) if index.is_dir(c) => {
                    // Only its ownership comes from here; its content and
                    // time are its own folder's business.
                    if index.owner(c) != Some(sub.owner) {
                        patch.owners.push((c, sub.owner));
                    }
                }
                other => {
                    patch.removed.extend(other);
                    let (name, child_path) = (sub.name.clone(), sub.path.clone());
                    let mut child = refs.clone();
                    child.push(&name);
                    let folder = walker.walk(&child_path, &child, Subfolder { ..sub });
                    patch.added.push((id, Added::Folder(folder)));
                }
            }
        }
        // Whatever is left is gone.
        patch.removed.extend(known.into_values());
    }
    patch.read_time = start.elapsed();
    patch
}

impl Index {
    fn build_tree(&self) -> crate::Tree {
        // Children of each folder, by folder slot.
        let store = &self.store;
        let slots = store.folders.len();
        let mut start = vec![0u32; slots + 1];
        for e in &store.entries {
            if e.parent != NO_PARENT {
                start[store.entries[e.parent as usize].size as usize + 1] += 1;
            }
        }
        for i in 0..slots {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut children = vec![0u32; start[slots] as usize];
        for (id, e) in store.entries.iter().enumerate() {
            if e.parent != NO_PARENT {
                let slot = store.entries[e.parent as usize].size as usize;
                children[fill[slot] as usize] = id as u32;
                fill[slot] += 1;
            }
        }
        crate::Tree { start, children, extra: HashMap::new() }
    }

    /// Readies the index for in-place updates: the name lookup and the
    /// children of every folder. Without it, [`plan`] and [`Index::apply`]
    /// still work, but each call pays for this.
    pub fn prepare(&mut self) {
        if self.tree.is_none() {
            self.tree = Some(self.build_tree());
        }
        if self.lookup.is_none() {
            self.lookup = Some(Lookup::of(&self.names));
        }
    }

    /// The same entries without the dead ones, with new ids.
    pub fn compacted(&self) -> Index {
        let mut b = crate::IndexBuilder::reusing_names(self);
        let mut new_id = vec![NO_PARENT; self.len()];
        for id in 0..self.len() {
            if self.is_dead(id as u32) {
                continue;
            }
            let parent = match self.store.entries[id].parent {
                NO_PARENT => NO_PARENT,
                parent => new_id[parent as usize],
            };
            new_id[id] = b.store.copy(parent, &self.store, id as u32, None);
        }
        let index = b.build();
        crate::give_back_memory();
        index
    }

    /// Names of the folders from the root down to `id`.
    fn segments(&self, id: u32) -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if self.parent(c).is_some() {
                chain.push(self.name(c).into_owned());
            }
            cur = self.parent(c);
        }
        chain.reverse();
        chain
    }

    fn append(&mut self, parent: u32, name: &str, flags: u8, size: u64, mtime: i64, owner: Option<Owner>) -> u32 {
        let lookup = self.lookup.get_or_insert_with(|| Lookup::of(&self.names));
        let name = lookup.intern(&mut self.names, name);
        let id = self.store.push(parent, name, flags, size, mtime, owner);
        if let Some(tree) = &mut self.tree {
            tree.extra.entry(parent).or_default().push(id);
        }
        id
    }

    fn append_folder(&mut self, parent: u32, folder: Folder, new: &mut Vec<u32>) {
        let flags = FLAG_DIR | if folder.context { FLAG_CONTEXT } else { 0 };
        let id = self.append(parent, &folder.name, flags, 0, folder.mtime, folder.owner);
        new.push(id);
        for (name, file) in folder.listing.files() {
            let file = self.append(id, name, 0, file.size, file.mtime, None);
            new.push(file);
        }
        for child in folder.folders {
            self.append_folder(id, child, new);
        }
    }

    /// Makes the changes worked out by [`plan`].
    pub fn apply(&mut self, patch: Patch) -> UpdateStats {
        let start = Instant::now();
        let mut stats = UpdateStats {
            folders: patch.folder_times.len(),
            read_time: patch.read_time,
            largest_folder: patch.largest_folder,
            files_checked: patch.files_checked,
            ..Default::default()
        };
        let n = self.len();
        // Entries whose size or time changes leave and re-enter the size and
        // date orders.
        let mut moved = vec![false; n];
        let mut changed = Vec::new();
        for (id, mtime) in patch.folder_times {
            let before = self.modified(id);
            self.store.set_folder_mtime(id, mtime);
            if self.modified(id) != before && self.is_result(id) && !moved[id as usize] {
                moved[id as usize] = true;
                changed.push(id);
            }
        }
        for (id, owner) in patch.owners {
            self.store.set_owner(id, owner);
            stats.permissions_changed = true;
        }

        // Removed entries and everything below them. Children always come
        // after their parents, so one pass from the first one finds them all.
        let mut gone = vec![false; n];
        let mut first = n;
        for id in patch.removed {
            if !self.is_dead(id) {
                gone[id as usize] = true;
                first = first.min(id as usize);
            }
        }
        for id in first..n {
            let parent = self.store.entries[id].parent;
            if !gone[id] && parent != NO_PARENT && gone[parent as usize] && !self.is_dead(id as u32) {
                gone[id] = true;
            }
            if gone[id] {
                self.store.entries[id].flags |= FLAG_DEAD;
                self.dead += 1;
                stats.removed += 1;
            }
        }

        for (id, size, mtime) in patch.files {
            if self.is_dead(id) {
                continue;
            }
            self.store.set_file(id, size, mtime);
            if !moved[id as usize] {
                moved[id as usize] = true;
                changed.push(id);
            }
        }
        // Dead entries are dropped from every order anyway.
        changed.retain(|&id| !self.is_dead(id));
        stats.changed = changed.len();

        let mut new = Vec::new();
        for (parent, added) in patch.added {
            if self.is_dead(parent) {
                continue;
            }
            match added {
                Added::File { name, size, mtime } => new.push(self.append(parent, &name, 0, size, mtime, None)),
                Added::Folder(folder) => self.append_folder(parent, folder, &mut new),
            }
        }
        new.retain(|&id| self.is_result(id));
        stats.added = new.len();

        let keep = |id: &u32| !gone.get(*id as usize).copied().unwrap_or(false);
        let keep_unmoved = |id: &u32| keep(id) && !moved.get(*id as usize).copied().unwrap_or(false);
        let mut resorted = new.clone();
        resorted.extend(&changed);

        // Only the orders something happened to are touched: a file that
        // grows changes the size and date orders, not the others.
        let removed_any = stats.removed > 0;
        let moved_any = !changed.is_empty();
        if removed_any || !new.is_empty() {
            let mut by_name = std::mem::take(&mut self.by_name);
            if removed_any {
                by_name.retain(keep);
            }
            self.by_name = self.merge(by_name, new.clone(), Index::cmp_name);
            let mut by_path = std::mem::take(&mut self.by_path);
            if removed_any {
                by_path.retain(keep);
            }
            self.by_path = self.merge(by_path, new, Index::cmp_path);
        }
        if removed_any || moved_any || !resorted.is_empty() {
            let mut by_size = std::mem::take(&mut self.by_size);
            if removed_any || moved_any {
                by_size.retain(keep_unmoved);
            }
            self.by_size = self.merge(by_size, resorted.clone(), Index::cmp_size);
            let mut by_modified = std::mem::take(&mut self.by_modified);
            if removed_any || moved_any {
                by_modified.retain(keep_unmoved);
            }
            self.by_modified = self.merge(by_modified, resorted, Index::cmp_modified);
        }

        stats.apply_time = start.elapsed();
        stats
    }

    /// Merges `add` into the sorted `order`: positions by binary search, then
    /// one copy, so only the new entries are ever compared.
    fn merge(&self, order: Vec<u32>, mut add: Vec<u32>, cmp: fn(&Index, u32, u32) -> Ordering) -> Vec<u32> {
        if add.is_empty() {
            return order;
        }
        add.sort_by(|&a, &b| cmp(self, a, b));
        let mut out = Vec::with_capacity(order.len() + add.len());
        let mut from = 0;
        for id in add {
            let at = from + order[from..].partition_point(|&o| cmp(self, o, id) == Ordering::Less);
            out.extend_from_slice(&order[from..at]);
            out.push(id);
            from = at;
        }
        out.extend_from_slice(&order[from..]);
        out
    }

    /// The order of [`Index::build_orders`]: folded name, then name id,
    /// then entry id.
    fn cmp_name(&self, a: u32, b: u32) -> Ordering {
        let (na, nb) = (self.store.entries[a as usize].name, self.store.entries[b as usize].name);
        self.names.folded(na).cmp(self.names.folded(nb)).then(na.cmp(&nb)).then(a.cmp(&b))
    }

    fn cmp_size(&self, a: u32, b: u32) -> Ordering {
        self.size(a).cmp(&self.size(b)).then_with(|| self.cmp_name(a, b))
    }

    fn cmp_modified(&self, a: u32, b: u32) -> Ordering {
        self.modified(a).cmp(&self.modified(b)).then_with(|| self.cmp_name(a, b))
    }

    /// Depth-first order with siblings by name: compare the chains of
    /// folders from the root.
    fn cmp_path(&self, a: u32, b: u32) -> Ordering {
        let chain = |id: u32| {
            let mut chain = Vec::with_capacity(16);
            let mut cur = Some(id);
            while let Some(c) = cur {
                if self.parent(c).is_some() {
                    chain.push(c);
                }
                cur = self.parent(c);
            }
            chain.reverse();
            chain
        };
        let (ca, cb) = (chain(a), chain(b));
        for (x, y) in ca.iter().zip(&cb) {
            if x != y {
                return self.cmp_name(*x, *y);
            }
        }
        ca.len().cmp(&cb.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Query, SortKey, scan};
    use std::fs;

    /// Every order as paths with sizes and times, to compare indexes whose
    /// ids differ. Entries that tie on the sort key are ordered by id, which
    /// differs between a fresh scan and appended entries, so within a tie
    /// they are sorted by path.
    fn snapshot(index: &Index, base: &str) -> Vec<Vec<String>> {
        [SortKey::Name, SortKey::Path, SortKey::Size, SortKey::Modified]
            .into_iter()
            .map(|key| {
                let rows: Vec<(String, String)> = index
                    .search(&Query::parse(""), key, false)
                    .into_iter()
                    .map(|id| {
                        let path = index.path(id);
                        let name = index.name(id).to_lowercase();
                        let tie = match key {
                            SortKey::Name => name,
                            SortKey::Path => path.clone(),
                            SortKey::Size => format!("{} {name}", index.size(id)),
                            SortKey::Modified => format!("{} {name}", index.modified(id)),
                        };
                        let row = format!("{} {} {}", path.strip_prefix(base).unwrap_or(&path), index.size(id), index.modified(id));
                        (tie, row)
                    })
                    .collect();
                let mut out = Vec::new();
                for group in rows.chunk_by(|a, b| a.0 == b.0) {
                    let mut rows: Vec<String> = group.iter().map(|r| r.1.clone()).collect();
                    rows.sort();
                    out.extend(rows);
                }
                out
            })
            .collect()
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    fn folders_under(root: &Path) -> Vec<PathBuf> {
        let mut out = vec![root.to_path_buf()];
        let mut i = 0;
        while i < out.len() {
            if let Ok(entries) = fs::read_dir(&out[i]) {
                for e in entries.flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        out.push(e.path());
                    }
                }
            }
            i += 1;
        }
        out
    }

    fn files_under(root: &Path) -> Vec<PathBuf> {
        folders_under(root)
            .into_iter()
            .flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .collect()
    }

    #[test]
    fn named_rereads_look_only_at_named_and_new_entries() {
        let root = std::env::temp_dir().join(format!("anything-named-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("big/sub")).unwrap();
        for i in 0..20 {
            fs::write(root.join(format!("big/f{i}")), b"x").unwrap();
        }
        let base = root.to_str().unwrap().to_owned();
        let scope = Scope::new([&base], [] as [&str; 0]);
        let mut index = scan::scan(&scope, &[], &Progress::default()).0;
        index.prepare();
        let size = |index: &Index, name: &str| {
            index.search(&Query::parse(name), SortKey::Name, false).first().map(|&id| index.size(id))
        };

        fs::write(root.join("big/new"), b"newfile").unwrap();
        fs::remove_file(root.join("big/f3")).unwrap();
        fs::write(root.join("big/f5"), b"grown").unwrap();
        fs::write(root.join("big/f6"), b"grown too").unwrap();
        let named = HashSet::from(["new".to_owned(), "f6".to_owned()]);
        let patch = plan_named(&index, &scope, &[], &[(root.join("big"), Some(named))]);
        // Looked at: the new entry and the named one, nothing else.
        assert_eq!(patch.largest_folder, 2);
        index.apply(patch);
        assert_eq!(size(&index, "new"), Some(7));
        assert_eq!(size(&index, "f3"), None);
        assert_eq!(size(&index, "f6"), Some(9));
        // Not named: left alone here (its own "written" report updates it).
        assert_eq!(size(&index, "f5"), Some(1));
        assert!(index.search(&Query::parse("sub"), SortKey::Name, false).len() == 1);

        // Without names everything is looked at.
        let patch = plan(&index, &scope, &[], &[root.join("big")]);
        index.apply(patch);
        assert_eq!(size(&index, "f5"), Some(5));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn written_files_without_reading_their_folder() {
        let root = std::env::temp_dir().join(format!("anything-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("big")).unwrap();
        for i in 0..50 {
            fs::write(root.join(format!("big/f{i}")), b"x").unwrap();
        }
        let base = root.to_str().unwrap().to_owned();
        let scope = Scope::new([&base], [] as [&str; 0]);
        let mut index = scan::scan(&scope, &[], &Progress::default()).0;
        index.prepare();

        // One file grows: only that one entry changes.
        fs::write(root.join("big/f7"), b"longer").unwrap();
        let (patch, folders) = plan_files(&index, &[root.join("big/f7"), root.join("big/f8")]);
        assert!(folders.is_empty());
        assert_eq!(patch.files.len(), 1, "f8 did not change");
        index.apply(patch);
        let id = index.search(&Query::parse("f7"), SortKey::Name, false)[0];
        assert_eq!(index.size(id), 6);

        // Unknown or gone: its folder is read after all.
        fs::write(root.join("big/new"), b"x").unwrap();
        fs::remove_file(root.join("big/f9")).unwrap();
        let (patch, folders) = plan_files(&index, &[root.join("big/new"), root.join("big/f9"), root.join("nowhere/x")]);
        assert!(patch.is_empty());
        assert_eq!(folders, [root.join("big/new"), root.join("big/f9"), root.join("nowhere/x")]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn random_changes_match_a_fresh_scan() {
        for seed in 1..=20u64 {
            random_run(seed);
        }
    }

    fn random_run(seed: u64) {
        let root = std::env::temp_dir().join(format!("anything-update-{}-{seed}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let base = root.to_str().unwrap().to_owned();
        let scope = Scope::new([&base], ["skipped"]);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
        let names = ["Rechnung", "rechnung", "Angebot", "a", "Z", "ü", "IMG", "skipped", "x.txt"];

        // A starting tree.
        for i in 0..40 {
            let dirs = folders_under(&root);
            let dir = &dirs[rng.below(dirs.len())];
            let name = format!("{}-{}", names[rng.below(names.len())], i);
            if rng.below(3) == 0 {
                fs::create_dir_all(dir.join(&name)).unwrap();
            } else {
                fs::write(dir.join(&name), vec![b'x'; rng.below(50)]).unwrap();
            }
        }
        let mut index = scan::scan(&scope, &[], &Progress::default()).0;
        // Half the runs use a prepared index, half work it out per call.
        if seed.is_multiple_of(2) {
            index.prepare();
        }

        for round in 0..30 {
            // A few changes, reporting the folders they happen in.
            let mut dirty = Vec::new();
            for step in 0..rng.below(6) + 1 {
                let dirs = folders_under(&root);
                let files = files_under(&root);
                let name = format!("{}-r{round}s{step}", names[rng.below(names.len())]);
                match rng.below(6) {
                    0 => {
                        let dir = &dirs[rng.below(dirs.len())];
                        fs::write(dir.join(&name), vec![b'y'; rng.below(80)]).unwrap();
                        dirty.push(dir.clone());
                    }
                    1 => {
                        let dir = &dirs[rng.below(dirs.len())];
                        fs::create_dir_all(dir.join(&name).join("inner")).unwrap();
                        fs::write(dir.join(&name).join("inner/f.txt"), b"z").unwrap();
                        dirty.push(dir.clone());
                    }
                    2 if !files.is_empty() => {
                        let file = &files[rng.below(files.len())];
                        fs::remove_file(file).unwrap();
                        dirty.push(file.parent().unwrap().to_path_buf());
                    }
                    3 if dirs.len() > 1 => {
                        let dir = &dirs[1 + rng.below(dirs.len() - 1)];
                        fs::remove_dir_all(dir).unwrap();
                        dirty.push(dir.parent().unwrap().to_path_buf());
                    }
                    4 if !files.is_empty() => {
                        let file = &files[rng.below(files.len())];
                        fs::write(file, vec![b'g'; 100 + rng.below(100)]).unwrap();
                        dirty.push(file.parent().unwrap().to_path_buf());
                    }
                    _ if dirs.len() > 1 => {
                        // Rename a folder within its parent.
                        let dir = &dirs[1 + rng.below(dirs.len() - 1)];
                        let to = dir.parent().unwrap().join(&name);
                        fs::rename(dir, &to).unwrap();
                        dirty.push(dir.parent().unwrap().to_path_buf());
                    }
                    _ => {}
                }
            }
            let patch = plan(&index, &scope, &[], &dirty);
            index.apply(patch);
            let fresh = scan::scan(&scope, &[], &Progress::default()).0;
            let (mine, want) = (snapshot(&index, &base), snapshot(&fresh, &base));
            for (order, (m, w)) in ["name", "path", "size", "modified"].iter().zip(mine.iter().zip(&want)) {
                if m != w {
                    let at = m.iter().zip(w).position(|(a, b)| a != b).unwrap_or(m.len().min(w.len()));
                    panic!(
                        "seed {seed} round {round}, {order} order differs at {at} ({} vs {} entries): {:?} vs {:?}",
                        m.len(),
                        w.len(),
                        &m[at.saturating_sub(1)..(at + 2).min(m.len())],
                        &w[at.saturating_sub(1)..(at + 2).min(w.len())]
                    );
                }
            }
        }
        // Compacting drops the dead entries and keeps everything else.
        let compact = index.compacted();
        assert_eq!(compact.dead_share(), 0.0);
        assert_eq!(snapshot(&compact, &base), snapshot(&index, &base));
        // Dead and appended entries survive a trip through a snapshot.
        let file = root.with_extension("idx");
        index.save(&file, 7).unwrap();
        let loaded = Index::load(&file, 7).unwrap();
        assert_eq!(snapshot(&loaded, &base), snapshot(&index, &base));
        assert_eq!(loaded.dead_share(), index.dead_share());
        fs::remove_file(file).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
