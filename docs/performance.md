# Performance

How fast Anything is, measured, and how to measure it yourself. The numbers
are a snapshot from one machine on one day, not a promise.

Machine: Intel Core Ultra 9 285K (24 cores), 128 GB RAM, WD_BLACK SN850X NVMe
SSD with ext4 on LUKS, 120 Hz display, Linux Mint 22.3 (X11, Cinnamon).
Measured on 2026-10-11 with Anything 0.1.0.

## Search engine: 5 million generated entries

```sh
cargo run --release -p anything-core --example bench
```

| Query | Matches | Time |
|---|---:|---:|
| *(empty: everything)* | 4,999,999 | 2.1 ms |
| `r` | 2,318,476 | 10.7 ms |
| `rechnung` | 191,300 | 7.7 ms |
| `*.pdf` | 430,153 | 7.2 ms |
| `rechnung*.pdf` | 15,744 | 6.8 ms |
| `rechnung *.pdf` | 52,078 | 7.2 ms |
| `日本` | 17,680 | 6.6 ms |
| `zzzz` (nothing) | 0 | 7.1 ms |

Sorting by path or size instead of name costs the same: sort orders are kept
ready in the index. Building an index of 5 million entries takes 2.1 s and
260 MB.

## User interface: 5 million generated entries

```sh
ANYTHING_BENCH=1 cargo run --release -p anything-gui
```

Every time runs from the action to the end of the first frame that shows its
result.

| Action | Median | Worst |
|---|---:|---:|
| Typing a character | 18.5 ms | 22.4 ms |
| Deleting a character | 18.4 ms | 19.7 ms |
| Re-sorting all 5 million | 20.7 ms | 21.0 ms |
| Showing the hidden window | 9.9 ms | 15.3 ms |
| Scrolling, 600 frames | 8.3 ms per frame (120 fps) | 0 dropped frames |
| Dragging the scroll bar, 240 frames | 8.3 ms per frame (120 fps) | 0 dropped frames |

The frame rate is the display's.

## A real system: the service on the author's desktop

| | |
|---|---:|
| Entries on all local disks | about 3.3 million (5.5 million with caches and `node_modules`) |
| Full scan at service start, 5.5 million entries | 3.2 s |
| Search from the command line (`any`), end to end | about 10 ms |
| Memory of the service | about 0.45 GB, up to 0.6 GB at times |
| CPU while programs keep writing in the home folder | about 1 % of one core |
| Memory of the window while it uses the service | about 330 MB |

Try it without indexing anything: `ANYTHING_DEMO=1 anything --show` searches 5
million made-up entries.
