//! The socket: one thread per connection, each with its own last result.

use std::{
    collections::HashMap,
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    sync::{Arc, Mutex, atomic::Ordering},
    time::Instant,
};

use anything_core::{Index, Query, Scope, Selection, SortKey, access::Viewer};
use anything_proto::{
    MAX_REQUEST, Request, Response, Row, Sort, VERSION, read_frame, write_frame,
};

use crate::Shared;

/// Connections at once, in total and per user.
const MAX_CONNECTIONS: usize = 64;
const MAX_PER_USER: usize = 8;
/// Visibility masks kept for reuse.
const MASKS_KEPT: usize = 8;

pub fn listen(socket: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    }
    // A socket left over from a crash: remove it, but only if it is one and
    // no other service still answers on it.
    if let Ok(meta) = std::fs::symlink_metadata(socket) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() {
            if UnixStream::connect(socket).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "another anythingd is already running"));
            }
            std::fs::remove_file(socket)?;
        }
    }
    let listener = UnixListener::bind(socket)?;
    // Every local user may ask; each only sees what they could list.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Who is on the other end, from the kernel (`SO_PEERCRED`).
fn peer(stream: &UnixStream) -> io::Result<(Viewer, i32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if ok != 0 {
        return Err(io::Error::last_os_error());
    }
    // Supplementary groups of the connecting process.
    let mut groups = vec![cred.gid];
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", cred.pid))
        && let Some(line) = status.lines().find(|l| l.starts_with("Groups:"))
    {
        groups.extend(line[7..].split_whitespace().filter_map(|g| g.parse::<u32>().ok()));
    }
    groups.sort_unstable();
    groups.dedup();
    Ok((Viewer { uid: cred.uid, groups }, cred.pid))
}

#[derive(Clone, PartialEq, Eq)]
struct MaskKey {
    viewer: Viewer,
    include: Vec<String>,
    exclude: Vec<String>,
}

struct MaskEntry {
    key: MaskKey,
    /// The index and rights it was made for, and how many entries it covers.
    epoch: u64,
    permissions: u64,
    upto: usize,
    bits: Arc<Vec<u64>>,
}

/// Recently used visibility masks. They depend on the index, the user and
/// the user's filter; live updates only extend them.
#[derive(Default)]
pub struct MaskCache(Mutex<Vec<MaskEntry>>);

impl MaskCache {
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn get(&self, index: &Index, epoch: u64, permissions: u64, key: MaskKey) -> Arc<Vec<u64>> {
        let n = index.len();
        {
            let mut kept = self.0.lock().unwrap();
            if let Some(entry) = kept.iter_mut().find(|e| e.key == key)
                && entry.epoch == epoch && entry.permissions == permissions
            {
                if entry.upto < n {
                    let scope = Scope::new(&entry.key.include, &entry.key.exclude);
                    let bits = Arc::make_mut(&mut entry.bits);
                    anything_core::access::extend(index, Some(&entry.key.viewer), Some(&scope), bits, entry.upto);
                    entry.upto = n;
                }
                return entry.bits.clone();
            }
        }
        let scope = Scope::new(&key.include, &key.exclude);
        let bits = Arc::new(anything_core::access::mask(index, Some(&key.viewer), Some(&scope)));
        let mut kept = self.0.lock().unwrap();
        kept.retain(|e| e.key != key && e.epoch == epoch);
        if kept.len() >= MASKS_KEPT {
            kept.remove(0);
        }
        kept.push(MaskEntry { key, epoch, permissions, upto: n, bits: bits.clone() });
        bits
    }
}

pub fn serve(listener: UnixListener, shared: Arc<Shared>) {
    let open: Arc<Mutex<HashMap<u32, usize>>> = Arc::default();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok((viewer, _pid)) = peer(&stream) else { continue };
        {
            let mut open = open.lock().unwrap();
            let total: usize = open.values().sum();
            let mine = open.entry(viewer.uid).or_default();
            if total >= MAX_CONNECTIONS || *mine >= MAX_PER_USER {
                continue;
            }
            *mine += 1;
        }
        let (conn_shared, conn_open) = (shared.clone(), open.clone());
        let uid = viewer.uid;
        let spawned = std::thread::Builder::new().name("connection".into()).spawn(move || {
            Connection { shared: conn_shared, viewer, last: None }.run(stream);
            *conn_open.lock().unwrap().entry(uid).or_default() -= 1;
        });
        if spawned.is_err() {
            *open.lock().unwrap().entry(uid).or_default() -= 1;
        }
    }
}

