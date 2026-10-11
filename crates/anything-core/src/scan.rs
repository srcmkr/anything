//! Walks the file system and builds an [`Index`], and brings an existing
//! index up to date by reading only the folders that changed.
//!
//! Folders are read in parallel; the walk follows the [`Scope`] and never
//! crosses into pseudo, virtual or network file systems. Symbolic links are
//! recorded but not followed. Folders that cannot be read are counted and
//! skipped, as are names that are not valid UTF-8.

use std::{
    collections::HashMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use rayon::prelude::*;

use crate::{FLAG_CONTEXT, FLAG_DEAD, FLAG_DIR, Index, IndexBuilder, NANOS, NO_PARENT, Owner, Scope};

/// File systems that hold real files on local disks.
const LOCAL_FS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "bcachefs", "zfs", "jfs", "reiserfs", "vfat",
    "exfat", "ntfs", "ntfs3", "fuseblk", "hfsplus", "udf", "ecryptfs",
];

/// Mount points whose file system is not on [`LOCAL_FS`], from mountinfo.
pub fn foreign_mounts(mountinfo: &str) -> Vec<String> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let mount_point = fields.get(4)?;
            let fs_type = fields.iter().position(|f| *f == "-").and_then(|at| fields.get(at + 1))?;
            (!LOCAL_FS.contains(fs_type)).then(|| unescape(mount_point))
        })
        .collect()
}

/// mountinfo escapes space, tab, newline and backslash as octal.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            out.push(u8::from_str_radix(&field[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encrypted home folders (eCryptfs): the encrypted folder underneath each
/// one, as mounted (often a link), and where it is mounted.
pub fn ecryptfs_mounts(mountinfo: &str) -> Vec<(String, String)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let mount_point = fields.get(4)?;
            let at = fields.iter().position(|f| *f == "-")?;
            if *fields.get(at + 1)? != "ecryptfs" {
                return None;
            }
            Some((unescape(fields.get(at + 2)?), unescape(mount_point)))
        })
        .collect()
}

/// Folders to skip on this machine: mount points of file systems that are
/// not local disks, and the encrypted side of encrypted home folders (it
/// holds the same files again, under scrambled names).
pub fn foreign_mounts_here() -> Vec<String> {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    let mut skip = foreign_mounts(&text);
    for (lower, _) in ecryptfs_mounts(&text) {
        if let Ok(real) = fs::canonicalize(&lower) {
            skip.push(real.to_string_lossy().into_owned());
        }
    }
    skip
}

/// Progress of a running scan, readable from other threads.
#[derive(Default)]
pub struct Progress {
    pub entries: AtomicUsize,
    pub unreadable: AtomicUsize,
    /// Set to stop the scan early; the result is then incomplete.
    pub cancel: AtomicBool,
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
pub struct ScanStats {
    pub entries: usize,
    pub folders: usize,
    pub unreadable: usize,
    pub duration: Duration,
    pub cancelled: bool,
}

pub(crate) fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.mtime().saturating_mul(NANOS) + meta.mtime_nsec()
}

/// How recent a folder's modification time may be before it is not trusted.
const RACY: i64 = 2 * NANOS;

/// A folder's modification time as kept in the index. A folder changed
/// within the same tick of the file system's clock as it was read keeps its
/// time, so a time that fresh may hide a change; it is kept slightly too
/// off by a nanosecond (never across a second, so the time shown stays
/// right), so the next full check reads the folder again (as git does with
/// "racy" index entries).
pub(crate) fn settled(mtime: i64) -> i64 {
    settled_at(mtime, std::time::SystemTime::now())
}

fn settled_at(mtime: i64, now: std::time::SystemTime) -> i64 {
    let now = now.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64);
    match now - mtime < RACY {
        false => mtime,
        true if mtime.rem_euclid(NANOS) == 0 => mtime + 1,
        true => mtime - 1,
    }
}

/// Owner, group, mode and whether a POSIX access ACL is set.
fn owner(path: &Path, meta: &fs::Metadata) -> Owner {
    Owner { uid: meta.uid(), gid: meta.gid(), mode: (meta.mode() & 0o7777) as u16, acl: has_acl(path) }
}

