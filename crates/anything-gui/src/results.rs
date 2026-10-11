//! The current search result, from the in-process index or from the
//! service, and the table that shows it.

use std::{
    cell::Cell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anything_core::{Index, Query, SortKey};
use gpui::{
    App, Context, FontWeight, HighlightStyle, IntoElement, ParentElement as _, Styled as _, StyledText,
    TextRun, WeakEntity, Window, div, px, rems,
};
use gpui_component::{
    ActiveTheme as _,
    table::{Column, ColumnSort, TableDelegate, TableState},
};
use rust_i18n::t;

use crate::{actions, format, view::SearchView};

/// Translation key, sort key and default width of each column.
pub const COLUMNS: [(&str, SortKey, f32); 4] = [
    ("column.name", SortKey::Name, 360.0),
    ("column.path", SortKey::Path, 500.0),
    ("column.size", SortKey::Size, 100.0),
    ("column.modified", SortKey::Modified, 140.0),
];

/// Rows fetched from the service at a time.
pub const BLOCK: usize = 200;

/// What a result row shows.
#[derive(Clone, Debug)]
pub struct RowInfo {
    pub name: String,
    /// Folder the entry is in.
    pub dir: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
}

impl RowInfo {
    pub fn path(&self) -> PathBuf {
        anything_core::name::to_path(&self.dir).join(anything_core::name::to_os(&self.name))
    }
}

pub enum Hits {
    /// Ids in an index held by this process.
    Local { index: Arc<Index>, ids: Vec<u32> },
    /// Held by the service; rows arrive in blocks as they are needed, by
    /// the service's positions. `skipped` are positions of entries removed
    /// here that the service may not know about yet (sorted).
    Remote { total: usize, rows: HashMap<usize, RowInfo>, skipped: Vec<usize> },
}

/// The service's position for the `position` shown, given the sorted
/// positions left out.
fn service_position(skipped: &[usize], position: usize) -> usize {
    let mut at = position;
    for &gone in skipped {
        if gone <= at {
            at += 1;
        } else {
            break;
        }
    }
    at
}

impl Hits {
    pub fn len(&self) -> usize {
        match self {
            Hits::Local { ids, .. } => ids.len(),
            Hits::Remote { total, skipped, .. } => total - skipped.len(),
        }
    }

    /// The service's position for a position shown.
    pub fn service_position(&self, position: usize) -> usize {
        match self {
            Hits::Remote { skipped, .. } => service_position(skipped, position),
            Hits::Local { .. } => position,
        }
    }

    /// Leaves an entry out until the service has caught up.
    pub fn skip(&mut self, position: usize) {
        if let Hits::Remote { skipped, .. } = self {
            let at = service_position(skipped, position);
            if let Err(slot) = skipped.binary_search(&at) {
                skipped.insert(slot, at);
            }
        }
    }

    /// Whether the row at the service's `position` is here.
    pub fn has_service_row(&self, position: usize) -> bool {
        match self {
            Hits::Remote { rows, skipped, .. } => rows.contains_key(&service_position(skipped, position)),
            Hits::Local { ids, .. } => position < ids.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The row at `position`, if known (remote rows may still be on their way).
    pub fn row(&self, position: usize) -> Option<RowInfo> {
        match self {
            Hits::Local { index, ids } => ids.get(position).map(|&id| RowInfo {
                name: index.name(id).into_owned(),
                dir: index.dir_path(id),
                is_dir: index.is_dir(id),
                size: index.size(id),
                modified: index.modified(id),
            }),
            Hits::Remote { rows, skipped, .. } => rows.get(&service_position(skipped, position)).cloned(),
        }
    }

    pub fn path(&self, position: usize) -> Option<PathBuf> {
        match self {
            Hits::Local { index, ids } => ids.get(position).map(|&id| anything_core::name::to_path(&index.path(id))),
            Hits::Remote { rows, skipped, .. } => rows.get(&service_position(skipped, position)).map(RowInfo::path),
        }
    }

    pub fn index(&self) -> Option<&Arc<Index>> {
        match self {
            Hits::Local { index, .. } => Some(index),
            Hits::Remote { .. } => None,
        }
    }
}

pub struct ResultTable {
    pub(crate) hits: Hits,
    pub(crate) query_text: String,
    pub(crate) query: Rc<Query>,
    pub(crate) sort: SortKey,
    pub(crate) descending: bool,
    /// Column widths the user dragged, if any.
    pub(crate) widths: Vec<f32>,
    pub(crate) search_time: Duration,
    pub(crate) view: WeakEntity<SearchView>,
    /// A fetch of missing rows is already scheduled for this frame.
    fetch_scheduled: Cell<bool>,
}

impl ResultTable {
    pub fn new(sort: SortKey, descending: bool, widths: Vec<f32>, view: WeakEntity<SearchView>) -> Self {
        Self {
            hits: Hits::Remote { total: 0, rows: HashMap::new(), skipped: Vec::new() },
            query_text: String::new(),
            query: Rc::new(Query::parse("")),
            sort,
            descending,
            widths: if widths.len() == COLUMNS.len() { widths } else { Vec::new() },
            search_time: Duration::ZERO,
            view,
            fetch_scheduled: Cell::new(false),
        }
    }

    /// Asks the view to fetch the rows the table is missing, once per frame.
    fn want_rows(&self, cx: &mut Context<TableState<Self>>) {
        if self.fetch_scheduled.replace(true) {
            return;
        }
        let view = self.view.clone();
        let table = cx.entity().downgrade();
        cx.defer(move |cx| {
            let Some(table) = table.upgrade() else { return };
            let visible = table.read(cx).visible_range().rows().clone();
            table.update(cx, |table, _| table.delegate().fetch_scheduled.set(false));
            view.update(cx, |view, cx| view.fetch_rows(visible, cx)).ok();
        });
    }
}

impl TableDelegate for ResultTable {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.hits.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let (title, key, width) = COLUMNS[col_ix];
        let width = self.widths.get(col_ix).copied().unwrap_or(width);
        let column = Column::new(title, t!(title)).width(px(width));
        let column = match (key == self.sort, self.descending) {
            (true, false) => column.sort(ColumnSort::Ascending),
            (true, true) => column.sort(ColumnSort::Descending),
            (false, _) => column.sortable(),
        };
        if col_ix == 2 { column.text_right() } else { column }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        self.sort = COLUMNS[col_ix].1;
        self.descending = matches!(sort, ColumnSort::Descending);
        let view = self.view.clone();
        cx.defer(move |cx| {
            view.update(cx, |view, cx| view.search_again(false, cx)).ok();
        });
    }

    fn context_menu(
        &mut self,
        _: usize,
        menu: gpui_component::menu::PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> gpui_component::menu::PopupMenu {
        actions::menu(menu, window, cx)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let cell = div().whitespace_nowrap().overflow_hidden().text_ellipsis();
        let Some(row) = self.hits.row(row_ix) else {
            self.want_rows(cx);
            return cell;
        };
        match col_ix {
            0 => {
                let highlight = HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    color: Some(cx.theme().blue),
                    ..Default::default()
                };
                let ranges = self.query.highlights(&row.name);
                let (name, ranges) = format::shown(row.name, ranges);
                cell.child(StyledText::new(name).with_highlights(ranges.into_iter().map(|r| (r, highlight))))
            }
            1 => {
                let width = self.widths.get(1).copied().unwrap_or(COLUMNS[1].2);
                let padding = gpui_component::Size::default().table_cell_padding();
                let max = width - f32::from(padding.left + padding.right) - 2.0;
                let dir = format::shown(row.dir, Vec::new()).0;
                let fitted = format::fit_path(&format::tilde(&dir), max, text_width(window));
                // If even that is too long, the end stays.
                div()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis_start()
                    .text_color(cx.theme().muted_foreground)
                    .child(fitted)
            }
            2 if row.is_dir => cell,
            2 => cell.child(format::size(row.size)),
            _ => cell.text_color(cx.theme().muted_foreground).child(format::time(row.modified)),
        }
    }
}

/// Measures text in the size lists and the status bar use (`text_sm`).
/// Widths are remembered: lists repeat the same folder names over and over,
/// and shaping text anew for every row in every frame costs whole frames.
pub fn text_width(window: &Window) -> impl Fn(&str) -> f32 + '_ {
    thread_local! {
        static WIDTHS: std::cell::RefCell<HashMap<(String, u32), f32>> = std::cell::RefCell::new(HashMap::new());
    }
    let size = rems(0.875).to_pixels(window.rem_size());
    let font = window.text_style().font();
    move |text: &str| {
        let key = (text.to_owned(), f32::from(size).to_bits());
        if let Some(width) = WIDTHS.with(|w| w.borrow().get(&key).copied()) {
            return width;
        }
        let run = TextRun { len: text.len(), font: font.clone(), color: Default::default(), background_color: None, underline: None, strikethrough: None };
        let width = f32::from(window.text_system().shape_line(text.to_owned().into(), size, &[run], None).width());
        WIDTHS.with(|w| {
            let mut widths = w.borrow_mut();
            // Bounded: a long session sees many names.
            if widths.len() > 100_000 {
                widths.clear();
            }
            widths.insert(key, width);
        });
        width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_rows_shift_the_rest() {
        let row = |n: usize| RowInfo { name: n.to_string(), dir: "/".into(), is_dir: false, size: 0, modified: 0 };
        let mut hits = Hits::Remote { total: 5, rows: (0..5).map(|i| (i, row(i))).collect(), skipped: Vec::new() };
        hits.skip(1);
        hits.skip(2); // the row shown at 2 is the service's 3
        assert_eq!(hits.len(), 3);
        let names: Vec<String> = (0..3).map(|p| hits.row(p).unwrap().name).collect();
        assert_eq!(names, ["0", "2", "4"]);
        assert_eq!(hits.service_position(2), 4);
    }
}