struct Last {
    query: String,
    sort: Sort,
    descending: bool,
    include: Vec<String>,
    exclude: Vec<String>,
    generation: u64,
    selection: Selection,
}

struct Connection {
    shared: Arc<Shared>,
    viewer: Viewer,
    /// The last search, to fetch rows from and to repeat when the index
    /// changed meanwhile.
    last: Option<Last>,
}

fn sort_key(sort: Sort) -> SortKey {
    match sort {
        Sort::Name => SortKey::Name,
        Sort::Path => SortKey::Path,
        Sort::Size => SortKey::Size,
        Sort::Modified => SortKey::Modified,
    }
}

impl Connection {
    fn select(&self, index: &Index, query: &str, sort: Sort, descending: bool, include: &[String], exclude: &[String]) -> Selection {
        let shared = &self.shared;
        let epoch = shared.epoch.load(Ordering::Acquire);
        let permissions = shared.permissions.load(Ordering::Acquire);
        let key = MaskKey { viewer: self.viewer.clone(), include: include.to_vec(), exclude: exclude.to_vec() };
        let mask = shared.masks.get(index, epoch, permissions, key);
        index.select(&Query::parse(query), sort_key(sort), descending, Some(&mask))
    }

    fn run(&mut self, mut stream: UnixStream) {
        loop {
            let request = match read_frame(&mut stream, MAX_REQUEST) {
                Ok(Some(frame)) => Request::decode(&frame),
                Ok(None) | Err(_) => return,
            };
            let response = match request {
                Ok(request) => self.answer(request),
                Err(err) => {
                    // Malformed input: say why, then hang up.
                    let reply = Response::Error { message: err.to_string() };
                    write_frame(&mut stream, &reply.encode()).ok();
                    return;
                }
            };
            if write_frame(&mut stream, &response.encode()).is_err() {
                return;
            }
        }
    }

    fn answer(&mut self, request: Request) -> Response {
        let shared = &self.shared;
        match request {
            Request::Hello { .. } => Response::Hello { version: VERSION },
            Request::Status => {
                let state = *shared.state.lock().unwrap();
                let entries = match state {
                    anything_proto::State::Scanning => shared.progress.entries.load(Ordering::Relaxed) as u64,
                    _ => shared.index.read().unwrap().searchable_len() as u64,
                };
                Response::Status { state, entries, generation: shared.generation.load(Ordering::Acquire) }
            }
            Request::Search { query, sort, descending, include, exclude } => {
                let start = Instant::now();
                let index = shared.index.read().unwrap();
                let generation = shared.generation.load(Ordering::Acquire);
                let selection = self.select(&index, &query, sort, descending, &include, &exclude);
                let total = selection.len() as u32;
                self.last = Some(Last { query, sort, descending, include, exclude, generation, selection });
                Response::Searched {
                    total,
                    micros: start.elapsed().as_micros().min(u32::MAX as u128) as u32,
                    generation,
                }
            }
            Request::Rows { start, count } => {
                let index = shared.index.read().unwrap();
                let generation = shared.generation.load(Ordering::Acquire);
                let Some(mut last) = self.last.take() else {
                    return Response::Error { message: "search first".into() };
                };
                // The index changed since: the selection points into old
                // positions, so repeat the search first.
                if last.generation != generation {
                    last.selection = self.select(&index, &last.query, last.sort, last.descending, &last.include, &last.exclude);
                    last.generation = generation;
                }
                let rows = index
                    .selected(&last.selection, start as usize, count as usize)
                    .into_iter()
                    .map(|id| Row {
                        name: index.name(id).into_owned(),
                        dir: index.dir_path(id),
                        is_dir: index.is_dir(id),
                        size: index.size(id),
                        modified: index.modified(id),
                    })
                    .collect();
                self.last = Some(last);
                Response::Rows { start, rows }
            }
        }
    }
}
