//! The search window: input on top, results below, status line at the bottom.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use std::{collections::HashSet, path::PathBuf, sync::{Mutex, atomic::Ordering}};

use anything_core::{Index, Query, Scope, SortKey, scan};
use anything_proto::{Client, Request, Response, Sort};
use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, Styled as _, Subscription, Task, Window, canvas, div,
    point, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _,
    notification::Notification,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    table::{DataTable, TableEvent, TableState},
    v_flex,
};

use gpui_base::actions::SelectUp;
use rust_i18n::t;

use crate::{
    actions::{
        self, CopyFile, CopyFolder, CopyName, CopyPath, DeleteItem, Do, OpenItem, OpenWith, RevealItem,
        ShowProperties, TrashItem,
    },
    state::{Geometry, UiState},
    bench::{Action, Bench},
    grid::{self, GridState},
    results::{BLOCK, Hits, ResultTable, RowInfo},
    thumbs::Thumbnails,
    settings::{self, AppSettings},
    source::{RescanRequest, Source, Status, scope_tag},
    x11::X11Window,
};

/// How often the index is checked against the disk.
const RECONCILE_EVERY: Duration = Duration::from_secs(120);
/// How often Anything looks for the service while it runs without.
const SERVICE_WATCH: Duration = Duration::from_secs(3);
/// A service that does not answer for this long is given up on.
const GIVE_UP_ON_SERVICE: Duration = Duration::from_secs(30);

fn sort_key_name(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Path => "path",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
    }
}

