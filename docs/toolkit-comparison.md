# GUI toolkit comparison (2026-10-11)

Before writing the front end we built the same throwaway window twice, once
with GTK4 + libadwaita and once with GPUI + gpui-component, and drove both
through an identical scripted run over 5 million fake entries: search field,
four-column table, sorting by column.

Machine: Linux Mint 22.3 (X11, Cinnamon), RTX 4090, 120 Hz display.
Versions: GTK 4.14.5 / libadwaita 1.5 (gtk4-rs 0.11, libadwaita-rs 0.9);
gpui-pre 0.3.8 / gpui-component 0.7.1.

Latencies run from the change to the end of the frame that shows it.

| | GPUI | GTK4, plain list model | GTK4, windowed model |
|---|---|---|---|
| Typing (median / max) | **9 / 20 ms** | 150 / 670 ms | 45 / 65 ms |
| Clearing back to 5 M hits | **9 / 13 ms** | up to 1,200 ms | 44 / 48 ms |
| Re-sorting 5 M hits | **6 / 14 ms** | 1,230 ms | 40 ms |
| Fast scrolling | **120 fps, no late frames** | ~95 fps, 90 of 600 late | 27 fps |
| Dragging the scroll bar | **120 fps** | 11 fps | 25 fps |
| First frame after start | 370 ms | 360 ms | 260 ms |
| Memory for the UI | ~310 MB | ~230 MB | ~215 MB |

Findings:

- GTK's list widgets scale with the number of items. Removing millions of
  items costs over a second, a scroll bar jump 85 ms. Switching the selection
  model, swapping the model instead of emitting `items-changed`, and the
  Vulkan vs. GL renderer made no difference.
- Exposing only one screen of rows to GTK ("windowed") fixes typing but makes
  every scroll step rebind all visible rows, loses pixel scrolling and needs
  hand-written keyboard navigation.
- GPUI held 120 fps throughout with its uniform list.

Decision: GPUI. Its API is still moving, so versions are pinned exactly.

Pitfall: `gpui-pre-platform` enables no windowing backend by default; without
`features = ["x11", "wayland"]` the window silently never appears.