fn has_acl(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // Size query only; a positive size means the attribute exists.
    let size = unsafe {
        libc::lgetxattr(c_path.as_ptr(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0)
    };
    size > 0
}

pub(crate) struct FileRecord {
    /// End of the name in [`Listing::names`].
    pub(crate) end: u32,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
}

pub(crate) struct Subfolder {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) mtime: i64,
    pub(crate) owner: Owner,
    pub(crate) included: bool,
}

/// One folder's content, file names packed into a single string.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) names: String,
    pub(crate) files: Vec<FileRecord>,
    pub(crate) folders: Vec<Subfolder>,
    /// Entries still there that were not looked at (see [`Walker::read_known`]).
    pub(crate) kept: Vec<String>,
}

impl Listing {
    pub(crate) fn files(&self) -> impl Iterator<Item = (&str, &FileRecord)> {
        let mut start = 0;
        self.files.iter().map(move |f| {
            let name = &self.names[start..f.end as usize];
            start = f.end as usize;
            (name, f)
        })
    }
}

pub(crate) struct Folder {
    pub(crate) name: String,
    pub(crate) mtime: i64,
    pub(crate) owner: Option<Owner>,
    /// Kept only for the paths below it.
    pub(crate) context: bool,
    pub(crate) listing: Listing,
    pub(crate) folders: Vec<Folder>,
}

#[derive(Clone, Copy)]
pub(crate) struct Walker<'a> {
    pub(crate) scope: &'a Scope,
    pub(crate) skip: &'a [String],
    pub(crate) progress: &'a Progress,
}

impl Walker<'_> {
    /// Reads one folder: the files the scope includes and the subfolders to
    /// descend into. `None` if it cannot be read.
    pub(crate) fn read(&self, path: &Path, segments: &[&str], included: bool) -> Option<Listing> {
        self.read_known(path, segments, included, |_, _| false)
    }

    /// Like [`read`](Self::read), but entries for which `known(name, is_dir)`
    /// holds are only noted in `kept`, without looking at them: listing a
    /// folder's names is cheap, looking at every entry is not (in an
    /// encrypted home folder it means reading every file's header).
    pub(crate) fn read_known(
        &self,
        path: &Path,
        segments: &[&str],
        included: bool,
        known: impl Fn(&str, bool) -> bool,
    ) -> Option<Listing> {
        let Ok(read) = fs::read_dir(path) else {
            self.progress.unreadable.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let mut listing = Listing::default();
        for item in read.flatten() {
            let file_name = item.file_name();
            let name = crate::name::from_os(&file_name);
            let name = name.as_ref();
            // The entry type comes with the name on most file systems.
            if let Ok(kind) = item.file_type()
                && !kind.is_symlink()
                && known(name, kind.is_dir())
            {
                listing.kept.push(name.to_owned());
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            if self.scope.is_excluded_at(segments, name) {
                continue;
            }
            let child_included = included || self.scope.is_included_at(segments, name);
            if meta.is_dir() {
                if !child_included && !self.scope.leads_to_include_at(segments, name) {
                    continue;
                }
                let child_path = path.join(&file_name);
                if self.skip.iter().any(|m| Path::new(m) == child_path) {
                    continue;
                }
                listing.folders.push(Subfolder {
                    owner: owner(&child_path, &meta),
                    path: child_path,
                    name: name.to_owned(),
                    mtime: settled(mtime_ns(&meta)),
                    included: child_included,
                });
            } else if child_included {
                listing.names.push_str(name);
                listing.files.push(FileRecord {
                    end: listing.names.len() as u32,
                    size: meta.len(),
                    mtime: mtime_ns(&meta),
                });
            }
        }
        let new = listing.files.len() + listing.folders.iter().filter(|f| f.included).count();
        self.progress.entries.fetch_add(new, Ordering::Relaxed);
        Some(listing)
    }

    pub(crate) fn walk(&self, path: &Path, segments: &[&str], sub: Subfolder) -> Folder {
        let Subfolder { name, mtime, owner, included, .. } = sub;
        let listing = if self.progress.cancelled() {
            Listing::default()
        } else {
            self.read(path, segments, included).unwrap_or_default()
        };
        let mut folder = Folder { name, mtime, owner: Some(owner), context: !included, listing, folders: Vec::new() };
        let subfolders = std::mem::take(&mut folder.listing.folders);
        folder.folders = subfolders
            .into_par_iter()
            .map(|sub| {
                let (name, path) = (sub.name.clone(), sub.path.clone());
                let mut child: Vec<&str> = segments.to_vec();
                child.push(&name);
                self.walk(&path, &child, sub)
            })
            .collect();
        folder
    }
}

/// Adds `folder` and everything below it; returns the number of folders.
fn flatten(b: &mut IndexBuilder, parent: u32, folder: Folder) -> usize {
    let id = b.push_folder_owned(parent, &folder.name, folder.mtime, folder.context, folder.owner);
    for (name, file) in folder.listing.files() {
        b.push_file(id, name, file.size, file.mtime);
    }
    1 + folder.folders.into_iter().map(|child| flatten(b, id, child)).sum::<usize>()
}

/// Scans what `scope` covers, skipping the mount points in `skip` (see
/// [`foreign_mounts`]).
pub fn scan(scope: &Scope, skip: &[String], progress: &Progress) -> (Index, ScanStats) {
    let start = Instant::now();
    let walker = Walker { scope, skip, progress };
    let root_meta = fs::metadata("/").ok();
    let root = Subfolder {
        path: PathBuf::from("/"),
        name: String::new(),
        mtime: root_meta.as_ref().map_or(0, |m| settled(mtime_ns(m))),
        owner: root_meta.as_ref().map_or(Owner { uid: 0, gid: 0, mode: 0o755, acl: false }, |m| owner(Path::new("/"), m)),
        included: scope.is_included(&[]),
    };
    let tree = walker.walk(Path::new("/"), &[], root);

    let mut b = IndexBuilder::with_capacity(progress.entries.load(Ordering::Relaxed) + 1);
    // The root has no parent and no name.
    let root = b.push_folder_owned(NO_PARENT, "", tree.mtime, false, tree.owner);
    for (name, file) in tree.listing.files() {
        b.push_file(root, name, file.size, file.mtime);
    }
    let folders = tree.folders.into_iter().map(|child| flatten(&mut b, root, child)).sum();
    let index = b.build();
    crate::give_back_memory();
    let stats = ScanStats {
        entries: index.searchable_len(),
        folders,
        unreadable: progress.unreadable.load(Ordering::Relaxed),
        duration: start.elapsed(),
        cancelled: progress.cancelled(),
    };
    (index, stats)
}

#[derive(Debug, Clone)]
pub struct ReconcileStats {
    pub folders_checked: usize,
    pub folders_changed: usize,
    pub duration: Duration,
    pub cancelled: bool,
}

/// What [`reconcile`] needs to copy the old index over.
struct Rebuild<'a> {
    old: &'a Index,
    walker: Walker<'a>,
    changed: Vec<bool>,
    now: Vec<i64>,
    child_start: Vec<u32>,
    children: Vec<u32>,
}

