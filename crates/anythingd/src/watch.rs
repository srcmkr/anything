//! Live changes through fanotify: one mark per local file system reports
//! every file and folder that is created, deleted, moved, changed in its
//! attributes or written. Each event names the folder it happened in; those
//! folders are collected and read again in batches.
//!
//! Needs root (`CAP_SYS_ADMIN` for the marks, `CAP_DAC_READ_SEARCH` to turn
//! the reported folder handles back into paths). Encrypted home folders are
//! followed through the folder underneath them (see `crypt`).

use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// What the watcher found since it was last asked.
#[derive(Default)]
pub struct Changes {
    /// Folders to read again, with the names reported in them; `None` when
    /// a report named nothing, so all of the folder is looked at.
    pub folders: Mutex<HashMap<PathBuf, Option<HashSet<String>>>>,
    /// Files that were only written: read just them, not their folders.
    pub files: Mutex<HashSet<PathBuf>>,
    /// The kernel dropped events: everything has to be checked.
    pub overflow: AtomicBool,
}

impl Changes {
    pub fn take(&self) -> Vec<(PathBuf, Option<HashSet<String>>)> {
        self.folders.lock().unwrap().drain().collect()
    }

    /// Notes a change in `folder`: of the entry `name`, of some entry that
    /// shows by itself (`Some(None)`: new or gone), or of anything (`None`).
    fn note(&self, changed: Vec<(PathBuf, Option<Option<String>>)>) {
        let mut folders = self.folders.lock().unwrap();
        for (folder, name) in changed {
            let entry = folders.entry(folder).or_insert_with(|| Some(HashSet::new()));
            match (entry.as_mut(), name) {
                (Some(names), Some(Some(name))) => {
                    names.insert(name);
                }
                (Some(_), Some(None)) | (None, _) => {}
                (Some(_), None) => *entry = None,
            }
        }
    }

    pub fn take_files(&self) -> Vec<PathBuf> {
        self.files.lock().unwrap().drain().collect()
    }
}

/// File systems mounted somewhere, once each, with their mount points.
fn local_mounts() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else { return Vec::new() };
    let foreign: HashSet<String> = anything_core::scan::foreign_mounts(&text).into_iter().collect();
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|m| m.replace("\\040", " "))
        .filter(|m| !foreign.contains(m))
        .collect()
}

