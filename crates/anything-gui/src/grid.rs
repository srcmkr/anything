//! Grid view: tiles with a thumbnail (or a type icon) and the name below.

use std::{cell::Cell, ops::Range, rc::Rc};

use gpui::{
    AnyElement, ClickEvent, Context, DispatchPhase, FocusHandle, FontWeight, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ObjectFit, ParentElement as _, ScrollDelta, ScrollStrategy,
    ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _, StyledImage as _, StyledText, TextRun,
    UniformListScrollHandle, Window, canvas, div, img, prelude::FluentBuilder as _, px, rems, uniform_list,
};
use gpui_component::{ActiveTheme as _, Icon, h_flex, menu::ContextMenuExt as _, v_flex};
use gpui_kit_assets::IconName;

use crate::{actions::Do, thumbs::Kind, view::SearchView};

/// Tile sizes, as factors of the normal one; Ctrl+mouse wheel steps through them.
pub const ZOOM_LEVELS: [f32; 6] = [0.6, 0.75, 1.0, 1.25, 1.5, 2.0];
pub const DEFAULT_ZOOM: usize = 2;
const PADDING: f32 = 8.;
/// Inside a tile: padding around, gap between preview and name.
const TILE_PADDING: f32 = 8.;
const TILE_GAP: f32 = 4.;
/// Two lines of `text_xs`.
const LABEL_HEIGHT: f32 = 32.;
/// Pixels of smooth scrolling (touchpad) per zoom step.
const PIXELS_PER_STEP: f32 = 60.;

/// The sizes of one zoom level.
#[derive(Clone, Copy)]
struct Size {
    tile_width: f32,
    thumb_height: f32,
    icon: f32,
}

impl Size {
    fn at(zoom: usize) -> Self {
        let z = ZOOM_LEVELS[zoom.min(ZOOM_LEVELS.len() - 1)];
        Size { tile_width: (184. * z).round(), thumb_height: (120. * z).round(), icon: (56. * z).round() }
    }

    fn tile_height(self) -> f32 {
        self.thumb_height + 2. * TILE_PADDING + TILE_GAP + LABEL_HEIGHT
    }

    fn label_width(self) -> f32 {
        self.tile_width - 2. * TILE_PADDING
    }
}

pub struct GridState {
    pub selected: Option<usize>,
    pub scroll: UniformListScrollHandle,
    /// Tiles per row, measured from the last layout.
    pub columns: Rc<Cell<usize>>,
    pub focus: FocusHandle,
    /// Index into [`ZOOM_LEVELS`].
    pub zoom: usize,
    /// Smooth scrolling not yet turned into a zoom step.
    zoom_pixels: f32,
}

impl GridState {
    pub fn new(focus: FocusHandle, zoom: usize) -> Self {
        Self {
            selected: None,
            scroll: UniformListScrollHandle::new(),
            columns: Rc::new(Cell::new(5)),
            focus,
            zoom,
            zoom_pixels: 0.,
        }
    }

    /// Larger (`+1`) or smaller (`-1`) tiles. Returns whether the size changed.
    pub fn zoom_by(&mut self, steps: i32) -> bool {
        let zoom = (self.zoom as i32 + steps).clamp(0, ZOOM_LEVELS.len() as i32 - 1) as usize;
        let changed = zoom != self.zoom;
        self.zoom = zoom;
        changed
    }

    /// Keeps the selected tile in view after the size changed.
    pub fn keep_selection_visible(&self) {
        if let Some(index) = self.selected {
            self.scroll.scroll_to_item(index / self.columns(), ScrollStrategy::Nearest);
        }
    }

    pub fn columns(&self) -> usize {
        self.columns.get().max(1)
    }

