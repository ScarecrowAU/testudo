//! The network and console tables: gpui-kit `DataTable`s shared by the
//! inspector and the focus view, so columns sort and resize.
//!
//! One table state per list lives on the app. Each render hands its delegate
//! the rows to show, the scrub time and the current row; at most one of each
//! list is on screen at a time. The delegate keeps the user's sort and maps
//! table rows to items through `order`.

use std::cmp::Ordering;

use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableEvent, TableState};

use super::*;

/// A request as shown in the network table.
#[derive(Clone)]
pub(super) struct NetItem {
    pub started: f64,
    /// Seconds since the page opened.
    pub rel: f64,
    pub finished: Option<f64>,
    pub method: SharedString,
    pub status: Option<u16>,
    pub error: Option<SharedString>,
    pub kind: SharedString,
    pub url: SharedString,
    pub failed: bool,
}

impl NetItem {
    pub fn from_request(r: &crate::model::NetRow, opened_at: f64) -> Self {
        Self {
            started: r.started,
            rel: (r.started - opened_at) / 1000.0,
            finished: r.finished,
            method: r.method.clone().into(),
            status: r.status,
            error: r.error.clone().map(Into::into),
            kind: r.resource_type.clone().into(),
            url: r.url.clone().into(),
            failed: r.error.is_some() || r.status.is_some_and(|s| s >= 400),
        }
    }

    fn duration(&self) -> Option<f64> {
        self.finished.map(|f| f - self.started)
    }
}

/// A console message as shown in the console table.
#[derive(Clone)]
pub(super) struct ConsoleItem {
    pub at: f64,
    /// Seconds since the page opened.
    pub rel: f64,
    pub level: SharedString,
    pub text: SharedString,
}

/// Body rows are this tall, like the other lists.
const ROW_H: f32 = 22.;

/// The order to show `n` items in under `sort`: as given, or stably sorted by
/// `cmp(column, a, b)`.
fn sorted_order(
    n: usize,
    sort: Option<(usize, ColumnSort)>,
    cmp: impl Fn(usize, usize, usize) -> Ordering,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    match sort {
        Some((col, ColumnSort::Ascending)) => order.sort_by(|&a, &b| cmp(col, a, b)),
        Some((col, ColumnSort::Descending)) => order.sort_by(|&a, &b| cmp(col, b, a)),
        _ => {}
    }
    order
}

fn header_cell(name: SharedString, cx: &App) -> Div {
    div()
        .w_full()
        .truncate()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(name)
}

fn cell(text: impl Into<SharedString>, color: Hsla, mono: bool) -> Div {
    div()
        .w_full()
        .truncate()
        .text_xs()
        .text_color(color)
        .when(mono, |d| d.font_family(crate::fonts::mono()))
        .child(text.into())
}

// --- network -------------------------------------------------------------------

const NET_COLS: [(&str, &str, f32); 6] = [
    ("time", "Time", 76.),
    ("method", "Method", 80.),
    ("status", "Status", 68.),
    ("type", "Type", 90.),
    ("duration", "Duration", 84.),
    ("path", "Path", 720.),
];
const NET_DURATION: usize = 4;

pub(super) struct NetTable {
    rows: Rc<Vec<NetItem>>,
    order: Vec<usize>,
    sort: Option<(usize, ColumnSort)>,
    /// The scrub time: requests not started by then are dimmed, those still
    /// running are in flight. Infinite outside the focus view.
    t: f64,
    /// Item index of the request at the scrub time, highlighted.
    current: Option<usize>,
}

impl NetTable {
    fn new() -> Self {
        Self {
            rows: Rc::default(),
            order: Vec::new(),
            sort: None,
            t: f64::INFINITY,
            current: None,
        }
    }

    pub fn state(window: &mut Window, cx: &mut App) -> Entity<TableState<Self>> {
        cx.new(|cx| {
            TableState::new(Self::new(), window, cx)
                .sortable(true)
                .col_resizable(true)
                .col_movable(false)
                .col_selectable(false)
                .row_selectable(true)
        })
    }

    pub fn set(&mut self, rows: Rc<Vec<NetItem>>, t: f64, current: Option<usize>) {
        self.rows = rows;
        self.t = t;
        self.current = current;
        self.reorder();
    }

    pub fn item(&self, row: usize) -> Option<&NetItem> {
        self.order.get(row).and_then(|&i| self.rows.get(i))
    }

    /// The table row showing item `item`.
    pub fn row_of(&self, item: usize) -> Option<usize> {
        self.order.iter().position(|&i| i == item)
    }