fn sort_from_key(name: &str) -> SortKey {
    match name {
        "path" => SortKey::Path,
        "size" => SortKey::Size,
        "modified" => SortKey::Modified,
        _ => SortKey::Name,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    List,
    Grid,
}

pub struct SearchView {
    mode: ViewMode,
    pub(crate) grid: GridState,
    pub(crate) thumbs: Entity<Thumbnails>,
    source: Source,
    /// Scan include and exclude lists and database folder in effect.
    applied: Option<(Vec<String>, Vec<String>, std::path::PathBuf)>,
    index_task: Task<()>,
    /// Redraws while loading or scanning, for the progress in the status.
    ticker: Task<()>,
    status: Status,
    progress: Option<Arc<scan::Progress>>,
    /// Index set but not yet searched; see [`Self::set_index`].
    pending_index: Option<Arc<Index>>,
    /// Keeps the snapshot current (disk only).
    reconcile_task: Task<()>,
    reconcile_timer: Task<()>,
    /// Service: blocks of rows being fetched, the service's index
    /// generation and entry count, and the status poll.
    fetching: HashSet<usize>,
    service_generation: Option<u64>,
    service_entries: u64,
    service_poll: Task<()>,
    /// Without the service: looks for it now and then, to switch over.
    service_watch: Task<()>,
    unreachable_since: Option<Instant>,
    scrolled_once: bool,
    /// The index changed while the window was hidden.
    outdated: bool,
    input: Entity<InputState>,
    table: Entity<TableState<ResultTable>>,
    generation: u64,
    search_task: Task<()>,
    x11: Option<X11Window>,
    /// Whether window and view state are saved (not during benchmarks).
    persist: bool,
    /// Entries removed from the results (trash, delete) that the index of
    /// this process still has, until it is replaced.
    removed: Option<(Arc<Index>, HashSet<u32>)>,
    visible: bool,
    /// Set when the window is asked to appear; cleared by the next paint.
    shown_at: Rc<Cell<Option<Instant>>>,
    bench: Option<Rc<RefCell<Bench>>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchView {
    pub fn new(
        source: Source,
        bench: Option<Rc<RefCell<Bench>>>,
        visible: bool,
        ui: UiState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("search.placeholder")));
        let mut results =
            ResultTable::new(sort_from_key(&ui.sort), ui.descending, ui.column_widths.clone(), cx.entity().downgrade());
        match &source {
            Source::Demo(full) => {
                let ids = full.search(&Query::parse(""), results.sort, results.descending);
                results.hits = Hits::Local { index: full.clone(), ids };
            }
            Source::Disk => results.hits = Hits::Local { index: Arc::new(Index::empty()), ids: Vec::new() },
            Source::Service { .. } => {}
        }
        let table = cx.new(|cx| {
            TableState::new(results, window, cx).col_movable(false).loop_selection(false)
        });

        let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
            match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.search(text, cx);
                }
                // Enter in the search field opens the first result, Ctrl+Enter
                // shows it in its folder.
                InputEvent::PressEnter { secondary, .. } => {
                    let what = if *secondary { Do::Reveal } else { Do::Open };
                    this.run_on(what, Some(0), window, cx);
                }
                _ => {}
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));

        if bench.is_some() {
            cx.on_next_frame(window, Self::bench_tick);
        }
        let double_click = cx.subscribe_in(&table, window, |this, table, event, window, cx| match event {
            TableEvent::DoubleClickedRow(row) => this.run_on(Do::Open, Some(*row), window, cx),
            // The status bar shows the selected path.
            TableEvent::SelectRow(_) => cx.notify(),
            TableEvent::ColumnWidthsChanged(widths) => {
                table.update(cx, |table, _| {
                    table.delegate_mut().widths = widths.iter().map(|w| f32::from(*w)).collect();
                });
                this.save_state(window, cx);
            }
            _ => {}
        });
        let settings_changed = cx.observe_global_in::<AppSettings>(window, |this, window, cx| {
            this.apply_scope(false, cx);
            // Texts may be in a new language.
            this.table.update(cx, |table, cx| table.refresh(cx));
            this.input.update(cx, |input, cx| input.set_placeholder(t!("search.placeholder"), window, cx));
        });
        let rescan = cx.observe_global::<RescanRequest>(|this, cx| this.apply_scope(true, cx));
        let updates = cx.observe_global::<crate::update::Updates>(|_, cx| cx.notify());
        let thumbs = cx.new(Thumbnails::new);
        let thumbs_ready = cx.observe(&thumbs, |_, _, cx| cx.notify());
        let view = std::env::var("ANYTHING_VIEW").unwrap_or(ui.view.clone());
        let mode = if view == "grid" { ViewMode::Grid } else { ViewMode::List };
        let grid_zoom = ui.grid_zoom.unwrap_or(grid::DEFAULT_ZOOM).min(grid::ZOOM_LEVELS.len() - 1);
        let x11 = X11Window::new(window);
        if let (Some(x11), Some(geometry)) = (&x11, ui.window) {
            x11.set_position(geometry.x, geometry.y);
            if visible {
                x11.apply_position();
            }
        }
        let mut this = Self {
            mode,
            grid: GridState::new(cx.focus_handle(), grid_zoom),
            thumbs,
            source,
            applied: None,
            index_task: Task::ready(()),
            ticker: Task::ready(()),
            status: Status::Ready,
            progress: None,
            pending_index: None,
            reconcile_task: Task::ready(()),
            reconcile_timer: Task::ready(()),
            fetching: HashSet::new(),
            service_generation: None,
            service_entries: 0,
            service_poll: Task::ready(()),
            service_watch: Task::ready(()),
            unreachable_since: None,
            scrolled_once: false,
            outdated: false,
            input,
            table,
            generation: 0,
            search_task: Task::ready(()),
            x11,
            persist: bench.is_none(),
            removed: None,
            visible,
            shown_at: Rc::new(Cell::new(None)),
            bench,
            _subscriptions: vec![subscription, double_click, settings_changed, thumbs_ready, rescan, updates],
        };
        this.apply_scope(false, cx);
        // Development aid for screenshots: ANYTHING_QUERY=<text> starts with
        // this search.
        if let Ok(query) = std::env::var("ANYTHING_QUERY") {
            this.input.update(cx, |input, cx| input.set_value(query, window, cx));
            this.search_again(false, cx);
        }
        crate::source::set_using_service(matches!(this.source, Source::Service { .. }), cx);
        if matches!(this.source, Source::Service { .. }) {
            this.start_service_poll(cx);
        }
        if matches!(this.source, Source::Disk) {
            this.start_reconcile_timer(cx);
            this.start_service_watch(cx);
        }
        this
    }

    fn start_reconcile_timer(&mut self, cx: &mut Context<Self>) {
        self.reconcile_timer = cx.spawn(async move |this, cx| {
            loop {
                // ANYTHING_RECONCILE_SECS shortens the wait for testing.
                let every = std::env::var("ANYTHING_RECONCILE_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .map_or(RECONCILE_EVERY, Duration::from_secs);
                cx.background_executor().timer(every).await;
                if this.update(cx, |this, cx| this.reconcile(cx)).is_err() {
                    break;
                }
            }
        });
    }

    /// Switches to the service once it runs and has its index ready, e.g.
    /// after it was started from the settings.
    fn start_service_watch(&mut self, cx: &mut Context<Self>) {
        self.service_watch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SERVICE_WATCH).await;
                let socket = crate::source::service_socket();
                let ready = cx
                    .background_spawn(async move {
                        let mut client = Client::connect(&socket).ok()?;
                        match client.ask(&Request::Status) {
                            Ok(Response::Status { state: anything_proto::State::Ready, .. }) => Some((socket, client)),
                            _ => None,
                        }
                    })
                    .await;
                if let Some((socket, client)) = ready {
                    this.update(cx, |this, cx| this.use_service(socket, client, cx)).ok();
                    break;
                }
            }
        });
    }

    fn use_service(&mut self, socket: PathBuf, client: Client, cx: &mut Context<Self>) {
        eprintln!("anything: using the service at {}", socket.display());
        // The own index is no longer needed.
        if let Some(running) = self.progress.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.index_task = Task::ready(());
        self.reconcile_task = Task::ready(());
        self.reconcile_timer = Task::ready(());
        self.service_watch = Task::ready(());
        self.pending_index = None;
        self.source = Source::Service { socket, client: Arc::new(Mutex::new(Some(client))) };
        self.service_generation = None;
        self.status = Status::Ready;
        crate::source::set_using_service(true, cx);
        self.start_service_poll(cx);
        self.search_again(false, cx);
        // Once the old results are gone, return their memory.
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            anything_core::give_back_memory();
        })
        .detach();
    }

    /// The service was stopped: index this user's files again, as without it.
    fn use_disk(&mut self, cx: &mut Context<Self>) {
        eprintln!("anything: the service stopped; scanning here");
        self.service_poll = Task::ready(());
        self.source = Source::Disk;
        self.fetching.clear();
        self.service_generation = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().hits = Hits::Local { index: Arc::new(Index::empty()), ids: Vec::new() };
            cx.notify();
        });
        crate::source::set_using_service(false, cx);
        self.applied = None;
        self.apply_scope(false, cx);
        self.start_reconcile_timer(cx);
        self.start_service_watch(cx);
    }

    /// Reads the folders that changed since the index was made and, if any
    /// did, switches to the updated index and saves it.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        if self.status != Status::Ready || !matches!(self.source, Source::Disk) {
            return;
        }
        let Some((include, exclude, database_dir)) = self.applied.clone() else { return };
        let Some(index) = self.latest_index(cx) else { return };
        // A rescan cancels this through the shared progress.
        let progress = Arc::new(scan::Progress::default());
        self.progress = Some(progress.clone());
        self.reconcile_task = cx.spawn(async move |this, cx| {
            let updated = cx
                .background_spawn(async move {
                    let scope = Scope::new(&include, &exclude);
                    let skip = scan::foreign_mounts_here();
                    let (updated, stats) = scan::reconcile(&index, &scope, &skip, &progress);
                    if let Some(updated) = &updated {
                        eprintln!(
                            "anything: {} of {} folders changed, updated in {:.1?}",
                            stats.folders_changed, stats.folders_checked, stats.duration
                        );
                        let snapshot = database_dir.join("index.bin");
                        if let Err(err) = updated.save(&snapshot, scope_tag(&include, &exclude)) {
                            eprintln!("anything: could not save {}: {err}", snapshot.display());
                        }
                    }
                    updated
                })
                .await;
            if let Some(updated) = updated {
                this.update(cx, |this, cx| this.set_index(Arc::new(updated), true, cx)).ok();
            }
        });
    }

    /// Brings the index in line with the scan settings: loads the snapshot
    /// or scans the disk, or cuts the demo index. `rescan` scans even when a
    /// snapshot exists.
    fn apply_scope(&mut self, rescan: bool, cx: &mut Context<Self>) {
        let settings = settings::get(cx);
        let wanted = (settings.scan_include.clone(), settings.all_excludes(), settings.database_dir.clone());
        if !rescan && self.applied.as_ref() == Some(&wanted) {
            return;
        }
        let first = self.applied.is_none();
        self.applied = Some(wanted.clone());
        let (include, exclude, database_dir) = wanted;
        let scope = Scope::new(&include, &exclude);

        let full = match &self.source {
            Source::Demo(full) => full.clone(),
            Source::Disk => {
                self.load_or_scan(scope, scope_tag(&include, &exclude), database_dir.join("index.bin"), rescan, cx);
                return;
            }
            // The service indexes on its own; the scan settings filter.
            Source::Service { .. } => {
                self.search_again(false, cx);
                return;
            }
        };
        if scope.is_everything() {
            self.status = Status::Ready;
            self.index_task = Task::ready(());
            if !first {
                self.set_index(full, false, cx);
            }
            return;
        }
        self.status = Status::Applying;
        cx.notify();
        self.index_task = cx.spawn(async move |this, cx| {
            let index = cx.background_spawn(async move { Arc::new(full.restrict(&scope)) }).await;
            this.update(cx, |this, cx| {
                this.status = Status::Ready;
                this.set_index(index, false, cx);
            })
            .ok();
        });
    }

    fn load_or_scan(&mut self, scope: Scope, tag: u64, snapshot: PathBuf, rescan: bool, cx: &mut Context<Self>) {
        if let Some(running) = self.progress.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        let progress = Arc::new(scan::Progress::default());
        self.progress = Some(progress.clone());
        self.status = if rescan { Status::Scanning } else { Status::Loading };
        self.start_ticker(cx);
        self.index_task = cx.spawn(async move |this, cx| {
            if !rescan {
                let path = snapshot.clone();
                let loaded = cx.background_spawn(async move { Index::load(&path, tag) }).await;
                match loaded {
                    Ok(index) => {
                        this.update(cx, |this, cx| {
                            this.status = Status::Ready;
                            this.set_index(Arc::new(index), false, cx);
                            // The snapshot may be older than what is on disk.
                            this.reconcile(cx);
                        })
                        .ok();
                        return;
                    }
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                        eprintln!("anything: not using {}: {err}", snapshot.display());
                    }
                    Err(_) => {}
                }
                this.update(cx, |this, cx| {
                    this.status = Status::Scanning;
                    cx.notify();
                })
                .ok();
            }
            let (index, stats) = cx
                .background_spawn(async move {
                    let skip = scan::foreign_mounts_here();
                    let (index, stats) = scan::scan(&scope, &skip, &progress);
                    if !stats.cancelled
                        && let Err(err) = index.save(&snapshot, tag)
                    {
                        eprintln!("anything: could not save {}: {err}", snapshot.display());
                    }
                    (index, stats)
                })
                .await;
            if stats.cancelled {
                return;
            }
            eprintln!(
                "anything: scanned {} entries in {:.1?} ({} folders unreadable)",
                stats.entries, stats.duration, stats.unreadable
            );
            this.update(cx, |this, cx| {
                this.status = Status::Ready;
                this.set_index(Arc::new(index), false, cx);
            })
            .ok();
        });
    }

    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        self.ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                let busy = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.status != Status::Ready
                    })
                    .unwrap_or(false);
                if !busy {
                    break;
                }
            }
        });
    }

    /// Switches to a new index. The current results stay on screen until the
    /// search on the new index is done, since hits are only valid for the
    /// index they came from. `keep_position` leaves selection and scrolling
    /// alone, for updates in the background.
    fn set_index(&mut self, index: Arc<Index>, keep_position: bool, cx: &mut Context<Self>) {
        crate::source::set_index_size(index.searchable_len(), cx);
        self.pending_index = Some(index);
        let text = self.input.read(cx).value().to_string();
        self.run_search(text, keep_position, cx);
    }

    /// The newest local index, even if no search has used it yet.
    fn latest_index(&self, cx: &App) -> Option<Arc<Index>> {
        self.pending_index.clone().or_else(|| self.results(cx).hits.index().cloned())
    }

    fn search(&mut self, text: String, cx: &mut Context<Self>) {
        self.run_search(text, false, cx);
    }

    /// Repeats the current search, e.g. with another sort order or after the
    /// index changed.
    pub(crate) fn search_again(&mut self, keep_position: bool, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        self.run_search(text, keep_position, cx);
    }

    /// Runs the search off the UI thread, locally or in the service; a newer
    /// search makes older results obsolete.
    fn run_search(&mut self, text: String, keep_position: bool, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let (sort, descending) = {
            let results = self.table.read(cx).delegate();
            (results.sort, results.descending)
        };
        let settings = settings::get(cx);
        let (include, exclude) = (settings.scan_include.clone(), settings.all_excludes());
        let local = self.latest_index(cx);
        // Entries removed here that the index does not know about yet.
        let hidden = match (&local, &self.removed) {
            (Some(index), Some((known, ids))) if Arc::ptr_eq(index, known) => ids.clone(),
            _ => HashSet::new(),
        };
        let service = match &self.source {
            Source::Service { client, .. } => Some(client.clone()),
            _ => None,
        };
        self.search_task = cx.spawn(async move |this, cx| {
            let (query, hits, elapsed, text, searched) = cx
                .background_spawn(async move {
                    let start = Instant::now();
                    let query = Query::parse(&text);
                    let (hits, searched) = match (service, local) {
                        (Some(client), _) => (remote_search(&client, &text, sort, descending, include, exclude), None),
                        (None, Some(index)) => {
                            let mut ids = index.search(&query, sort, descending);
                            if !hidden.is_empty() {
                                ids.retain(|id| !hidden.contains(id));
                            }
                            (Some(Hits::Local { index: index.clone(), ids }), Some(index))
                        }
                        (None, None) => (None, None),
                    };
                    (query, hits, start.elapsed(), text, searched)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let (Some(pending), Some(searched)) = (&this.pending_index, &searched)
                    && Arc::ptr_eq(pending, searched)
                {
                    this.pending_index = None;
                }
                let Some(hits) = hits else {
                    // The service did not answer; the poll reconnects.
                    this.status = Status::Unreachable;
                    cx.notify();
                    return;
                };
                if this.status == Status::Unreachable {
                    this.status = Status::Ready;
                }
                this.fetching.clear();
                let count = hits.len();
                this.table.update(cx, |table, cx| {
                    let results = table.delegate_mut();
                    results.hits = hits;
                    results.query = Rc::new(query);
                    results.query_text = text;
                    results.search_time = elapsed;
                    if !keep_position || table.selected_row().is_some_and(|row| row >= count) {
                        table.clear_selection(cx);
                    }
                    if !keep_position {
                        table.scroll_to_row(0, cx);
                    }
                    cx.notify();
                });
                if !keep_position {
                    this.grid.reset();
                } else if this.grid.selected.is_some_and(|i| i >= count) {
                    this.grid.selected = None;
                }
                // Development aid: jump to a row once, e.g. to test fetching.
                if let Some(row) = std::env::var("ANYTHING_SCROLL_TO").ok().and_then(|r| r.parse::<usize>().ok())
                    && count > row && !this.scrolled_once
                {
                    this.scrolled_once = true;
                    this.table.update(cx, |table, cx| table.scroll_to_row(row, cx));
                    this.grid.select(row);
                }
                if let Some(bench) = &this.bench {
                    bench.borrow_mut().result_applied();
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// The result an action applies to: the right-clicked or selected one.
    fn target(&self, cx: &App) -> Option<usize> {
        match self.mode {
            ViewMode::Grid => self.grid.selected,
            ViewMode::List => {
                let table = self.table.read(cx);
                table.right_clicked_row().or(table.selected_row())
            }
        }
    }

    pub(crate) fn run_on(&mut self, what: Do, position: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = position.and_then(|p| self.results(cx).hits.path(p)) else { return };
        let window_id = self.x11.as_ref().map(|x11| x11.id());
        actions::run(what, path, window_id, window, cx);
    }

    /// Moves the result at `position` to the trash, or (`permanent`) deletes
    /// it after asking.
    fn remove(&mut self, position: Option<usize>, permanent: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = position else { return };
        let Some(path) = self.results(cx).hits.path(position) else { return };
        if !crate::demo::may_open(&path) {
            window.push_notification(Notification::info(path.display().to_string()).title(t!("note.demo_only")), cx);
            return;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (name, _) = crate::format::shown(name, Vec::new());
        let view = cx.entity().downgrade();
        if !permanent {
            let task = cx.background_spawn({
                let path = path.clone();
                async move { crate::desktop::trash(&path) }
            });
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                this.update_in(cx, |this, window, cx| match result {
                    Ok(()) => {
                        this.forget(position, &path, cx);
                        let undo = path.clone();
                        let view = view.clone();
                        window.push_notification(
                            Notification::success(t!("note.trashed", name = name)).action(move |_, _, _| {
                                let (undo, view) = (undo.clone(), view.clone());
                                Button::new("undo-trash").label(t!("button.undo")).on_click(move |_, window, cx| {
                                    let (undo, view) = (undo.clone(), view.clone());
                                    let task = cx.background_spawn(async move { crate::desktop::untrash(&undo) });
                                    window
                                        .spawn(cx, async move |cx| {
                                            let result = task.await;
                                            cx.update(|window, cx| match result {
                                                Ok(()) => {
                                                    view.update(cx, |view, cx| view.restored(cx)).ok();
                                                }
                                                Err(error) => window.push_notification(
                                                    Notification::error(t!("note.failed", error = error)),
                                                    cx,
                                                ),
                                            })
                                            .ok();
                                        })
                                        .detach();
                                })
                            }),
                            cx,
                        );
                    }
                    Err(error) => window.push_notification(Notification::error(t!("note.failed", error = error)), cx),
                })
                .ok();
            })
            .detach();
            return;
        }
        let is_dir = path.is_dir();
        let question = if is_dir { t!("delete.folder", name = name) } else { t!("delete.file", name = name) };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (path, view) = (path.clone(), view.clone());
            alert
                .confirm()
                .title(question.clone())
                .description(t!("delete.cannot_undo"))
                .ok_text(t!("delete.ok"))
                .ok_variant(gpui_component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let path = path.clone();
                    let view = view.clone();
                    let task = cx.background_spawn({
                        let path = path.clone();
                        async move { crate::desktop::delete(&path) }
                    });
                    window
                        .spawn(cx, async move |cx| {
                            let result = task.await;
                            cx.update(|window, cx| match result {
                                Ok(()) => {
                                    view.update(cx, |view, cx| view.forget(position, &path, cx)).ok();
                                }
                                Err(error) => {
                                    window.push_notification(Notification::error(t!("note.failed", error = error)), cx)
                                }
                            })
                            .ok();
                        })
                        .detach();
                    true
                })
        });
    }

    /// Takes a removed entry out of the results right away; the index
    /// catches up (the service live, a scan of our own at the next check).
    fn forget(&mut self, position: usize, path: &std::path::Path, cx: &mut Context<Self>) {
        let still_there = self.results(cx).hits.path(position).is_some_and(|p| p == path);
        if still_there {
            let mut forgotten = None;
            self.table.update(cx, |table, cx| {
                let hits = &mut table.delegate_mut().hits;
                match hits {
                    Hits::Local { index, ids } if position < ids.len() => {
                        forgotten = Some((index.clone(), ids.remove(position)));
                    }
                    // The service follows within seconds; until then the
                    // entry is left out here.
                    Hits::Remote { .. } => hits.skip(position),
                    Hits::Local { .. } => {}
                }
                table.refresh(cx);
            });
            if let Some((index, id)) = forgotten {
                match &mut self.removed {
                    Some((known, ids)) if Arc::ptr_eq(known, &index) => {
                        ids.insert(id);
                    }
                    removed => *removed = Some((index, HashSet::from([id]))),
                }
            }
        }
        self.reconcile(cx);
        cx.notify();
    }

    /// After "Undo": the entry is back on disk.
    fn restored(&mut self, cx: &mut Context<Self>) {
        self.removed = None;
        self.reconcile(cx);
        self.search_again(true, cx);
    }

    /// Fetches the service rows around `visible` that are not here yet.
    pub(crate) fn fetch_rows(&mut self, visible: std::ops::Range<usize>, cx: &mut Context<Self>) {
        let Source::Service { client, .. } = &self.source else { return };
        let hits = &self.results(cx).hits;
        let Hits::Remote { total, .. } = hits else { return };
        let total = *total;
        if total == 0 {
            return;
        }
        // Blocks are the service's positions.
        let visible = hits.service_position(visible.start)..hits.service_position(visible.end);
        // One block of margin on both sides.
        let first = visible.start.saturating_sub(BLOCK) / BLOCK;
        let last = (visible.end + BLOCK).min(total - 1) / BLOCK;
        for block in first..=last {
            let start = block * BLOCK;
            if self.fetching.contains(&block) || self.results(cx).hits.has_service_row(start) {
                continue;
            }
            self.fetching.insert(block);
            let (client, generation) = (client.clone(), self.generation);
            cx.spawn(async move |this, cx| {
                let rows = cx
                    .background_spawn(async move { remote_rows(&client, start, BLOCK) })
                    .await;
                this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    this.fetching.remove(&block);
                    let Some(rows) = rows else { return };
                    this.table.update(cx, |table, cx| {
                        if let Hits::Remote { rows: known, .. } = &mut table.delegate_mut().hits {
                            known.extend(rows.into_iter().enumerate().map(|(i, row)| (start + i, row)));
                        }
                        cx.notify();
                    });
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    /// Asks the service for its state every two seconds: progress while it
    /// loads, a new search when its index changed, a reconnect if it went away.
    fn start_service_poll(&mut self, cx: &mut Context<Self>) {
        let Source::Service { socket, client } = &self.source else { return };
        let (socket, client) = (socket.clone(), client.clone());
        self.service_poll = cx.spawn(async move |this, cx| {
            loop {
                let (socket, client) = (socket.clone(), client.clone());
                let status = cx
                    .background_spawn(async move {
                        let mut guard = client.lock().unwrap();
                        if guard.is_none() {
                            *guard = Client::connect(&socket).ok();
                        }
                        let answer = guard.as_mut().map(|c| c.ask(&Request::Status));
                        match answer {
                            Some(Ok(Response::Status { state, entries, generation })) => Some((state, entries, generation)),
                            _ => {
                                *guard = None;
                                None
                            }
                        }
                    })
                    .await;
                let Ok(visible) = this.update(cx, |this, cx| {
                    this.service_status(status, cx);
                    this.visible
                }) else {
                    break;
                };
                // Hidden, there is only progress to keep track of.
                let pause = if visible { Duration::from_secs(1) } else { Duration::from_secs(5) };
                cx.background_executor().timer(pause).await;
            }
        });
    }

    fn service_status(&mut self, status: Option<(anything_proto::State, u64, u64)>, cx: &mut Context<Self>) {
        let Some((state, entries, generation)) = status else {
            // Stopped on purpose: the service removes its socket.
            // Gone otherwise (a crash), it is normally restarted at once;
            // if it stays away, go on without it.
            let since = *self.unreachable_since.get_or_insert_with(Instant::now);
            if !crate::source::service_socket().exists() || since.elapsed() > GIVE_UP_ON_SERVICE {
                self.unreachable_since = None;
                self.use_disk(cx);
                return;
            }
            self.status = Status::Unreachable;
            cx.notify();
            return;
        };
        self.unreachable_since = None;
        self.service_entries = entries;
        crate::source::set_index_size(entries as usize, cx);
        let was = self.status;
        self.status = match state {
            anything_proto::State::Ready => Status::Ready,
            anything_proto::State::Loading => Status::Loading,
            anything_proto::State::Scanning => Status::Scanning,
        };
        let changed = self.service_generation != Some(generation);
        self.service_generation = Some(generation);
        if changed || (was == Status::Unreachable && self.status == Status::Ready) {
            // Nobody looks at a hidden window: search again when it shows.
            if self.visible {
                self.search_again(true, cx);
            } else {
                self.outdated = true;
            }
        }
        cx.notify();
    }

    fn on_open(&mut self, _: &OpenItem, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::Open, self.target(cx), window, cx);
    }

    fn on_reveal(&mut self, _: &RevealItem, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::Reveal, self.target(cx), window, cx);
    }

    fn on_copy_path(&mut self, _: &CopyPath, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::CopyPath, self.target(cx), window, cx);
    }

    fn on_copy_name(&mut self, _: &CopyName, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::CopyName, self.target(cx), window, cx);
    }

    fn on_open_with(&mut self, _: &OpenWith, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::OpenWith, self.target(cx), window, cx);
    }

    fn on_copy_folder(&mut self, _: &CopyFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::CopyFolder, self.target(cx), window, cx);
    }

    fn on_copy_file(&mut self, _: &CopyFile, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::CopyFile, self.target(cx), window, cx);
    }

    fn on_properties(&mut self, _: &ShowProperties, window: &mut Window, cx: &mut Context<Self>) {
        self.run_on(Do::Properties, self.target(cx), window, cx);
    }

    fn on_trash(&mut self, _: &TrashItem, window: &mut Window, cx: &mut Context<Self>) {
        self.remove(self.target(cx), false, window, cx);
    }

    fn on_delete(&mut self, _: &DeleteItem, window: &mut Window, cx: &mut Context<Self>) {
        self.remove(self.target(cx), true, window, cx);
    }

    pub(crate) fn results<'a>(&self, cx: &'a App) -> &'a ResultTable {
        self.table.read(cx).delegate()
    }

    fn set_mode(&mut self, mode: ViewMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        // Keep the focus where it was: in the search field, or on the results.
        let in_results = self.table.focus_handle(cx).is_focused(window) || self.grid.focus.is_focused(window);
        self.mode = mode;
        self.save_state(window, cx);
        if in_results {
            match mode {
                ViewMode::List => {
                    let row = self.grid.selected.unwrap_or(0);
                    self.table.update(cx, |table, cx| table.set_selected_row(row, cx));
                    self.table.focus_handle(cx).focus(window, cx);
                }
                ViewMode::Grid => {
                    let row = self.table.read(cx).selected_row().unwrap_or(0);
                    self.grid.select(row);
                    self.grid.focus.focus(window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Brings the window up with the search text selected, ready to type over.
    pub fn show(&mut self, requested: Instant, window: &mut Window, cx: &mut Context<Self>) {
        self.shown_at.set(Some(requested));
        match &self.x11 {
            Some(x11) => x11.show(),
            None => window.activate_window(),
        }
        self.visible = true;
        if std::mem::take(&mut self.outdated) {
            self.search_again(true, cx);
        }
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        window.refresh();
    }

    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_state(window, cx);
        match &self.x11 {
            Some(x11) => x11.hide(),
            None => window.minimize_window(),
        }
        self.visible = false;
    }

    /// Larger or smaller tiles in the grid; saved with the window state when
    /// the window hides.
    pub(crate) fn zoom_grid(&mut self, steps: i32, cx: &mut Context<Self>) {
        if steps == 0 || !self.grid.zoom_by(steps) {
            return;
        }
        self.grid.keep_selection_visible();
        cx.notify();
    }

    /// Saves window placement, view, sorting and column widths.
    pub fn save_state(&self, window: &Window, cx: &App) {
        if !self.persist {
            return;
        }
        let results = self.results(cx);
        let position = self.x11.as_ref().and_then(|x11| {
            if self.visible { x11.current_position() } else { x11.remembered_position() }
        });
        let size = window.bounds().size;
        UiState {
            window: position.map(|(x, y)| Geometry {
                x,
                y,
                width: f32::from(size.width),
                height: f32::from(size.height),
            }),
            view: match self.mode {
                ViewMode::List => "list",
                ViewMode::Grid => "grid",
            }
            .into(),
            sort: sort_key_name(results.sort).into(),
            descending: results.descending,
            column_widths: results.widths.clone(),
            grid_zoom: Some(self.grid.zoom),
            settings_window: crate::settings_window::placement(cx),
            skipped_version: crate::update::skipped(cx),
        }
        .save();
    }

    pub fn toggle(&mut self, requested: Instant, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(window, cx);
        } else {
            self.show(requested, window, cx);
        }
    }

    fn bench_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bench = self.bench.clone().unwrap();
        let rows = self.table.read(cx).delegate().hits.len();
        bench.borrow_mut().set_rows(rows);
        let action = bench.borrow_mut().next_action();
        match action {
            Some(Action::SetQuery(query)) => {
                self.input.update(cx, |input, cx| input.set_value(query.clone(), window, cx));
                self.search(query, cx);
            }
            Some(Action::Sort(key, descending)) => {
                self.table.update(cx, |table, _| {
                    let results = table.delegate_mut();
                    results.sort = key;
                    results.descending = descending;
                });
                self.search_again(false, cx);
            }
            Some(Action::ScrollBy(pixels)) if self.mode == ViewMode::Grid => {
                let state = self.grid.scroll.0.borrow();
                let offset = state.base_handle.offset();
                state.base_handle.set_offset(point(offset.x, offset.y - px(pixels)));
            }
            Some(Action::JumpTo(row)) if self.mode == ViewMode::Grid => self.grid.select(row),
            Some(Action::ScrollBy(pixels)) => {
                self.table.update(cx, |table, cx| {
                    let state = table.vertical_scroll_handle.0.borrow();
                    let offset = state.base_handle.offset();
                    state.base_handle.set_offset(point(offset.x, offset.y - px(pixels)));
                    drop(state);
                    cx.notify();
                });
            }
            Some(Action::JumpTo(row)) => {
                self.table.update(cx, |table, cx| table.scroll_to_row(row, cx));
            }
            Some(Action::ShowCycles(cycles)) => self.bench_show_cycles(cycles, window, cx),
            Some(Action::Quit) => {
                println!("{}", bench.borrow().report());
                cx.quit();
                return;
            }
            None => {}
        }
        cx.notify();
        window.refresh();
        cx.on_next_frame(window, Self::bench_tick);
    }

    fn bench_show_cycles(&mut self, cycles: usize, window: &mut Window, cx: &mut Context<Self>) {
        let bench = self.bench.clone().unwrap();
        let shown_at = self.shown_at.clone();
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..cycles {
                this.update_in(cx, |this, window, cx| this.hide(window, cx)).ok();
                cx.background_executor().timer(Duration::from_millis(300)).await;
                let requested = Instant::now();
                this.update_in(cx, |this, window, cx| this.show(requested, window, cx)).ok();
                let deadline = Instant::now() + Duration::from_secs(2);
                while shown_at.get().is_some() && Instant::now() < deadline {
                    cx.background_executor().timer(Duration::from_millis(1)).await;
                }
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            bench.borrow_mut().step_done();
        })
        .detach();
    }

    /// Keyboard flow between search field and list: arrow keys move into the
    /// list and back, typing in the list goes on in the search field.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let in_input = self.input.focus_handle(cx).is_focused(window);
        let in_grid = self.grid.focus.is_focused(window);
        let in_list = self.table.focus_handle(cx).is_focused(window) || in_grid;
        let shortcut = keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;
        match keystroke.key.as_str() {
            "escape" => self.hide(window, cx),
            "," if keystroke.modifiers.control => crate::settings_window::open(cx),
            "1" if keystroke.modifiers.control => self.set_mode(ViewMode::List, window, cx),
            "2" if keystroke.modifiers.control => self.set_mode(ViewMode::Grid, window, cx),
            // Tile size, like Ctrl+mouse wheel.
            "+" | "=" if keystroke.modifiers.control && self.mode == ViewMode::Grid => self.zoom_grid(1, cx),
            "-" if keystroke.modifiers.control && self.mode == ViewMode::Grid => self.zoom_grid(-1, cx),
            "0" if keystroke.modifiers.control && self.mode == ViewMode::Grid => {
                let steps = grid::DEFAULT_ZOOM as i32 - self.grid.zoom as i32;
                self.zoom_grid(steps, cx)
            }
            "down" | "pagedown" if in_input && !shortcut => {
                if self.table.read(cx).delegate().hits.is_empty() {
                    return;
                }
                match self.mode {
                    ViewMode::List => {
                        self.table.update(cx, |table, cx| table.set_selected_row(0, cx));
                        self.table.focus_handle(cx).focus(window, cx);
                    }
                    ViewMode::Grid => {
                        self.grid.select(0);
                        self.grid.focus.focus(window, cx);
                    }
                }
            }
            "left" | "right" | "up" | "down" | "pageup" | "pagedown" | "home" | "end"
                if in_grid && !shortcut =>
            {
                if !grid::move_selection(self, &keystroke.key, window, cx) {
                    return;
                }
            }
            "backspace" if in_list && !shortcut => {
                self.focus_input_at_end(window, cx);
                let mut text = self.input.read(cx).value().to_string();
                text.pop();
                self.input.update(cx, |input, cx| input.set_value(text.clone(), window, cx));
                self.search(text, cx);
            }
            _ if in_list && !shortcut => {
                let Some(typed) = keystroke.key_char.clone().filter(|t| !t.chars().any(char::is_control))
                else {
                    return;
                };
                self.focus_input_at_end(window, cx);
                self.input.update(cx, |input, cx| input.insert(typed, window, cx));
                let text = self.input.read(cx).value().to_string();
                self.search(text, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The list binds the arrow keys itself, so moving up past the first row
    /// is caught as an action before the list sees it.
    fn on_select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        let in_list = self.table.focus_handle(cx).is_focused(window);
        if in_list && self.table.read(cx).selected_row().unwrap_or(0) == 0 {
            self.table.update(cx, |table, cx| table.clear_selection(cx));
            self.focus_input_at_end(window, cx);
            cx.stop_propagation();
        }
    }

    pub(crate) fn focus_input_at_end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            let end = input.value().len();
            input.set_selected_range(end..end, cx);
        });
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (count, search_time) = {
            let results = self.table.read(cx).delegate();
            (results.hits.len(), results.search_time)
        };
        let summary = if self.status != Status::Ready {
                        let lang = crate::i18n::current();
                        match self.status {
                            Status::Applying => t!("status.applying_scope").into_owned(),
                            Status::Loading => t!("status.loading").into_owned(),
                            Status::Unreachable => t!("status.unreachable").into_owned(),
                            _ => {
                                let count = match &self.source {
                                    Source::Service { .. } => self.service_entries as usize,
                                    _ => self.progress.as_ref().map_or(0, |p| p.entries.load(Ordering::Relaxed)),
                                };
                                t!("status.scanning", count = crate::i18n::number(count, lang)).into_owned()
                            }
                        }
                    } else {
                        let ms = format!("{:.1}", search_time.as_secs_f64() * 1000.0);
                        let lang = crate::i18n::current();
                        let ms = ms.replace('.', lang.decimal().encode_utf8(&mut [0; 4]));
                        t!("status.objects", count = crate::i18n::number(count, lang), ms = ms).into_owned()
                    };
        // The selected file's path, shortened in the middle to what is left
        // beside the summary.
        let selected = self.target(cx).and_then(|p| self.results(cx).hits.path(p)).map_or_else(String::new, |path| {
            let text = crate::format::shown(anything_core::name::path_text(&path).into_owned(), Vec::new()).0;
            let measure = crate::results::text_width(window);
            let room = f32::from(window.viewport_size().width) - 24.0 - 16.0 - measure(&summary) - 4.0;
            crate::format::fit_path(&crate::format::tilde(&text), room, measure)
        });
        // A search that found nothing gets a word of help instead of an
        // empty list.
        let no_hits = {
            let results = self.results(cx);
            (self.status == Status::Ready && count == 0 && !results.query_text.trim().is_empty())
                .then(|| results.query_text.clone())
        };
        let shown_at = self.shown_at.clone();
        let bench = self.bench.clone();
        let trace = std::env::var_os("ANYTHING_TRACE").is_some();
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_sm()
            .capture_key_down(cx.listener(Self::on_key_down))
            .capture_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_name))
            .on_action(cx.listener(Self::on_open_with))
            .on_action(cx.listener(Self::on_copy_folder))
            .on_action(cx.listener(Self::on_copy_file))
            .on_action(cx.listener(Self::on_properties))
            .on_action(cx.listener(Self::on_trash))
            .on_action(cx.listener(Self::on_delete))
            .when_some(update_bar(cx), |this, bar| this.child(bar))
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(
                        Input::new(&self.input).large().prefix(
                            Icon::new(IconName::Search).text_color(cx.theme().muted_foreground),
                        ),
                    )
                    .child(
                        h_flex()
                            .child(
                                Button::new("view-list")
                                    .ghost()
                                    .large()
                                    .icon(gpui_kit_assets::IconName::List)
                                    .selected(self.mode == ViewMode::List)
                                    .tooltip(t!("tooltip.list"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_mode(ViewMode::List, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("view-grid")
                                    .ghost()
                                    .large()
                                    .icon(gpui_kit_assets::IconName::LayoutGrid)
                                    .selected(self.mode == ViewMode::Grid)
                                    .tooltip(t!("tooltip.grid"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_mode(ViewMode::Grid, window, cx)
                                    })),
                            ),
                    )
                    .child(
                        Button::new("open-settings")
                            .ghost()
                            .large()
                            .icon(IconName::Settings)
                            .tooltip(t!("tooltip.settings"))
                            .on_click(|_, _, cx| crate::settings_window::open(cx)),
                    ),
            )
            .map(|this| match self.mode {
                _ if no_hits.is_some() => this.child(no_results(no_hits.as_deref().unwrap_or_default(), cx)),
                ViewMode::List => this.child(
                    div().key_context(actions::RESULTS).flex_1().min_h_0().child(DataTable::new(&self.table)),
                ),
                ViewMode::Grid => {
                    let rows = count.div_ceil(self.grid.columns());
                    this.child(
                        v_flex().key_context(actions::RESULTS).flex_1().min_h_0().child(grid::render(self, rows, cx)),
                    )
                }
            })
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .gap_4()
                    .justify_between()
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis_start().whitespace_nowrap().child(selected))
                    .child(div().flex_none().child(summary)),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, _, _| {
                        if let Some(requested) = shown_at.take() {
                            let latency = requested.elapsed();
                            if trace {
                                eprintln!("anything: shown {:.1} ms after request", latency.as_secs_f64() * 1000.0);
                            }
                            if let Some(bench) = &bench {
                                bench.borrow_mut().record("show", latency);
                            }
                        }
                        if let Some(bench) = &bench {
                            bench.borrow_mut().painted();
                        }
                    },
                )
                .absolute()
                .size(px(1.)),
            )
    }
}

