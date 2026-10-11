//! Only one Anything runs per user and configuration. A second start asks
//! the running one to show its window and exits.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    time::Instant,
};

use crate::tray::TrayEvent;

/// One socket per configuration folder, so a test instance with its own
/// `XDG_CONFIG_HOME` does not hand over to the user's instance.
fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_default();
    // FNV-1a, enough to tell configurations apart.
    let hash = config.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    runtime.join(format!("anything-{hash:016x}.sock"))
}

/// Returns `false` when another instance is running; it has been asked to
/// show itself. Otherwise starts listening for later starts.
pub fn claim(events: async_channel::Sender<TrayEvent>) -> bool {
    let path = socket_path();
    if let Ok(mut stream) = UnixStream::connect(&path) {
        stream.write_all(b"show\n").ok();
        return false;
    }
    // Nobody answers: a leftover from a crash, or nothing at all.
    std::fs::remove_file(&path).ok();
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("anything: cannot listen on {}: {err}", path.display());
            return true;
        }
    };
    std::thread::Builder::new()
        .name("anything-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line).ok();
                if line.trim() == "show" {
                    events.send_blocking(TrayEvent::Show(Instant::now())).ok();
                }
            }
        })
        .ok();
    true
}

/// Removes the socket on a clean exit.
pub fn release() {
    std::fs::remove_file(socket_path()).ok();
}