    fn reorder(&mut self) {
        let rows = &self.rows;
        self.order = sorted_order(rows.len(), self.sort, |col, a, b| {
            let (a, b) = (&rows[a], &rows[b]);
            match col {
                0 => a.started.total_cmp(&b.started),
                1 => a.method.cmp(&b.method),
                2 => a.status.cmp(&b.status),
                3 => a.kind.cmp(&b.kind),
                NET_DURATION => a.duration().partial_cmp(&b.duration()).unwrap_or(Ordering::Equal),
                _ => url_path(&a.url).cmp(url_path(&b.url)),
            }
        });
    }
}

impl TableDelegate for NetTable {
    fn columns_count(&self, _: &App) -> usize {
        NET_COLS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, col: usize, _: &App) -> Column {
        let (key, name, width) = NET_COLS[col];
        let c = Column::new(key, name).width(px(width)).sortable();
        if col == NET_DURATION { c.text_right() } else { c }
    }

    fn perform_sort(&mut self, col: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        self.sort = Some((col, sort));
        self.reorder();
    }

    fn render_th(&mut self, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        header_cell(self.column(col, cx).name, cx)
    }

    fn render_tr(&mut self, row: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let current = self.current.is_some() && self.order.get(row) == self.current.as_ref();
        let tip: SharedString = match self.item(row) {
            Some(NetItem {
                url, error: Some(e), ..
            }) => format!("{url}\n{e}").into(),
            Some(r) => r.url.clone(),
            None => SharedString::default(),
        };
        div()
            .id(("net-row", row))
            .h(px(ROW_H))
            .cursor_pointer()
            .when(current, |d| d.bg(cx.theme().list_active))
            .tooltip(tooltip_text(tip))
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let Some(r) = self.item(row) else { return div() };
        let state = row_state(r.started, r.finished, self.t);
        let color = match state {
            RowState::Pending => theme.foreground.opacity(0.3),
            RowState::InFlight => theme.blue,
            RowState::Done if r.failed => theme.red,
            RowState::Done => theme.foreground,
        };
        match col {
            0 => cell(format!("{:.3}s", r.rel), color, true),
            1 => cell(r.method.clone(), color, false),
            2 => cell(
                match state {
                    RowState::Pending => String::new(),
                    RowState::InFlight => "…".to_owned(),
                    RowState::Done => r
                        .status
                        .map(|s| s.to_string())
                        .or_else(|| r.error.as_ref().map(|_| "ERR".to_owned()))
                        .unwrap_or_default(),
                },
                color,
                true,
            ),
            3 => cell(r.kind.clone(), color, false),
            NET_DURATION => cell(
                match (state, r.duration()) {
                    (RowState::Done, Some(d)) => format!("{d:.0}ms"),
                    _ => String::new(),
                },
                color,
                true,
            )
            .text_right(),
            _ => cell(url_path(&r.url).to_owned(), color, false),
        }
    }

    fn cell_label(&self, row: usize, col: usize, _: &App) -> Option<SharedString> {
        let r = self.item(row)?;
        Some(match col {
            0 => format!("{:.3}s", r.rel).into(),
            1 => r.method.clone(),
            2 => r.status.map(|s| s.to_string()).unwrap_or_default().into(),
            3 => r.kind.clone(),
            NET_DURATION => r.duration().map(|d| format!("{d:.0}ms")).unwrap_or_default().into(),
            _ => r.url.clone(),
        })
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        div()
            .p_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child("No requests.")
    }
}

// --- console -------------------------------------------------------------------

const CONSOLE_COLS: [(&str, &str, f32); 3] = [
    ("time", "Time", 76.),
    ("level", "Level", 84.),
    ("message", "Message", 900.),
];

pub(super) struct ConsoleTable {
    rows: Rc<Vec<ConsoleItem>>,
    order: Vec<usize>,
    sort: Option<(usize, ColumnSort)>,
    /// The scrub time: later messages are dimmed. Infinite outside the focus view.
    t: f64,
}

impl ConsoleTable {
    fn new() -> Self {
        Self {
            rows: Rc::default(),
            order: Vec::new(),
            sort: None,
            t: f64::INFINITY,
        }
    }

    pub fn state(window: &mut Window, cx: &mut App) -> Entity<TableState<Self>> {
        cx.new(|cx| {
            TableState::new(Self::new(), window, cx)
                .sortable(true)
                .col_resizable(true)
                .col_movable(false)
                .col_selectable(false)
                .row_selectable(true)
        })
    }

    pub fn set(&mut self, rows: Rc<Vec<ConsoleItem>>, t: f64) {
        self.rows = rows;
        self.t = t;
        self.reorder();
    }

    pub fn item(&self, row: usize) -> Option<&ConsoleItem> {
        self.order.get(row).and_then(|&i| self.rows.get(i))
    }

