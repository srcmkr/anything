//! `anythingd`: keeps the file name index of this machine and answers
//! searches over a Unix socket.
//!
//! Run as root by systemd it indexes every local file system and shows each
//! user only what that user could list. Run as a normal user (for testing)
//! it indexes what that user can read.
//!
//! The log carries numbers only, never file names or search terms.

/// Like `eprintln!`, but never fails: a closed log must not stop the service.
macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

mod crypt;
mod manage;
mod server;
mod watch;

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anything_core::{Index, Scope, scan, scope_tag, update};
use anything_proto::State;
use serde::Deserialize;

/// `/etc/anything/anythingd.toml`.
#[derive(Deserialize)]
#[serde(default)]
struct Config {
    /// Folders to index.
    include: Vec<String>,
    /// Folders and names to leave out.
    exclude: Vec<String>,
    /// Leave out the folders programs keep their caches in.
    skip_caches: bool,
    /// Leave out `node_modules` folders.
    skip_node_modules: bool,
    /// Seconds between checks for changed folders, without live updates.
    reconcile_secs: u64,
    /// Live changes are taken in at most this often; whatever happens in
    /// between is collected into one update.
    batch_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            include: vec!["/".into()],
            exclude: Vec::new(),
            skip_caches: true,
            skip_node_modules: true,
            reconcile_secs: 120,
            batch_secs: 5,
        }
    }
}

struct Options {
    socket: PathBuf,
    database: PathBuf,
    config: Config,
}

fn usage() -> ! {
    log!(
        "usage: anythingd [--socket PATH] [--database DIR] [--config FILE] \
         [--include DIR]... [--exclude PATTERN]... [--reconcile-secs N]\n       \
         anythingd --install | --uninstall | --start | --stop | --restart
         | --install-app | --update   (as root)"
    );
    std::process::exit(2)
}

fn options() -> Options {
    let mut socket = PathBuf::from(anything_proto::SOCKET);
    let mut database = PathBuf::from("/var/lib/anything");
    let mut config_path = PathBuf::from("/etc/anything/anythingd.toml");
    let (mut include, mut exclude, mut reconcile) = (Vec::new(), Vec::new(), None);
    let mut args = std::env::args().skip(1);
    // Managing the systemd service: one action alone, nothing else.
    let all: Vec<String> = std::env::args().skip(1).collect();
    if let [action] = all.as_slice()
        && let Some(code) = manage::run(action)
    {
        std::process::exit(code);
    }
    while let Some(flag) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--socket" => socket = value().into(),
            "--database" => database = value().into(),
            "--config" => config_path = value().into(),
            "--include" => include.push(value()),
            "--exclude" => exclude.push(value()),
            "--reconcile-secs" => reconcile = Some(value().parse().unwrap_or_else(|_| usage())),
            _ => usage(),
        }
    }
    let mut config: Config = match std::fs::read_to_string(&config_path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
            log!("anythingd: {}: {err}", config_path.display());
            std::process::exit(1)
        }),
        Err(_) => Config::default(),
    };
    if !include.is_empty() {
        config.include = include;
    }
    if !exclude.is_empty() {
        config.exclude = exclude;
    }
    if let Some(secs) = reconcile {
        config.reconcile_secs = secs;
    }
    Options { socket, database, config }
}

/// What the connections share.
pub struct Shared {
    /// Changed in place by live updates; replaced by full rebuilds.
    index: RwLock<Index>,
    /// Bumped on every change; clients search again when it moves.
    generation: AtomicU64,
    /// Bumped when the index is replaced, which renumbers its entries.
    epoch: AtomicU64,
    /// Bumped when some folder's owner or mode changed.
    permissions: AtomicU64,
    state: Mutex<State>,
    progress: Arc<scan::Progress>,
    masks: server::MaskCache,
}

impl Shared {
    fn replace(&self, mut index: Index) {
        index.prepare();
        let old = std::mem::replace(&mut *self.index.write().unwrap(), index);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.masks.clear();
        // For a moment both indexes were in memory. Without this the
        // allocator keeps the old one's space, hundreds of megabytes.
        drop(old);
        anything_core::give_back_memory();
    }

