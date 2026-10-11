//! Measures in-place updates on this machine's index; prints numbers only.
//! cargo run --release -p anything-core --example update -- SCRATCH_DIR --include / --exclude X

use anything_core::{Scope, scan, update};
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let scratch = std::path::PathBuf::from(args.next().expect("scratch dir"));
    let (mut include, mut exclude) = (Vec::new(), Vec::new());
    while let Some(flag) = args.next() {
        let value = args.next().expect("value");
        if flag == "--include" { include.push(value) } else { exclude.push(value) }
    }
    let scope = Scope::new(&include, &exclude);
    let skip = scan::foreign_mounts_here();
    let (mut index, stats) = scan::scan(&scope, &skip, &scan::Progress::default());
    println!("scan: {} entries in {:.2?}", stats.entries, stats.duration);
    let t = Instant::now();
    index.prepare();
    println!("prepare: {:.2?}", t.elapsed());

    std::fs::create_dir_all(&scratch).unwrap();
    for round in 0..5 {
        let file = scratch.join(format!("measure-{round}.txt"));
        for (what, change) in [("create", 0), ("grow", 1), ("delete", 2)] {
            match change {
                0 => std::fs::write(&file, b"x").unwrap(),
                1 => std::fs::write(&file, vec![b'x'; 4096]).unwrap(),
                _ => std::fs::remove_file(&file).unwrap(),
            }
            let t = Instant::now();
            let patch = update::plan(&index, &scope, &skip, std::slice::from_ref(&scratch));
            let planned = t.elapsed();
            let s = index.apply(patch);
            println!(
                "round {round} {what:>6}: plan {:>8.2?}, apply {:>8.2?} (+{} -{} ~{})",
                planned, s.apply_time, s.added, s.removed, s.changed
            );
        }
    }
}