impl Rebuild<'_> {
    fn children_of(&self, id: u32) -> &[u32] {
        &self.children[self.child_start[id as usize] as usize..self.child_start[id as usize + 1] as usize]
    }

    /// Copies folder `id` of the old index under `parent`, reading it from
    /// disk again if it changed.
    /// `owner` is the folder's ownership as just read from disk, if known.
    fn copy(
        &self,
        b: &mut IndexBuilder,
        id: u32,
        parent: u32,
        path: &Path,
        segments: &mut Vec<String>,
        owner: Option<Owner>,
    ) {
        let old = self.old;
        let changed = self.changed[id as usize];
        let new = b.store.copy(parent, &old.store, id, changed.then(|| self.now[id as usize]));
        if let Some(owner) = owner {
            b.store.set_owner(new, owner);
        }
        if !changed {
            for &child in self.children_of(id) {
                if old.is_dead(child) {
                    continue;
                }
                if old.is_dir(child) {
                    let name = old.name(child).into_owned();
                    let child_path = path.join(crate::name::to_os(&name));
                    segments.push(name);
                    self.copy(b, child, new, &child_path, segments, None);
                    segments.pop();
                } else {
                    b.store.copy(new, &old.store, child, None);
                }
            }
            return;
        }
        // Read the folder again; keep subfolders that still exist and scan
        // the new ones.
        let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
        let included = if old.parent(id).is_none() {
            self.walker.scope.is_included(&[])
        } else {
            old.flags(id) & FLAG_CONTEXT == 0
        };
        let Some(mut listing) = self.walker.read(path, &refs, included) else { return };
        for (name, file) in listing.files() {
            b.push_file(new, name, file.size, file.mtime);
        }
        let known: HashMap<std::borrow::Cow<'_, str>, u32> = self
            .children_of(id)
            .iter()
            .filter(|&&c| old.flags(c) & (FLAG_DIR | FLAG_DEAD) == FLAG_DIR)
            .map(|&c| (old.name(c), c))
            .collect();
        let (kept, fresh): (Vec<Subfolder>, Vec<Subfolder>) =
            std::mem::take(&mut listing.folders).into_iter().partition(|sub| known.contains_key(sub.name.as_str()));
        let scanned: Vec<Folder> = fresh
            .into_par_iter()
            .map(|sub| {
                let (name, path) = (sub.name.clone(), sub.path.clone());
                let mut child = refs.clone();
                child.push(&name);
                self.walker.walk(&path, &child, sub)
            })
            .collect();
        for sub in kept {
            segments.push(sub.name.clone());
            self.copy(b, known[sub.name.as_str()], new, &sub.path, segments, Some(sub.owner));
            segments.pop();
        }
        for folder in scanned {
            flatten(b, new, folder);
        }
    }
}