    pub fn reset(&mut self) {
        self.selected = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    pub fn select(&mut self, index: usize) {
        self.selected = Some(index);
        self.scroll.scroll_to_item(index / self.columns(), ScrollStrategy::Nearest);
    }

    /// Rows that fit on screen, for page up and down.
    pub fn page_rows(&self) -> usize {
        let height = self.scroll.0.borrow().base_handle.bounds().size.height;
        ((height / px(Size::at(self.zoom).tile_height())).floor() as usize).max(1)
    }
}

fn icon_for(kind: Kind) -> IconName {
    match kind {
        Kind::Folder => IconName::Folder,
        Kind::Image => IconName::Image,
        Kind::Video => IconName::Film,
        Kind::Audio => IconName::Music,
        Kind::Document => IconName::FileText,
        Kind::Spreadsheet => IconName::FileSpreadsheet,
        Kind::Presentation => IconName::Presentation,
        Kind::Archive => IconName::FileArchive,
        Kind::Code => IconName::FileCode,
        Kind::Other => IconName::File,
    }
}

/// A dezent colour per kind of file, for icons without a preview.
fn icon_color(kind: Kind, name: &str, cx: &gpui::App) -> Hsla {
    let theme = cx.theme();
    let color = match kind {
        Kind::Image => theme.magenta,
        Kind::Video => theme.blue,
        Kind::Audio => theme.green,
        Kind::Document if name.to_ascii_lowercase().ends_with(".pdf") => theme.red,
        Kind::Spreadsheet => theme.cyan,
        Kind::Presentation => theme.yellow,
        _ => return theme.muted_foreground,
    };
    color.opacity(0.8)
}

pub fn render(view: &SearchView, rows: usize, cx: &mut Context<SearchView>) -> impl IntoElement {
    let columns = view.grid.columns.clone();
    let tile_width = Size::at(view.grid.zoom).tile_width;
    let this = cx.entity().downgrade();
    div()
        .id("grid")
        .flex_1()
        .min_h_0()
        .relative()
        .track_focus(&view.grid.focus)
        .child(
            uniform_list(
                "grid-rows",
                rows,
                cx.processor(|view: &mut SearchView, range: Range<usize>, window, cx| {
                    range.map(|row| render_row(view, row, window, cx)).collect::<Vec<_>>()
                }),
            )
            .track_scroll(&view.grid.scroll)
            .size_full()
            .p(px(PADDING)),
        )
        .child(
            // Measures the width to decide how many tiles fit in a row.
            canvas(
                move |bounds, window, _| {
                    let fit = ((bounds.size.width - px(2. * PADDING)) / px(tile_width)).floor();
                    let fit = (fit as usize).max(1);
                    if fit != columns.get() {
                        columns.set(fit);
                        window.refresh();
                    }
                    bounds
                },
                // Ctrl+mouse wheel changes the tile size. Taken before the
                // list sees the event, so it does not scroll as well.
                move |_, bounds, window, _| {
                    let this = this.clone();
                    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                        if phase != DispatchPhase::Capture || !event.modifiers.control || !bounds.contains(&event.position) {
                            return;
                        }
                        cx.stop_propagation();
                        this.update(cx, |view, cx| {
                            let grid = &mut view.grid;
                            let steps = match event.delta {
                                ScrollDelta::Lines(lines) => -lines.y.signum() as i32,
                                ScrollDelta::Pixels(pixels) => {
                                    grid.zoom_pixels -= f32::from(pixels.y);
                                    let steps = (grid.zoom_pixels / PIXELS_PER_STEP).trunc();
                                    grid.zoom_pixels -= steps * PIXELS_PER_STEP;
                                    steps as i32
                                }
                            };
                            view.zoom_grid(steps, cx);
                        })
                        .ok();
                    });
                },
            )
            .absolute()
            .size_full(),
        )
}

fn render_row(view: &mut SearchView, row: usize, window: &mut Window, cx: &mut Context<SearchView>) -> AnyElement {
    let columns = view.grid.columns();
    let size = Size::at(view.grid.zoom);
    let (query, rows, start) = {
        let results = view.results(cx);
        let start = (row * columns).min(results.hits.len());
        let end = (start + columns).min(results.hits.len());
        (results.query.clone(), (start..end).map(|p| results.hits.row(p)).collect::<Vec<_>>(), start)
    };
    if rows.iter().any(Option::is_none) {
        view.fetch_rows(start..start + rows.len(), cx);
    }
    let highlight = HighlightStyle {
        font_weight: Some(FontWeight::BOLD),
        color: Some(cx.theme().blue),
        ..Default::default()
    };
    // Rows are centered; the last one is padded so its tiles line up.
    let missing = columns - rows.len();
    h_flex()
        .w_full()
        .justify_center()
        .children(rows.into_iter().enumerate().map(|(column, info)| {
            let position = row * columns + column;
            let Some(info) = info else {
                // Still on its way from the service.
                return div().flex_shrink_0().w(px(size.tile_width)).h(px(size.tile_height())).into_any_element();
            };
            let name = info.name.clone();
            let kind = crate::thumbs::kind(&name, info.is_dir);
            let thumb = (kind != Kind::Folder)
                .then(|| {
                    let path = info.path();
                    view.thumbs.update(cx, |thumbs, cx| thumbs.get(&path, info.size, info.modified, cx))
                })
                .flatten();
            let selected = view.grid.selected == Some(position);
            let (label, ranges) = crate::format::shown(name.clone(), query.highlights(&name));
            v_flex()
                .id(("tile", position))
                .flex_shrink_0()
                .w(px(size.tile_width))
                .h(px(size.tile_height()))
                .p(px(TILE_PADDING))
                .gap(px(TILE_GAP))
                .rounded(cx.theme().radius)
                .when(selected, |tile| tile.bg(cx.theme().table_active))
                .when(!selected, |tile| tile.hover(|tile| tile.bg(cx.theme().list_hover)))
                .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                    view.grid.selected = Some(position);
                    view.grid.focus.focus(window, cx);
                    if event.click_count() == 2 {
                        view.run_on(Do::Open, Some(position), window, cx);
                    }
                    cx.notify();
                }))
                // A right click selects the tile the context menu is about.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                        view.grid.selected = Some(position);
                        view.grid.focus.focus(window, cx);
                        cx.notify();
                    }),
                )
                // Previews sit on the name, as in file managers; what a
                // wide picture leaves free stays above it.
                .child(
                    div()
                        .flex_none()
                        .h(px(size.thumb_height))
                        .w_full()
                        .flex()
                        .items_end()
                        .justify_center()
                        .child(match thumb {
                            Some(path) => img(path).max_w_full().max_h_full().object_fit(ObjectFit::Contain).into_any_element(),
                            None => div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(Icon::new(icon_for(kind)).size(px(size.icon)).text_color(icon_color(kind, &name, cx)))
                                .into_any_element(),
                        }),
                )
                .child(tile_label(&label, &ranges, highlight, size.label_width(), window))
                .context_menu(|menu, window, cx| crate::actions::menu(menu, window, cx))
                .into_any_element()
        }))
        .children((0..missing).map(|_| div().w(px(size.tile_width)).flex_shrink_0()))
        .into_any_element()
}

