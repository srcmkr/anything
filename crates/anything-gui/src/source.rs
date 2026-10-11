//! Where the index comes from: the fake demo data, or this machine's file
//! system, scanned and kept as a snapshot in the database folder.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anything_core::Index;
use anything_proto::Client;
use gpui::{App, Global};

pub enum Source {
    /// The fake index (`ANYTHING_DEMO=1`), cut to the scan settings.
    Demo(Arc<Index>),
    /// Scanned from disk.
    Disk,
    /// Asked from the system service at this socket.
    Service { socket: PathBuf, client: Arc<Mutex<Option<Client>>> },
}

/// The service's socket: `ANYTHING_SOCKET`, or the system default.
pub fn service_socket() -> PathBuf {
    std::env::var_os("ANYTHING_SOCKET").map_or_else(|| PathBuf::from(anything_proto::SOCKET), PathBuf::from)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    /// Cutting the demo index to new scan settings.
    Applying,
    Loading,
    Scanning,
    /// The service does not answer.
    Unreachable,
}

/// Whether the results come from the service right now.
pub struct UsingService(pub bool);

impl Global for UsingService {}

pub fn set_using_service(using: bool, cx: &mut App) {
    if cx.try_global::<UsingService>().map(|u| u.0) != Some(using) {
        cx.set_global(UsingService(using));
    }
}

pub fn using_service(cx: &App) -> bool {
    cx.try_global::<UsingService>().is_some_and(|u| u.0)
}

/// How many entries the index the results come from holds, for the About page.
pub struct IndexSize(pub usize);

impl Global for IndexSize {}

pub fn set_index_size(entries: usize, cx: &mut App) {
    if cx.try_global::<IndexSize>().map(|s| s.0) != Some(entries) {
        cx.set_global(IndexSize(entries));
    }
}

pub fn index_size(cx: &App) -> Option<usize> {
    cx.try_global::<IndexSize>().map(|s| s.0).filter(|&n| n > 0)
}

/// Bumped by "Rescan now" in the settings.
pub struct RescanRequest(pub u64);

impl Global for RescanRequest {}

pub fn request_rescan(cx: &mut App) {
    let next = cx.try_global::<RescanRequest>().map_or(0, |r| r.0) + 1;
    cx.set_global(RescanRequest(next));
}

/// Identifies the scan settings a snapshot was made with.
pub fn scope_tag(include: &[String], exclude: &[String]) -> u64 {
    anything_core::scope_tag(include, exclude)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tags_tell_settings_apart() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let a = super::scope_tag(&s(&["/"]), &s(&["/home/x"]));
        assert_eq!(a, super::scope_tag(&s(&["/"]), &s(&["/home/x"])));
        assert_ne!(a, super::scope_tag(&s(&["/", "/home/x"]), &s(&[])));
        assert_ne!(a, super::scope_tag(&s(&["/"]), &s(&["/home/y"])));
    }
}