    fn reorder(&mut self) {
        let rows = &self.rows;
        self.order = sorted_order(rows.len(), self.sort, |col, a, b| {
            let (a, b) = (&rows[a], &rows[b]);
            match col {
                0 => a.at.total_cmp(&b.at),
                1 => a.level.cmp(&b.level),
                _ => a.text.cmp(&b.text),
            }
        });
    }
}

impl TableDelegate for ConsoleTable {
    fn columns_count(&self, _: &App) -> usize {
        CONSOLE_COLS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, col: usize, _: &App) -> Column {
        let (key, name, width) = CONSOLE_COLS[col];
        Column::new(key, name).width(px(width)).sortable()
    }

    fn perform_sort(&mut self, col: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        self.sort = Some((col, sort));
        self.reorder();
    }

    fn render_th(&mut self, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        header_cell(self.column(col, cx).name, cx)
    }

    fn render_tr(&mut self, row: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let (later, tip) = self
            .item(row)
            .map_or((false, SharedString::default()), |c| (c.at > self.t, c.text.clone()));
        div()
            .id(("console-row", row))
            .h(px(ROW_H))
            .cursor_pointer()
            .when(later, |d| d.opacity(0.35))
            .tooltip(tooltip_text(tip))
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let Some(c) = self.item(row) else { return div() };
        let color = level_color(&c.level, cx);
        match col {
            0 => cell(format!("{:.3}s", c.rel), theme.muted_foreground, true),
            1 => div().child(pill(c.level.clone(), color)),
            _ => cell(c.text.clone(), color, false),
        }
    }

    fn cell_label(&self, row: usize, col: usize, _: &App) -> Option<SharedString> {
        let c = self.item(row)?;
        Some(match col {
            0 => format!("{:.3}s", c.rel).into(),
            1 => c.level.clone(),
            _ => c.text.clone(),
        })
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        div()
            .p_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child("No console messages.")
    }
}

fn level_color(level: &str, cx: &App) -> Hsla {
    let theme = cx.theme();
    match level {
        "error" => theme.red,
        "warning" | "warn" => theme.yellow,
        _ => theme.foreground,
    }
}

// --- app side ------------------------------------------------------------------

impl TestudoApp {
    /// Clicking a request or message seeks the focus view to it.
    pub(super) fn table_subscriptions(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(&self.net_table, window, |this, table, ev: &TableEvent, _, cx| {
                if let TableEvent::SelectRow(row) = ev
                    && let Some(at) = table.read(cx).delegate().item(*row).map(|r| r.started)
                {
                    this.seek_focus(at, cx);
                }
            }),
            cx.subscribe_in(&self.console_table, window, |this, table, ev: &TableEvent, _, cx| {
                if let TableEvent::SelectRow(row) = ev
                    && let Some(at) = table.read(cx).delegate().item(*row).map(|c| c.at)
                {
                    this.seek_focus(at, cx);
                }
            }),
        ]
    }

    fn seek_focus(&mut self, at: f64, cx: &mut Context<Self>) {
        if let Some(w) = self.focused_view() {
            self.with_focus(w, |f, page, _, _| f.seek(at, page));
            cx.notify();
        }
    }

    /// The network table showing `rows`, scrolled to `current` when `sync`.
    pub(super) fn net_table(
        &mut self,
        rows: Rc<Vec<NetItem>>,
        t: f64,
        current: Option<usize>,
        sync: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.net_table.update(cx, |s, cx| {
            s.delegate_mut().set(rows, t, current);
            if sync && let Some(row) = current.and_then(|i| s.delegate().row_of(i)) {
                s.scroll_to_row(row, cx);
            }
        });
        DataTable::new(&self.net_table)
            .with_size(px(ROW_H))
            .header_row_height(px(ROW_H + 4.))
            .bordered(false)
    }

    pub(super) fn console_table(
        &mut self,
        rows: Rc<Vec<ConsoleItem>>,
        t: f64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.console_table.update(cx, |s, _| s.delegate_mut().set(rows, t));
        DataTable::new(&self.console_table)
            .with_size(px(ROW_H))
            .header_row_height(px(ROW_H + 4.))
            .bordered(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `super::*` brings in gpui's `test` attribute; this is the std one.
    #[core::prelude::v1::test]
    fn sorting_is_stable_and_reversible() {
        let keys = [3, 1, 2, 1];
        let cmp = |_: usize, a: usize, b: usize| keys[a].cmp(&keys[b]);
        assert_eq!(sorted_order(4, None, cmp), [0, 1, 2, 3]);
        assert_eq!(sorted_order(4, Some((0, ColumnSort::Default)), cmp), [0, 1, 2, 3]);
        assert_eq!(sorted_order(4, Some((0, ColumnSort::Ascending)), cmp), [1, 3, 2, 0]);
        assert_eq!(sorted_order(4, Some((0, ColumnSort::Descending)), cmp), [0, 2, 1, 3]);
    }
}