enum Piece {
    Name(Range<usize>),
    Ellipsis,
}

/// Splits `name` into at most two lines of `max` width. Lines break after a
/// space, `_` or `-`, or before a `.`; a name too long for two lines loses
/// its middle, so the start and the extension stay visible.
fn label_lines(name: &str, max: f32, window: &mut Window) -> Vec<Vec<Piece>> {
    let font_size = rems(0.75).to_pixels(window.rem_size());
    let style = window.text_style();
    let shape = |text: &str| {
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(text.to_owned().into(), font_size, &[run], None)
    };
    let shaped = shape(name);
    let max = px(max);
    let len = name.len();
    if shaped.width() <= max {
        return vec![vec![Piece::Name(0..len)]];
    }
    let x = |at: usize| shaped.x_for_index(at);
    let total = shaped.width();
    let boundaries: Vec<usize> = name.char_indices().map(|(at, _)| at).chain([len]).collect();
    let fit = boundaries.iter().rev().copied().find(|&b| x(b) <= max).unwrap_or(0);
    let is_break = |b: usize| b > 0 && (name[b..].starts_with('.') || name[..b].ends_with([' ', '_', '-']));
    // The latest break after which the rest fits on the second line.
    if let Some(at) = boundaries.iter().rev().copied().find(|&b| b <= fit && is_break(b) && total - x(b) <= max) {
        return vec![vec![Piece::Name(0..at)], vec![Piece::Name(at..len)]];
    }
    // Too long for two lines: keep the start and the end, drop the middle.
    let first_end = boundaries
        .iter()
        .rev()
        .copied()
        .find(|&b| b <= fit && is_break(b) && x(b) >= max * 0.5)
        .unwrap_or(fit)
        .max(boundaries.get(1).copied().unwrap_or(len));
    if total - x(first_end) <= max {
        return vec![vec![Piece::Name(0..first_end)], vec![Piece::Name(first_end..len)]];
    }
    let ellipsis = shape("…").width();
    let tail = boundaries.iter().copied().find(|&b| b >= first_end && total - x(b) + ellipsis <= max).unwrap_or(len);
    vec![vec![Piece::Name(0..first_end)], vec![Piece::Ellipsis, Piece::Name(tail..len)]]
}

fn tile_label(name: &str, highlights: &[Range<usize>], style: HighlightStyle, width: f32, window: &mut Window) -> impl IntoElement {
    let lines = label_lines(name, width, window);
    v_flex().w_full().items_center().text_xs().children(lines.into_iter().map(|pieces| {
        let mut text = String::new();
        let mut marks = Vec::new();
        for piece in pieces {
            match piece {
                Piece::Ellipsis => text.push('…'),
                Piece::Name(range) => {
                    let offset = text.len();
                    for h in highlights {
                        let (start, end) = (h.start.max(range.start), h.end.min(range.end));
                        if start < end {
                            marks.push((offset + start - range.start..offset + end - range.start, style));
                        }
                    }
                    text.push_str(&name[range]);
                }
            }
        }
        div().whitespace_nowrap().child(StyledText::new(text).with_highlights(marks))
    }))
}

pub fn move_selection(view: &mut SearchView, key: &str, window: &mut Window, cx: &mut Context<SearchView>) -> bool {
    let count = view.results(cx).hits.len();
    if count == 0 {
        return false;
    }
    let columns = view.grid.columns();
    let page = columns * view.grid.page_rows();
    let current = view.grid.selected.unwrap_or(0);
    let next = match key {
        "left" => current.saturating_sub(1),
        "right" => (current + 1).min(count - 1),
        "up" if current < columns => {
            view.grid.selected = None;
            view.focus_input_at_end(window, cx);
            return true;
        }
        "up" => current - columns,
        "down" => (current + columns).min(count - 1),
        "pageup" => current.saturating_sub(page),
        "pagedown" => (current + page).min(count - 1),
        "home" => 0,
        "end" => count - 1,
        _ => return false,
    };
    view.grid.select(next);
    cx.notify();
    true
}