fn cstring(path: &str) -> io::Result<CString> {
    CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

type Fsid = [i32; 2];

fn fsid_of(fd: &OwnedFd) -> io::Result<Fsid> {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // fsid_t keeps its two ints private; its layout is two i32.
    Ok(unsafe { std::mem::transmute::<libc::fsid_t, Fsid>(stat.f_fsid) })
}

/// Starts watching every local file system. Fails without the needed
/// rights; the caller then falls back to checking periodically.
pub fn start(changes: Arc<Changes>) -> io::Result<usize> {
    let fd = unsafe {
        libc::fanotify_init(
            libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_REPORT_DFID_NAME,
            (libc::O_RDONLY | libc::O_LARGEFILE | libc::O_CLOEXEC) as u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fan = unsafe { OwnedFd::from_raw_fd(fd) };
    let mask = libc::FAN_CREATE
        | libc::FAN_DELETE
        | libc::FAN_MOVED_FROM
        | libc::FAN_MOVED_TO
        | libc::FAN_ATTRIB
        | libc::FAN_CLOSE_WRITE
        | libc::FAN_ONDIR;

    // One mark per file system; one open folder per file system to resolve
    // handles against.
    let mut mounts: HashMap<Fsid, OwnedFd> = HashMap::new();
    for mount in local_mounts() {
        let path = cstring(&mount)?;
        let dir = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if dir < 0 {
            continue;
        }
        let dir = unsafe { OwnedFd::from_raw_fd(dir) };
        let Ok(fsid) = fsid_of(&dir) else { continue };
        if mounts.contains_key(&fsid) {
            continue;
        }
        let marked = unsafe {
            libc::fanotify_mark(
                fan.as_raw_fd(),
                libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                mask,
                libc::AT_FDCWD,
                path.as_ptr(),
            )
        };
        if marked != 0 {
            let err = io::Error::last_os_error();
            // Without the right to mark there is no point going on.
            if err.raw_os_error() == Some(libc::EPERM) {
                return Err(err);
            }
            log!("anythingd: {mount} cannot be watched live ({err})");
            continue;
        }
        mounts.insert(fsid, dir);
    }
    if mounts.is_empty() {
        return Err(io::Error::other("no file system could be watched"));
    }
    let count = mounts.len();
    std::thread::Builder::new()
        .name("watch".into())
        .spawn(move || {
            let crypt = crate::crypt::Crypt::here();
            if crypt.count() > 0 {
                log!("anythingd: following {} encrypted home folder(s) through their encrypted side", crypt.count());
            }
            read_events(fan, mounts, crypt, changes)
        })?;
    Ok(count)
}

fn read_events(fan: OwnedFd, mounts: HashMap<Fsid, OwnedFd>, mut crypt: crate::crypt::Crypt, changes: Arc<Changes>) {
    let mut buffer = vec![0u8; 256 * 1024];
    let meta_len = std::mem::size_of::<libc::fanotify_event_metadata>();
    loop {
        let read = unsafe { libc::read(fan.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log!("anythingd: watching stopped: {}", io::Error::last_os_error());
            changes.overflow.store(true, Ordering::Relaxed);
            return;
        }
        let mut at = 0usize;
        let end = read as usize;
        let mut found: Vec<(PathBuf, Option<Option<String>>)> = Vec::new();
        let mut written: Vec<PathBuf> = Vec::new();
        while at + meta_len <= end {
            let meta: libc::fanotify_event_metadata =
                unsafe { std::ptr::read_unaligned(buffer[at..].as_ptr().cast()) };
            let event_len = meta.event_len as usize;
            if event_len < meta_len || at + event_len > end {
                break;
            }
            if meta.mask & libc::FAN_Q_OVERFLOW != 0 {
                changes.overflow.store(true, Ordering::Relaxed);
            } else if let Some((dir, ino, name)) = folder_of(&buffer[at + meta.metadata_len as usize..at + event_len], &mounts) {
                // Created, removed or moved: the folder is read again. A
                // file only written: just that file.
                let structural = structural(meta.mask);
                let on_folder = meta.mask & libc::FAN_ONDIR != 0;
                if crypt.is_encrypted_side(&dir) {
                    // A change underneath an encrypted home folder is a change
                    // in it. Reading there touches the encrypted files and
                    // folders (reported as written and changed), so only
                    // what really moved counts.
                    if structural {
                        if let Some(folder) = crypt.readable(&dir, ino) {
                            // The names there are scrambled; new and removed
                            // entries show by themselves when the folder's
                            // names are listed. (An entry moved over an
                            // existing one keeps its old size and time until
                            // the next full check.)
                            found.push((folder, Some(None)));
                        }
                    } else if !on_folder
                        && let Some(name) = name.as_deref()
                        && written_lately(&dir, Some(name))
                    {
                        written.extend(crypt.readable_file(&dir, name, ino));
                    }
                } else if !structural && !on_folder && let Some(name) = name {
                    written.push(dir.join(name));
                } else {
                    let name = name.map(|n| Some(anything_core::name::from_os(&n).into_owned()));
                    found.push((dir, name));
                }
            }
            if meta.fd >= 0 {
                unsafe { libc::close(meta.fd) };
            }
            at += event_len;
        }
        if !found.is_empty() {
            changes.note(found);
        }
        if !written.is_empty() {
            changes.files.lock().unwrap().extend(written);
        }
    }
}

/// Something was created, removed or moved.
fn structural(mask: u64) -> bool {
    mask & (libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO) != 0
}

/// Whether the file `name` in `dir` was really written in the last minute.
///
/// Reading a folder in an encrypted home (to follow a change) makes eCryptfs
/// open its encrypted files for writing and touch their attributes; that is
/// reported as "attributes changed" and "closed after writing", and would be
/// read again, without end. A real write moves the modification time.
fn written_lately(dir: &std::path::Path, name: Option<&std::ffi::OsStr>) -> bool {
    let Some(name) = name else { return false };
    std::fs::symlink_metadata(dir.join(name))
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 60))
}

/// The folder an event happened in, its inode number and the name of the
/// entry concerned, from its "directory handle + name" record.
fn folder_of(info: &[u8], mounts: &HashMap<Fsid, OwnedFd>) -> Option<(PathBuf, u64, Option<std::ffi::OsString>)> {
    // struct fanotify_event_info_header { u8 type; u8 pad; u16 len; }
    let mut at = 0usize;
    while at + 4 <= info.len() {
        let kind = info[at];
        let len = u16::from_ne_bytes([info[at + 2], info[at + 3]]) as usize;
        if len < 4 || at + len > info.len() {
            return None;
        }
        if kind == libc::FAN_EVENT_INFO_TYPE_DFID_NAME || kind == libc::FAN_EVENT_INFO_TYPE_DFID {
            let record = &info[at..at + len];
            // header (4) + fsid (8) + struct file_handle { u32 bytes; i32 type; u8 handle[] }
            if record.len() < 4 + 8 + 8 {
                return None;
            }
            let fsid: Fsid = [
                i32::from_ne_bytes(record[4..8].try_into().unwrap()),
                i32::from_ne_bytes(record[8..12].try_into().unwrap()),
            ];
            let handle_bytes = u32::from_ne_bytes(record[12..16].try_into().unwrap()) as usize;
            if 12 + 8 + handle_bytes > record.len() {
                return None;
            }
            let mount = mounts.get(&fsid)?;
            // open_by_handle_at wants an aligned struct file_handle.
            let mut handle = vec![0u64; (8 + handle_bytes).div_ceil(8)];
            unsafe {
                std::ptr::copy_nonoverlapping(record[12..].as_ptr(), handle.as_mut_ptr().cast::<u8>(), 8 + handle_bytes);
            }
            let fd = unsafe {
                libc::open_by_handle_at(mount.as_raw_fd(), handle.as_mut_ptr().cast(), libc::O_PATH | libc::O_CLOEXEC)
            };
            if fd < 0 {
                // The folder is gone already; its parent will report it.
                return None;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // A folder deleted meanwhile has no links left (its path would
            // read as "… (deleted)", which a real name may also end with).
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 || stat.st_nlink == 0 {
                return None;
            }
            let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
            // DFID_NAME: a NUL-terminated name follows the handle.
            let name = (kind == libc::FAN_EVENT_INFO_TYPE_DFID_NAME)
                .then(|| record.get(12 + 8 + handle_bytes..))
                .flatten()
                .and_then(|rest| rest.split(|&b| b == 0).next())
                .filter(|n| !n.is_empty() && *n != b".")
                .map(|n| <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(n).to_os_string());
            return Some((path, stat.st_ino, name));
        }
        at += len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_writes_count() {
        let dir = std::env::temp_dir().join(format!("anythingd-written-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fresh"), b"x").unwrap();
        let old = std::fs::File::create(dir.join("old")).unwrap();
        old.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
        assert!(written_lately(&dir, Some(std::ffi::OsStr::new("fresh"))));
        assert!(!written_lately(&dir, Some(std::ffi::OsStr::new("old"))));
        assert!(!written_lately(&dir, Some(std::ffi::OsStr::new("missing"))));
        assert!(!written_lately(&dir, None));
        assert!(structural(libc::FAN_CREATE | libc::FAN_ONDIR));
        assert!(!structural(libc::FAN_ATTRIB | libc::FAN_CLOSE_WRITE));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