    fn changed(&self, permissions: bool) {
        if permissions {
            self.permissions.fetch_add(1, Ordering::AcqRel);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// Set by SIGTERM or SIGINT; the index thread saves and exits.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Whether a changed folder matters: inside what the service indexes.
fn covered(scope: &Scope, path: &Path) -> bool {
    let segments: Vec<std::borrow::Cow<str>> = path.iter().skip(1).map(anything_core::name::from_os).collect();
    let segments: Vec<&str> = segments.iter().map(|s| s.as_ref()).collect();
    for end in 1..=segments.len() {
        if scope.is_excluded(&segments[..end]) {
            return false;
        }
    }
    scope.is_included(&segments) || scope.leads_to_include(&segments)
}

/// Full check of every folder, at most this far apart, even with live updates.
const SAFETY_CHECK: Duration = Duration::from_secs(30 * 60);
/// The snapshot is written at most this often (and when stopping).
const SAVE_EVERY: Duration = Duration::from_secs(10 * 60);
/// How often reported changes are looked at.
const TICK: Duration = Duration::from_millis(250);
/// Share of dead entries that triggers a compaction.
const MAX_DEAD: f64 = 0.05;

/// Loads the snapshot or scans, then keeps the index current: live from
/// file system events when allowed, otherwise by checking periodically.
fn keep_index(shared: Arc<Shared>, database: PathBuf, config: Config, socket: PathBuf) {
    let exclude = anything_core::excludes(&config.exclude, config.skip_caches, config.skip_node_modules);
    let scope = Scope::new(&config.include, &exclude);
    let tag = scope_tag(&config.include, &exclude);
    let snapshot = database.join("index.bin");
    let skip = scan::foreign_mounts_here();

    // Watch before loading, so nothing that happens meanwhile is missed.
    let changes = Arc::new(watch::Changes::default());
    let live = match watch::start(changes.clone()) {
        Ok(count) => {
            log!("anythingd: watching {count} file systems live");
            true
        }
        Err(err) => {
            log!("anythingd: no live updates ({err}); checking every {} s", config.reconcile_secs);
            false
        }
    };

    let mut unsaved = false;
    match Index::load(&snapshot, tag) {
        Ok(index) => {
            log!("anythingd: loaded {} entries", index.searchable_len());
            shared.replace(index);
            // The snapshot may be older than the disk.
            changes.overflow.store(true, Ordering::Relaxed);
        }
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                log!("anythingd: not using the snapshot: {err}");
            }
            *shared.state.lock().unwrap() = State::Scanning;
            let (index, stats) = scan::scan(&scope, &skip, &shared.progress);
            log!(
                "anythingd: scanned {} entries in {:.1?} ({} folders unreadable)",
                stats.entries, stats.duration, stats.unreadable
            );
            save(&index, &snapshot, tag);
            shared.replace(index);
        }
    }
    *shared.state.lock().unwrap() = State::Ready;

    let mut last_full = Instant::now();
    let mut last_save = Instant::now();
    let mut next_update = Instant::now();
    let (mut updates, mut folders_read, mut busy) = (0usize, 0usize, Duration::ZERO);
    let mut last_report = Instant::now();
    loop {
        std::thread::sleep(TICK);
        if STOP.load(Ordering::Relaxed) {
            if unsaved {
                save(&shared.index.read().unwrap(), &snapshot, tag);
            }
            std::fs::remove_file(&socket).ok();
            log!("anythingd: stopped");
            std::process::exit(0);
        }
        let full_due = if live {
            changes.overflow.swap(false, Ordering::Relaxed) || last_full.elapsed() >= SAFETY_CHECK
        } else {
            last_full.elapsed() >= Duration::from_secs(config.reconcile_secs.max(1))
        };
        if full_due {
            // Check every folder; rebuild if anything changed. This also
            // clears out entries removed in place.
            last_full = Instant::now();
            changes.take();
            changes.take_files();
            let (rebuilt, stats) = {
                let index = shared.index.read().unwrap();
                scan::reconcile(&index, &scope, &skip, &scan::Progress::default())
            };
            if let Some(rebuilt) = rebuilt {
                busy += stats.duration;
                updates += 1;
                folders_read += stats.folders_changed;
                shared.replace(rebuilt);
                unsaved = true;
            }
        } else if live && Instant::now() >= next_update {
            let mut dirty: Vec<(PathBuf, Option<HashSet<String>>)> =
                changes.take().into_iter().filter(|(p, _)| covered(&scope, p)).collect();
            // Files only written: just them. Named to their folder if that is
            // read anyway.
            let mut written = Vec::new();
            for file in changes.take_files().into_iter().filter(|p| covered(&scope, p)) {
                match file.parent().and_then(|parent| dirty.iter_mut().find(|(d, _)| d == parent)) {
                    Some((_, Some(names))) => {
                        names.extend(file.file_name().map(|n| anything_core::name::from_os(n).into_owned()));
                    }
                    Some((_, None)) => {}
                    None => written.push(file),
                }
            }
            if !dirty.is_empty() || !written.is_empty() {
                // Read the disk with only a read lock; write briefly.
                let patch = {
                    let index = shared.index.read().unwrap();
                    let (mut patch, unknown) = update::plan_files(&index, &written);
                    for file in unknown {
                        let (Some(parent), Some(name)) = (file.parent(), file.file_name()) else { continue };
                        let name = anything_core::name::from_os(name).into_owned();
                        match dirty.iter_mut().find(|(d, _)| d == parent) {
                            Some((_, Some(names))) => {
                                names.insert(name);
                            }
                            Some((_, None)) => {}
                            None => dirty.push((parent.to_path_buf(), Some(HashSet::from([name])))),
                        }
                    }
                    if !dirty.is_empty() {
                        patch.merge(update::plan_named(&index, &scope, &skip, &dirty));
                    }
                    patch
                };
                if !patch.is_empty() {
                    let stats = shared.index.write().unwrap().apply(patch);
                    shared.changed(stats.permissions_changed);
                    let spent = stats.read_time + stats.apply_time;
                    // Numbers only: what makes an update slow.
                    if spent >= Duration::from_millis(200) {
                        log!(
                            "anythingd: slow update: {} folders (largest {} entries), {} files, read {:.0?}, apply {:.0?}",
                            stats.folders, stats.largest_folder, stats.files_checked, stats.read_time, stats.apply_time
                        );
                    }
                    // Collect what follows for a while; and never spend more
                    // than a fifth of the time updating.
                    next_update = Instant::now() + (spent * 5).max(Duration::from_secs(config.batch_secs)).max(TICK);
                    updates += 1;
                    folders_read += stats.folders;
                    busy += spent;
                    unsaved = true;
                }
            }
        }
        // Too many entries removed in place: give the space back.
        if shared.index.read().unwrap().dead_share() > MAX_DEAD {
            let compact = shared.index.read().unwrap().compacted();
            shared.replace(compact);
        }
        if unsaved && last_save.elapsed() >= SAVE_EVERY {
            save(&shared.index.read().unwrap(), &snapshot, tag);
            unsaved = false;
            last_save = Instant::now();
        }
        if last_report.elapsed() >= SAVE_EVERY && updates > 0 {
            log!("anythingd: {updates} updates, {folders_read} folders read, {busy:.1?} spent in the last 10 min");
            (updates, folders_read, busy) = (0, 0, Duration::ZERO);
            last_report = Instant::now();
        }
    }
}

fn save(index: &Index, snapshot: &Path, tag: u64) {
    // The snapshot holds every user's file names: readable by the owner only.
    if let Some(dir) = snapshot.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    if let Err(err) = index.save(snapshot, tag) {
        log!("anythingd: could not save the snapshot: {err}");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(snapshot, std::fs::Permissions::from_mode(0o600)).ok();
    // Writing used large buffers.
    anything_core::give_back_memory();
}

fn main() {
    let Options { socket, database, config } = options();
    // A panic anywhere ends the whole service, so systemd can restart it,
    // instead of leaving it half alive.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::exit(101);
    }));
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let shared = Arc::new(Shared {
        index: RwLock::new(Index::empty()),
        generation: AtomicU64::new(0),
        epoch: AtomicU64::new(0),
        permissions: AtomicU64::new(0),
        state: Mutex::new(State::Loading),
        progress: Arc::new(scan::Progress::default()),
        masks: server::MaskCache::default(),
    });
    // Listen first, so clients can see the progress while loading.
    let listener = server::listen(&socket).unwrap_or_else(|err| {
        log!("anythingd: cannot listen on {}: {err}", socket.display());
        std::process::exit(1)
    });
    log!("anythingd: listening on {}", socket.display());
    {
        let (shared, socket) = (shared.clone(), socket.clone());
        std::thread::Builder::new()
            .name("index".into())
            .spawn(move || keep_index(shared, database, config, socket))
            .expect("start index thread");
    }
    server::serve(listener, shared);
}