fn proto_sort(key: SortKey) -> Sort {
    match key {
        SortKey::Name => Sort::Name,
        SortKey::Path => Sort::Path,
        SortKey::Size => Sort::Size,
        SortKey::Modified => Sort::Modified,
    }
}

fn to_row(row: anything_proto::Row) -> RowInfo {
    RowInfo { name: row.name, dir: row.dir, is_dir: row.is_dir, size: row.size, modified: row.modified }
}

/// Searches in the service and fetches the first block of rows. `None` if
/// the service does not answer.
fn remote_search(
    client: &Mutex<Option<Client>>,
    text: &str,
    sort: SortKey,
    descending: bool,
    include: Vec<String>,
    exclude: Vec<String>,
) -> Option<Hits> {
    let mut guard = client.lock().unwrap();
    let c = guard.as_mut()?;
    let request = Request::Search { query: text.to_owned(), sort: proto_sort(sort), descending, include, exclude };
    let total = match c.ask(&request) {
        Ok(Response::Searched { total, .. }) => total as usize,
        _ => {
            *guard = None;
            return None;
        }
    };
    let mut rows = std::collections::HashMap::new();
    if total > 0
        && let Ok(Response::Rows { start, rows: first }) = c.ask(&Request::Rows { start: 0, count: BLOCK as u32 })
    {
        rows.extend(first.into_iter().enumerate().map(|(i, row)| (start as usize + i, to_row(row))));
    }
    Some(Hits::Remote { total, rows, skipped: Vec::new() })
}