/// Brings `old` up to date: every folder whose modification time changed is
/// read again, new folders are scanned, vanished ones dropped. Returns `None`
/// when nothing changed. Changes inside files that leave their folder alone
/// (a file growing, say) show up with the next full scan.
pub fn reconcile(
    old: &Index,
    scope: &Scope,
    skip: &[String],
    progress: &Progress,
) -> (Option<Index>, ReconcileStats) {
    let start = Instant::now();
    let n = old.len();
    let (child_start, children) = old.children(&(0..n as u32).collect::<Vec<_>>());

    // Every folder with its path.
    let roots: Vec<u32> = (0..n as u32).filter(|&id| old.parent(id).is_none()).collect();
    let mut folders: Vec<(u32, PathBuf)> = Vec::new();
    let mut stack: Vec<(u32, PathBuf)> = roots.iter().map(|&id| (id, PathBuf::from("/"))).collect();
    while let Some((id, path)) = stack.pop() {
        for &child in &children[child_start[id as usize] as usize..child_start[id as usize + 1] as usize] {
            if old.flags(child) & (FLAG_DIR | FLAG_DEAD) == FLAG_DIR {
                stack.push((child, path.join(crate::name::to_os(&old.name(child)))));
            }
        }
        folders.push((id, path));
    }

    // Compare modification times; a folder that is gone counts as a change
    // of its parent, which then no longer lists it.
    let current: Vec<Option<i64>> =
        folders.par_iter().map(|(_, path)| fs::symlink_metadata(path).ok().map(|m| mtime_ns(&m))).collect();
    let mut changed = vec![false; n];
    let mut now = vec![0i64; n];
    for ((id, _), mtime) in folders.iter().zip(&current) {
        let id = *id as usize;
        match mtime {
            Some(m) => {
                now[id] = settled(*m);
                if *m != old.store.mtime_ns(id as u32) {
                    changed[id] = true;
                }
            }
            None => {
                if let Some(parent) = old.parent(id as u32) {
                    changed[parent as usize] = true;
                }
            }
        }
    }
    let folders_checked = folders.len();
    finish(old, Walker { scope, skip, progress }, &folders, folders_checked, changed, now, child_start, children, roots, start)
}

