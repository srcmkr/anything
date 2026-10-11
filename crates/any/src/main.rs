//! `any`: search the Anything service from the command line.
//!
//!     any [--socket PATH] [--sort name|path|size|modified] [--desc]
//!         [--limit N] [--count] [--include DIR]... [--exclude PATTERN]... QUERY...

use std::{io::Write as _, os::unix::ffi::OsStrExt as _, path::PathBuf};

use anything_proto::{Client, Request, Response, Sort, State};

fn usage() -> ! {
    eprintln!(
        "usage: any [--socket PATH] [--sort name|path|size|modified] [--desc] [--limit N] [--count] \
         [--include DIR]... [--exclude PATTERN]... QUERY..."
    );
    std::process::exit(2)
}

fn main() {
    let mut socket = PathBuf::from(anything_proto::SOCKET);
    let (mut sort, mut descending, mut limit, mut count_only) = (Sort::Name, false, 100u32, false);
    let (mut include, mut exclude, mut words) = (Vec::new(), Vec::new(), Vec::new());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--socket" => socket = value().into(),
            "--sort" => {
                sort = match value().as_str() {
                    "name" => Sort::Name,
                    "path" => Sort::Path,
                    "size" => Sort::Size,
                    "modified" => Sort::Modified,
                    _ => usage(),
                }
            }
            "--desc" => descending = true,
            "--limit" => limit = value().parse().unwrap_or_else(|_| usage()),
            "--count" => count_only = true,
            "--include" => include.push(value()),
            "--exclude" => exclude.push(value()),
            "--help" | "-h" => usage(),
            // Everything after `--` is the query, even if it starts with `-`.
            "--" => {
                words.extend(args.by_ref());
                break;
            }
            _ => words.push(arg),
        }
    }
    if include.is_empty() {
        include.push("/".into());
    }
    let fail = |err: std::io::Error| -> ! {
        eprintln!("any: {}: {err}", socket.display());
        std::process::exit(1)
    };
    let mut client = Client::connect(&socket).unwrap_or_else(|e| fail(e));
    if let Ok(Response::Status { state, entries, .. }) = client.ask(&Request::Status)
        && state != State::Ready
    {
        eprintln!("any: the service is still {} ({entries} entries so far)", if state == State::Loading { "loading" } else { "scanning" });
    }
    let search = Request::Search { query: words.join(" "), sort, descending, include, exclude };
    let total = match client.ask(&search).unwrap_or_else(|e| fail(e)) {
        Response::Searched { total, .. } => total,
        other => fail(std::io::Error::other(format!("unexpected answer {other:?}"))),
    };
    if count_only {
        println!("{total}");
        return;
    }
    let mut start = 0;
    let wanted = total.min(limit);
    while start < wanted {
        let count = (wanted - start).min(anything_proto::MAX_ROWS);
        match client.ask(&Request::Rows { start, count }).unwrap_or_else(|e| fail(e)) {
            Response::Rows { rows, .. } if !rows.is_empty() => {
                let mut out = std::io::stdout().lock();
                for row in &rows {
                    // The real bytes, also for names that are not UTF-8.
                    let path = anything_core::name::to_path(&row.dir).join(anything_core::name::to_os(&row.name));
                    if out.write_all(path.as_os_str().as_bytes()).and_then(|()| out.write_all(b"\n")).is_err() {
                        std::process::exit(0);
                    }
                }
                start += rows.len() as u32;
            }
            _ => break,
        }
    }
    if total > wanted {
        eprintln!("any: {} more (use --limit)", total - wanted);
    }
}
