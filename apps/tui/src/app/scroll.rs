//! The transcript's rows, wrapped lazily: a frame wraps only the items it
//! shows (from the bottom, or from where the view was scrolled to), so a
//! long session costs no more to draw, resize or resume than a short one.

use protocol::view::{Glyph, Item, Kind};

use super::super::render::Viewport;
use super::super::text::Line;
use super::super::view::{self, CardCursor, Cursor, Layout, Look};
use super::{App, Rows};

impl App {
    /// Index an item's wraps, keeping only the viewport's materialized rows.
    pub(super) fn rows(&mut self, i: usize, width: usize) -> RowSlice<'_> {
        let item = &self.client.view().items[i];
        let streaming = self
            .client
            .view()
            .streaming
            .as_ref()
            .filter(|s| matches!(item, Item::Reply { call, .. } if *call == s.call));
        if self
            .cache
            .get(&i)
            .is_none_or(|r| r.width != width || r.streaming != streaming.is_some())
        {
            let look = Look {
                collapsed: self.collapsed.contains(&item.key()),
                verbose: self.verbose,
                deny: self.deny,
            };
            let p = self.client.view().present(item);
            let indexed = matches!(p.kind, Kind::User | Kind::Assistant)
                || streaming.is_some()
                || (p.kind == Kind::Reply && p.glyph == Glyph::None && !p.body.text().is_empty());
            // Settled inline output is handed to the terminal in its entirety.
            let cursor = (indexed
                && !(self.viewport == Viewport::Inline && !self.verbose && item.settled()))
            .then(|| {
                Cursor::new(if p.kind == Kind::User {
                    Layout::User
                } else {
                    Layout::Reply
                })
            });
            let card = (matches!(p.kind, Kind::Tool | Kind::Command)
                && client::approval_parts(&p).is_empty()
                && !(self.viewport == Viewport::Inline && !self.verbose && item.settled()))
            .then(|| CardCursor::new(item, &p, width, look, usize::from(self.body).max(1)));
            let lines = if cursor.is_some() || card.is_some() {
                Vec::new()
            } else {
                view::presented_rows(item, &p, width, look)
            };
            let total = card.as_ref().map_or(lines.len(), CardCursor::len);
            self.cache.insert(
                i,
                Rows {
                    width,
                    lines,
                    cursor,
                    card,
                    start: 0,
                    total,
                    streaming: streaming.is_some(),
                },
            );
        }
        if let Some(rows) = self.cache.get_mut(&i)
            && let Some(cursor) = &mut rows.cursor
        {
            let (text, call, attempt) = match streaming {
                Some(s) => (s.text.as_str(), s.call, s.attempt),
                None => (source(item), protocol::CallId(item.key()), 0),
            };
            if cursor.update(text, width, call, attempt, usize::from(self.body).max(1))
                || rows.lines.is_empty()
            {
                rows.total = cursor.len();
                rows.start = rows.total.saturating_sub(usize::from(self.body).max(1));
                rows.lines = cursor.window(text, rows.start..rows.total, streaming.is_some());
            }
        }
        if let Some(rows) = self.cache.get_mut(&i)
            && let Some(card) = &rows.card
        {
            rows.start = rows.total.saturating_sub(usize::from(self.body).max(1));
            rows.lines = card.window(
                item,
                &self.client.view().present(item),
                rows.start..rows.total,
            );
        }
        if self.cache.get(&i).is_some_and(|r| r.total == 0) {
            self.cache.remove(&i);
        }
        self.cached(i)
    }

    /// Where the bottom-most `body` rows start: (item, row within it).
    fn bottom_start(&mut self, width: usize, body: usize) -> (usize, usize) {
        let mut need = body;
        for i in (0..self.client.view().items.len()).rev() {
            let n = self.rows(i, width).len();
            if n >= need {
                return (i, n - need);
            }
            need -= n;
        }
        (0, 0)
    }

    /// Item `i`'s rows as last wrapped by [`Self::rows`].
    pub(super) fn cached(&self, i: usize) -> RowSlice<'_> {
        self.cache.get(&i).map_or(
            RowSlice {
                lines: &[],
                start: 0,
                total: 0,
            },
            |rows| RowSlice {
                lines: &rows.lines,
                start: rows.start,
                total: rows.total,
            },
        )
    }

    /// The rows a `body`-tall transcript shows, as (item, row within it):
    /// from the anchor when scrolled, else the latest; reaching the end
    /// follows it again.
    pub(super) fn window(&mut self, width: usize, body: usize) -> Vec<(usize, usize)> {
        let (mut i, mut row) = match self.anchor {
            Some(at) => at,
            None => self.bottom_start(width, body),
        };
        if self.viewport == Viewport::Inline && !self.verbose && i < self.committed {
            (i, row) = (self.committed, 0);
        }
        let mut out = Vec::with_capacity(body);
        while out.len() < body && i < self.client.view().items.len() {
            let len = self.rows(i, width).len();
            let take = (body - out.len()).min(len.saturating_sub(row));
            out.extend((row..row + take).map(|row| (i, row)));
            (i, row) = (i + 1, 0);
        }
        if self.anchor.is_some() && out.len() < body {
            self.anchor = None;
            return self.window(width, body);
        }
        self.keep_window(&out);
        out
    }

    /// Keep only the items contributing rows to this viewport.
    pub(super) fn keep_window(&mut self, shown: &[(usize, usize)]) {
        self.cache.retain(|i, rows| {
            let mut visible = shown
                .iter()
                .filter(|(item, _)| item == i)
                .map(|(_, row)| *row);
            let Some(start) = visible.next() else {
                return false;
            };
            let end = visible.next_back().unwrap_or(start) + 1;
            if rows.start == start && rows.lines.len() == end - start {
                return true;
            }
            if let Some(cursor) = &rows.cursor {
                let item = &self.client.view().items[*i];
                let stream = self
                    .client
                    .view()
                    .streaming
                    .as_ref()
                    .filter(|s| matches!(item, Item::Reply { call, .. } if *call == s.call));
                let text = stream.map_or_else(|| source(item), |s| s.text.as_str());
                rows.lines = cursor.window(text, start..end, stream.is_some());
                rows.start = start;
            }
            if let Some(card) = &rows.card {
                let item = &self.client.view().items[*i];
                rows.lines = card.window(item, &self.client.view().present(item), start..end);
                rows.start = start;
            }
            true
        });
    }

    /// Scrolls `up` rows toward older output (negative: newer).
    pub(super) fn scroll_by(&mut self, up: i32) {
        let (width, body) = (self.width, usize::from(self.body));
        if width == 0 {
            return;
        }
        let (mut i, mut row) = match self.anchor {
            Some(at) => at,
            None => self.bottom_start(width, body),
        };
        let mut n = up.unsigned_abs() as usize;
        if up > 0 {
            // `row` may be one past an item's last row: the same place as
            // the next item's first.
            while n > row {
                n -= row;
                if i == 0 {
                    (row, n) = (0, 0);
                    break;
                }
                self.cache.remove(&i);
                i -= 1;
                row = self.rows(i, width).len();
            }
            self.anchor = Some((i, row - n));
        } else {
            row += n;
            loop {
                if i >= self.client.view().items.len() {
                    self.anchor = None;
                    return;
                }
                let len = self.rows(i, width).len();
                if row < len {
                    break;
                }
                self.cache.remove(&i);
                row -= len;
                i += 1;
            }
            self.anchor = Some((i, row));
        }
    }
}

fn source(item: &Item) -> &str {
    match item {
        Item::User { text, .. } | Item::Assistant { text, .. } | Item::Reply { text, .. } => text,
        _ => unreachable!("only text items have wrapping cursors"),
    }
}

pub(super) struct RowSlice<'a> {
    pub lines: &'a [Line],
    start: usize,
    total: usize,
}

impl RowSlice<'_> {
    pub fn len(&self) -> usize {
        self.total
    }
    pub fn get(&self, row: usize) -> Option<&Line> {
        self.lines.get(row.checked_sub(self.start)?)
    }
}
