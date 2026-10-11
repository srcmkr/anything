//! cargo run --release -p anything-core --example bench [entries]

use anything_core::{Query, Scope, SortKey, synthetic_index};
use std::time::Instant;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5_000_000);
    let t = Instant::now();
    let index = synthetic_index(n, 42);
    println!("build {n} entries: {:?}, heap {} MB", t.elapsed(), index.heap_bytes() >> 20);

    for q in ["", "r", "re", "rech", "rechnung", "*.pdf", "rechnung*.pdf", "rechnung *.pdf", "日本", "zzzz"] {
        for key in [SortKey::Name, SortKey::Path, SortKey::Size] {
            let query = Query::parse(q);
            let t = Instant::now();
            let hits = index.search(&query, key, false);
            println!("{q:>12} {key:?}: {:>8} hits in {:?}", hits.len(), t.elapsed());
        }
    }

    for (include, exclude) in [
        (vec!["/home"], vec![]),
        (vec!["/"], vec!["/home/*"]),
        (vec!["/"], vec!["node_modules", ".cache"]),
    ] {
        let t = Instant::now();
        let restricted = index.restrict(&Scope::new(&include, &exclude));
        println!(
            "restrict {include:?} minus {exclude:?}: {} entries in {:?}",
            restricted.len(),
            t.elapsed()
        );
    }
}
