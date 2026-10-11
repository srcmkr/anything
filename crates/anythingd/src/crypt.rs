//! Live changes in encrypted home folders (eCryptfs).
//!
//! fanotify cannot watch an eCryptfs mount itself, but it sees the encrypted
//! folder underneath it, which lies on an ordinary file system and changes
//! along with it, under scrambled names. eCryptfs gives every folder the
//! inode number of its encrypted twin, so a change reported underneath is
//! turned into the readable folder through a table of inode numbers.

use std::{
    collections::HashMap,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

struct Home {
    /// The encrypted folder, as the kernel names it in events.
    lower: PathBuf,
    /// Where it is readable.
    upper: PathBuf,
    /// Folder inode number -> readable folder.
    folders: HashMap<u64, PathBuf>,
}

#[derive(Default)]
pub struct Crypt {
    homes: Vec<Home>,
    /// Readable folder -> when its names were read, and its entries by inode
    /// number. Only the names, no file looked at.
    names: HashMap<PathBuf, (std::time::Instant, HashMap<u64, std::ffi::OsString>)>,
}

/// Folders whose entries are kept by inode number at most.
const NAMES_KEPT: usize = 64;
/// A folder's names are read again at most this often: a file unknown so
/// soon after is new, and its folder's "created" report takes care of it.
const NAMES_FRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// Every folder below `root` on the same file system, by inode number.
fn folders_below(root: &Path, into: &mut HashMap<u64, PathBuf>) {
    let Ok(meta) = std::fs::symlink_metadata(root) else { return };
    let device = meta.dev();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&dir) else { continue };
        if !meta.is_dir() || meta.dev() != device {
            continue;
        }
        into.insert(meta.ino(), dir.clone());
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
    }
}

/// The inode number at `path`, if it is a folder.
fn folder_ino(path: &Path) -> Option<u64> {
    std::fs::symlink_metadata(path).ok().filter(|m| m.is_dir()).map(|m| m.ino())
}

impl Crypt {
    /// Finds the encrypted home folders on this machine and reads their
    /// folders (about a second for a quarter of a million).
    pub fn here() -> Self {
        let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else { return Self::default() };
        let mut crypt = Self::default();
        for (lower, upper) in anything_core::scan::ecryptfs_mounts(&text) {
            if let Ok(lower) = std::fs::canonicalize(&lower) {
                crypt.add(lower, PathBuf::from(upper));
            }
        }
        crypt
    }

    fn add(&mut self, lower: PathBuf, upper: PathBuf) {
        let mut folders = HashMap::new();
        folders_below(&upper, &mut folders);
        self.homes.push(Home { lower, upper, folders });
    }

    /// For a file `name` written in the encrypted folder `dir` (inode `ino`):
    /// the readable file.
    pub fn readable_file(&mut self, dir: &Path, name: &std::ffi::OsStr, ino: u64) -> Option<PathBuf> {
        let folder = self.readable(dir, ino)?;
        let file_ino = std::fs::symlink_metadata(dir.join(name)).ok()?.ino();
        match self.names.get(&folder) {
            Some((_, names)) if names.contains_key(&file_ino) => return Some(folder.join(&names[&file_ino])),
            Some((read, _)) if read.elapsed() < NAMES_FRESH => return None,
            _ => {}
        }
        // Not known yet: read the folder's names once more.
        use std::os::unix::fs::DirEntryExt as _;
        let names: HashMap<u64, std::ffi::OsString> =
            std::fs::read_dir(&folder).ok()?.flatten().map(|e| (e.ino(), e.file_name())).collect();
        let found = names.get(&file_ino).map(|name| folder.join(name));
        if self.names.len() >= NAMES_KEPT {
            self.names.clear();
        }
        self.names.insert(folder, (std::time::Instant::now(), names));
        found
    }

    pub fn count(&self) -> usize {
        self.homes.len()
    }

    /// Whether `path` lies on the encrypted side of a home folder.
    pub fn is_encrypted_side(&self, path: &Path) -> bool {
        self.homes.iter().any(|h| path.starts_with(&h.lower))
    }

    /// For a folder `path` with inode number `ino` reported by fanotify: the
    /// readable folder to read again if it lies in an encrypted folder,
    /// `None` if it does not.
    pub fn readable(&mut self, path: &Path, ino: u64) -> Option<PathBuf> {
        let home = self.homes.iter_mut().find(|h| path.starts_with(&h.lower))?;
        let known = |home: &Home, ino: u64| {
            home.folders.get(&ino).filter(|p| folder_ino(p) == Some(ino)).cloned()
        };
        if let Some(folder) = known(home, ino) {
            return Some(folder);
        }
        // A folder the table does not know (new, or moved): the nearest one
        // above it that it knows. Reading that again finds the new one; its
        // folders are added to the table.
        let found = path
            .ancestors()
            .skip(1)
            .take_while(|a| a.starts_with(&home.lower))
            .find_map(|a| folder_ino(a).and_then(|ino| known(home, ino)))
            .unwrap_or_else(|| home.upper.clone());
        folders_below(&found, &mut home.folders);
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_folders_by_inode() {
        // Without eCryptfs, one folder plays both sides: same inode numbers.
        let root = std::env::temp_dir().join(format!("anythingd-crypt-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let mut crypt = Crypt::default();
        crypt.add(root.clone(), root.clone());
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();

        let b = root.join("a/b");
        assert_eq!(crypt.readable(&b, ino(&b)), Some(b.clone()));
        // A file, found by its inode number.
        std::fs::write(b.join("file"), b"x").unwrap();
        assert_eq!(crypt.readable_file(&b, std::ffi::OsStr::new("file"), ino(&b)), Some(b.join("file")));
        // Made after the table: found through the folder above it, then known.
        let c = root.join("a/b/c");
        std::fs::create_dir(&c).unwrap();
        assert_eq!(crypt.readable(&c, ino(&c)), Some(b.clone()));
        assert_eq!(crypt.readable(&c, ino(&c)), Some(c.clone()));
        // Renamed: the old entry no longer matches.
        let d = root.join("a/d");
        std::fs::rename(&c, &d).unwrap();
        assert_eq!(crypt.readable(&d, ino(&d)), Some(root.join("a")));
        // Outside every encrypted folder.
        assert_eq!(crypt.readable(Path::new("/usr"), ino(Path::new("/usr"))), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
