//! Scans this machine and prints numbers only, never names.
//! cargo run --release -p anything-core --example scan -- SNAPSHOT --include / --exclude /home/x

use anything_core::{Index, Query, Scope, SortKey, scan};
use std::time::Instant;

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
        >> 10
}

fn main() {
    let mut args = std::env::args().skip(1);
    let snapshot = std::path::PathBuf::from(args.next().expect("snapshot path"));
    let (mut include, mut exclude) = (Vec::new(), Vec::new());
    while let Some(flag) = args.next() {
        let value = args.next().expect("value");
        match flag.as_str() {
            "--include" => include.push(value),
            "--exclude" => exclude.push(value),
            _ => panic!("unknown {flag}"),
        }
    }
    let scope = Scope::new(&include, &exclude);
    let skip = scan::foreign_mounts_here();
    println!("skipped mount points: {}", skip.len());

    let (index, stats) = scan::scan(&scope, &skip, &scan::Progress::default());
    println!(
        "scan: {} entries, {} folders, {} unreadable, {:.2?}, RSS {} MB, index heap {} MB",
        stats.entries, stats.folders, stats.unreadable, stats.duration, rss_mb(), index.heap_bytes() >> 20
    );

    let viewer = anything_core::access::Viewer {
        uid: unsafe { libc_getuid() },
        groups: std::process::Command::new("id").arg("-G").output().map(|o| {
            String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|g| g.parse().ok()).collect()
        }).unwrap_or_default(),
    };
    let t = Instant::now();
    let bits = anything_core::access::mask(&index, Some(&viewer), Some(&scope));
    let seen: u32 = bits.iter().map(|w| w.count_ones()).sum();
    println!("visibility for uid {}: {} of {} entries in {:.2?}", viewer.uid, seen, index.searchable_len(), t.elapsed());

    for round in 1..=2 {
        let (updated, stats) = scan::reconcile(&index, &scope, &skip, &scan::Progress::default());
        println!(
            "reconcile {round}: {} folders checked, {} changed, {:.2?}, rebuilt: {}",
            stats.folders_checked, stats.folders_changed, stats.duration, updated.is_some()
        );
    }

    let t = Instant::now();
    index.save(&snapshot, 1).unwrap();
    let size = std::fs::metadata(&snapshot).unwrap().len();
    println!("save: {:.2?}, {} MB", t.elapsed(), size >> 20);
    drop(index);

    let t = Instant::now();
    let index = Index::load(&snapshot, 1).unwrap();
    println!("load: {:.2?}", t.elapsed());

    let all = index.search(&Query::parse(""), SortKey::Name, false);
    let (mut bytes, mut changed, mut changed_bytes) = (0usize, 0usize, 0usize);
    for &id in &all {
        let name = index.name(id);
        bytes += name.len();
        if name.to_lowercase() != name {
            changed += 1;
            changed_bytes += name.len();
        }
    }
    let mut unique = std::collections::HashSet::new();
    let mut unique_bytes = 0usize;
    for &id in &all {
        if unique.insert(index.name(id)) {
            unique_bytes += index.name(id).len();
        }
    }
    println!("unique names: {} of {} ({} MB of {} MB)", unique.len(), all.len(), unique_bytes >> 20, bytes >> 20);
    println!(
        "names: avg {:.1} bytes, {:.1}% change when lowercased ({} MB of {} MB)",
        bytes as f64 / all.len() as f64,
        100.0 * changed as f64 / all.len() as f64,
        changed_bytes >> 20,
        bytes >> 20
    );

    for q in ["", "*.pdf", "rechnung", "*.jpg", "zzzzqqq"] {
        let query = Query::parse(q);
        let t = Instant::now();
        let hits = index.search(&query, SortKey::Name, false);
        println!("search {q:>9?}: {:>9} hits in {:.2?}", hits.len(), t.elapsed());
    }
}