/// Like [`reconcile`], for folders already known to have changed (from
/// file system events). A path that is not in the index stands for its
/// nearest folder that is.
pub fn update_folders(
    old: &Index,
    scope: &Scope,
    skip: &[String],
    progress: &Progress,
    dirty: &[PathBuf],
) -> (Option<Index>, ReconcileStats) {
    let start = Instant::now();
    let n = old.len();
    let (child_start, children) = old.children(&(0..n as u32).collect::<Vec<_>>());
    let roots: Vec<u32> = (0..n as u32).filter(|&id| old.parent(id).is_none()).collect();
    let children_of = |id: u32| &children[child_start[id as usize] as usize..child_start[id as usize + 1] as usize];
    // Walks down from the root; stops at the deepest folder that exists.
    let locate = |path: &Path| -> Option<(u32, PathBuf)> {
        let mut id = *roots.first()?;
        let mut found = PathBuf::from("/");
        for part in path.components() {
            let std::path::Component::Normal(part) = part else { continue };
            let name = crate::name::from_os(part);
            let next = children_of(id)
                .iter()
                .copied()
                .find(|&c| old.flags(c) & (FLAG_DIR | FLAG_DEAD) == FLAG_DIR && old.name(c) == name);
            match next {
                Some(next) => {
                    id = next;
                    found.push(part);
                }
                None => break,
            }
        }
        Some((id, found))
    };
    let mut changed = vec![false; n];
    let mut now = vec![0i64; n];
    let mut folders: Vec<(u32, PathBuf)> = Vec::new();
    for path in dirty {
        let Some((mut id, mut path)) = locate(path) else { continue };
        // Gone folders count as a change of their parent.
        loop {
            match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    now[id as usize] = settled(mtime_ns(&meta));
                    break;
                }
                Err(_) => match old.parent(id) {
                    Some(parent) => {
                        id = parent;
                        path.pop();
                    }
                    None => break,
                },
            }
        }
        if !changed[id as usize] {
            changed[id as usize] = true;
            folders.push((id, path));
        }
    }
    let checked = folders.len();
    finish(old, Walker { scope, skip, progress }, &folders, checked, changed, now, child_start, children, roots, start)
}