fn remote_rows(client: &Mutex<Option<Client>>, start: usize, count: usize) -> Option<Vec<RowInfo>> {
    let mut guard = client.lock().unwrap();
    match guard.as_mut()?.ask(&Request::Rows { start: start as u32, count: count as u32 }) {
        Ok(Response::Rows { rows, .. }) => Some(rows.into_iter().map(to_row).collect()),
        _ => None,
    }
}

/// A thin bar above the search field while a newer version is available.
fn update_bar(cx: &App) -> Option<gpui::AnyElement> {
    use crate::{service::Outcome, update};
    let release = update::available(cx)?;
    let state = cx.global::<update::Updates>();
    let text = match &state.outcome {
        _ if state.busy => t!("update.installing").into_owned(),
        Some(Outcome::Refused) => t!("service.refused").into_owned(),
        Some(Outcome::Failed(error)) => t!("update.failed", error = error).into_owned(),
        Some(Outcome::NoPrompt(command)) => format!("{} {command}", t!("service.no_prompt")),
        _ => t!("update.available", version = release.version).into_owned(),
    };
    let page = release.page.clone();
    let main = if update::can_update() {
        Button::new("update-install")
            .primary()
            .small()
            .label(t!("update.install"))
            .disabled(state.busy)
            .on_click(|_, _, cx| update::install(crate::service::Action::Update, cx))
    } else {
        // A build run from its own folder cannot replace itself.
        let page = page.clone();
        Button::new("update-download").primary().small().label(t!("update.download")).on_click(move |_, _, cx| cx.open_url(&page))
    };
    Some(
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(text))
            .child(main)
            .child(
                Button::new("update-notes")
                    .ghost()
                    .small()
                    .label(t!("update.whats_new"))
                    .on_click(move |_, _, cx| cx.open_url(&page)),
            )
            .child(
                Button::new("update-skip")
                    .ghost()
                    .small()
                    .label(t!("update.skip"))
                    .disabled(state.busy)
                    .on_click(|_, _, cx| update::skip(cx)),
            )
            .into_any_element(),
    )
}

/// "No results for …" with a few examples of the search syntax.
fn no_results(query: &str, cx: &App) -> impl IntoElement {
    let (query, _) = crate::format::shown(query.trim().to_owned(), Vec::new());
    let example = |term: &'static str, meaning: &'static str| {
        h_flex()
            .gap_3()
            .child(
                div()
                    .px_1p5()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().secondary)
                    .font_family("monospace")
                    .child(t!(term).into_owned()),
            )
            .child(div().text_color(cx.theme().muted_foreground).child(t!(meaning).into_owned()))
    };
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap_3()
        .p_4()
        .child(
            div()
                .max_w(px(640.))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_lg()
                .child(t!("empty.title", query = query).into_owned()),
        )
        .child(div().text_color(cx.theme().muted_foreground).child(t!("empty.hint").into_owned()))
        .child(
            v_flex()
                .gap_2()
                .child(example("empty.ex_contains", "empty.ex_contains.desc"))
                .child(example("empty.ex_pattern", "empty.ex_pattern.desc"))
                .child(example("empty.ex_and", "empty.ex_and.desc")),
        )
}