/// Shared by [`reconcile`] and [`update_folders`]: drops changes in folders
/// that only lead to included ones, then rebuilds.
#[allow(clippy::too_many_arguments)]
fn finish(
    old: &Index,
    walker: Walker<'_>,
    folders: &[(u32, PathBuf)],
    folders_checked: usize,
    mut changed: Vec<bool>,
    now: Vec<i64>,
    child_start: Vec<u32>,
    children: Vec<u32>,
    roots: Vec<u32>,
    start: Instant,
) -> (Option<Index>, ReconcileStats) {
    let progress = walker.progress;
    // Folders kept only on the way to an included one change all the time
    // (think of /tmp or /home); what matters there is only whether the
    // folders on the way are still the same.
    let segments_of = |id: u32| -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if old.parent(c).is_some() {
                chain.push(old.name(c).into_owned());
            }
            cur = old.parent(c);
        }
        chain.reverse();
        chain
    };
    for (id, path) in folders {
        let flags = old.flags(*id);
        if !changed[*id as usize] || flags & FLAG_CONTEXT == 0 {
            continue;
        }
        let segments = segments_of(*id);
        let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
        let quiet = Progress::default();
        let reader = Walker { progress: &quiet, ..walker };
        if let Some(listing) = reader.read(path, &refs, false) {
            let mut before: Vec<String> = children[child_start[*id as usize] as usize..child_start[*id as usize + 1] as usize]
                .iter()
                .filter(|&&c| old.flags(c) & (FLAG_DIR | FLAG_DEAD) == FLAG_DIR)
                .map(|&c| old.name(c).into_owned())
                .collect();
            let mut after: Vec<String> = listing.folders.iter().map(|f| f.name.clone()).collect();
            before.sort_unstable();
            after.sort_unstable();
            if before == after && listing.files.is_empty() {
                changed[*id as usize] = false;
            }
        }
    }

    let mut stats = ReconcileStats {
        folders_checked,
        folders_changed: changed.iter().filter(|&&c| c).count(),
        duration: start.elapsed(),
        cancelled: progress.cancelled(),
    };
    if stats.folders_changed == 0 || stats.cancelled {
        return (None, stats);
    }

    let rebuild = Rebuild { old, walker, changed, now, child_start, children };
    let mut b = IndexBuilder::reusing_names(old);
    for &root in &roots {
        rebuild.copy(&mut b, root, NO_PARENT, Path::new("/"), &mut Vec::new(), None);
    }
    stats.cancelled = progress.cancelled();
    if stats.cancelled {
        return (None, stats);
    }
    let index = b.build();
    crate::give_back_memory();
    stats.duration = start.elapsed();
    (Some(index), stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Query, SortKey};

    #[test]
    fn mountinfo() {
        let text = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
25 22 0:22 / /proc rw - proc proc rw
26 22 0:5 / /dev rw - devtmpfs udev rw
40 22 8:1 / /boot/efi rw - vfat /dev/sda1 rw
41 22 0:40 / /mnt/my\\040share rw - cifs //nas/share rw
42 22 0:41 / /run/user/1000/doc rw - fuse.portal portal rw";
        assert_eq!(foreign_mounts(text), ["/proc", "/dev", "/mnt/my share", "/run/user/1000/doc"]);
        let home = "71 22 0:50 / /home/me rw - ecryptfs /home/me/.Private rw,ecryptfs_sig=0";
        assert_eq!(ecryptfs_mounts(&format!("{text}\n{home}")), [("/home/me/.Private".to_owned(), "/home/me".to_owned())]);
        assert!(foreign_mounts(home).is_empty());
    }

    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("anything-scan-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["docs/2024", "docs/node_modules/pkg", "photos", "skip-me"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "docs/Rechnung-1.pdf",
            "docs/2024/Rechnung-2.pdf",
            "docs/node_modules/pkg/index.js",
            "photos/IMG_1.jpg",
            "skip-me/secret.txt",
        ] {
            fs::write(root.join(file), b"x").unwrap();
        }
        std::os::unix::fs::symlink(root.join("docs"), root.join("link-to-docs")).unwrap();
        root
    }

    fn relative_paths(index: &Index, base: &str) -> Vec<String> {
        index
            .search(&Query::parse(""), SortKey::Path, false)
            .iter()
            .map(|&id| index.path(id).strip_prefix(base).unwrap_or("?").to_owned())
            .collect()
    }

    fn scope_for(base: &str) -> Scope {
        Scope::new([base], [format!("{base}/skip-me"), "node_modules".into()])
    }

    #[test]
    fn scans_by_scope() {
        let root = tree("scope");
        let base = root.to_str().unwrap().to_owned();
        let (index, stats) = scan(&scope_for(&base), &[], &Progress::default());
        assert_eq!(stats.unreadable, 0);
        assert_eq!(
            relative_paths(&index, &base),
            [
                "",
                "/docs",
                "/docs/2024",
                "/docs/2024/Rechnung-2.pdf",
                "/docs/Rechnung-1.pdf",
                "/link-to-docs",
                "/photos",
                "/photos/IMG_1.jpg",
            ]
        );
        // The symlink is recorded, not followed.
        let link = index.search(&Query::parse("link"), SortKey::Name, false);
        assert!(!index.is_dir(link[0]));
        assert_eq!(stats.entries, 8);
        fs::remove_dir_all(root).unwrap();
    }

    /// Sets every folder's time an hour back: a tree made just now has
    /// times too fresh to trust (see [`settled`]).
    fn age(root: &Path) {
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                }
            }
            fs::File::open(&dir).unwrap().set_modified(hour_ago).unwrap();
        }
    }

    #[test]
    fn reconcile_reads_only_what_changed() {
        let root = tree("reconcile");
        age(&root);
        let base = root.to_str().unwrap().to_owned();
        let scope = scope_for(&base);
        let (index, _) = scan(&scope, &[], &Progress::default());

        let (same, stats) = reconcile(&index, &scope, &[], &Progress::default());
        assert!(same.is_none());
        assert_eq!(stats.folders_changed, 0);

        fs::write(root.join("docs/2024/Rechnung-3.pdf"), b"x").unwrap();
        fs::remove_file(root.join("photos/IMG_1.jpg")).unwrap();
        fs::create_dir_all(root.join("music/live")).unwrap();
        fs::write(root.join("music/live/song.mp3"), b"x").unwrap();
        fs::remove_dir_all(root.join("docs/node_modules")).unwrap();

        let (updated, stats) = reconcile(&index, &scope, &[], &Progress::default());
        let updated = updated.expect("changes found");
        assert!(stats.folders_changed >= 3, "{stats:?}");
        let fresh = scan(&scope, &[], &Progress::default()).0;
        assert_eq!(relative_paths(&updated, &base), relative_paths(&fresh, &base));
        assert!(relative_paths(&updated, &base).contains(&"/music/live/song.mp3".to_owned()));

        // Once the changed folders' times are old enough to trust, and they
        // have been read once more, everything is current.
        age(&root);
        let settled = reconcile(&updated, &scope, &[], &Progress::default()).0.unwrap_or(updated);
        assert!(reconcile(&settled, &scope, &[], &Progress::default()).0.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_folders_reads_only_the_given_ones() {
        let root = tree("update");
        let base = root.to_str().unwrap().to_owned();
        let scope = scope_for(&base);
        let (index, _) = scan(&scope, &[], &Progress::default());

        fs::write(root.join("docs/2024/Rechnung-3.pdf"), b"x").unwrap();
        fs::create_dir_all(root.join("new/deep")).unwrap();
        fs::write(root.join("new/deep/a.txt"), b"x").unwrap();
        // A change nobody reports is not seen...
        fs::write(root.join("photos/IMG_2.jpg"), b"x").unwrap();
        let dirty = [root.join("docs/2024"), root.join("new/deep")];
        let (updated, stats) = update_folders(&index, &scope, &[], &Progress::default(), &dirty);
        let updated = updated.expect("changes");
        assert_eq!(stats.folders_changed, 2);
        let paths = relative_paths(&updated, &base);
        assert!(paths.contains(&"/docs/2024/Rechnung-3.pdf".to_owned()));
        // ...the unknown "new/deep" stands for "new"'s parent, which is read.
        assert!(paths.contains(&"/new/deep/a.txt".to_owned()));
        assert!(!paths.contains(&"/photos/IMG_2.jpg".to_owned()));
        // A file growing is seen when its folder is reported.
        fs::write(root.join("docs/Rechnung-1.pdf"), b"xxxx").unwrap();
        let (grown, _) = update_folders(&updated, &scope, &[], &Progress::default(), &[root.join("docs")]);
        let grown = grown.unwrap();
        let hit = grown.search(&Query::parse("Rechnung-1"), SortKey::Name, false)[0];
        assert_eq!(grown.size(hit), 4);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_names_that_are_not_utf8() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let root = std::env::temp_dir().join(format!("anything-scan-bytes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let folder = root.join(OsStr::from_bytes(b"alt-\xfc"));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("Rechnung.pdf"), b"x").unwrap();
        fs::write(root.join(OsStr::from_bytes(b"Gr\xfc\xdfe.txt")), b"x").unwrap();
        fs::write(root.join("a\u{10FF41}b.txt"), b"x").unwrap();
        let base = root.to_str().unwrap().to_owned();
        let scope = Scope::new([&base], Vec::<String>::new());
        let (mut index, stats) = scan(&scope, &[], &Progress::default());
        assert_eq!(stats.unreadable, 0);
        assert_eq!(stats.entries, 5);
        // Every path from the index leads back to the file on disk.
        let all = index.search(&Query::parse(""), SortKey::Path, false);
        for &id in &all {
            assert!(crate::name::to_path(&index.path(id)).symlink_metadata().is_ok());
        }
        let hit = index.search(&Query::parse("rechnung"), SortKey::Name, false);
        assert_eq!(crate::name::to_path(&index.path(hit[0])), folder.join("Rechnung.pdf"));
        assert_eq!(index.search(&Query::parse("*.txt"), SortKey::Name, false).len(), 2);

        // Live changes inside such a folder are taken in...
        fs::write(folder.join("neu.pdf"), b"x").unwrap();
        index.prepare();
        let patch = crate::update::plan(&index, &scope, &[], std::slice::from_ref(&folder));
        index.apply(patch);
        assert_eq!(index.search(&Query::parse("*.pdf"), SortKey::Name, false).len(), 2);
        // ...and so are those found by checking all folders.
        fs::write(folder.join("dritte.pdf"), b"x").unwrap();
        let (updated, _) = reconcile(&index, &scope, &[], &Progress::default());
        let updated = updated.expect("change found");
        assert_eq!(updated.search(&Query::parse("*.pdf"), SortKey::Name, false).len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_folder_times_are_checked_again() {
        let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let mtime = 1_000 * NANOS + 500;
        // Read in the same second: kept off by a nanosecond.
        assert_eq!(settled_at(mtime, at(1_000)), mtime - 1);
        assert_eq!(settled_at(mtime, at(1_001)), mtime - 1);
        // Never across a second.
        assert_eq!(settled_at(1_000 * NANOS, at(1_000)), 1_000 * NANOS + 1);
        // Long enough ago: kept as it is.
        assert_eq!(settled_at(mtime, at(1_003)), mtime);
        assert_eq!(settled_at(mtime, at(5_000)), mtime);
    }

    #[test]
    fn cancel_stops_early() {
        let progress = Progress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        let (_, stats) = scan(&Scope::new(["/"], std::iter::empty::<&str>()), &[], &progress);
        assert!(stats.cancelled);
        assert_eq!(stats.entries, 0);
    }
}
