//! The transcript's rows, wrapped lazily: a frame wraps only the items it
//! shows (from the bottom, or from where the view was scrolled to), so a
//! long session costs no more to draw, resize or resume than a short one.

use protocol::view::Item;

use super::super::render::Viewport;
use super::super::text::Line;
use super::super::view::{self, Live, Look};
use super::{App, Rows};

impl App {
    /// Item `i`'s rows at `width`, wrapped now if out of date.
    pub(super) fn rows(&mut self, i: usize, width: usize) -> &[Line] {
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
            .is_none_or(|r| r.width != width || r.live.is_some() != streaming.is_some())
        {
            let look = Look {
                collapsed: self.collapsed.contains(&item.key()),
                verbose: self.verbose,
                deny: self.deny,
            };
            self.cache.insert(
                i,
                Rows {
                    width,
                    lines: if streaming.is_some() {
                        Vec::new()
                    } else {
                        view::presented_rows(item, &self.client.view().present(item), width, look)
                    },
                    live: streaming.map(|_| Live::default()),
                },
            );
        }
        if let Some(live) = streaming
            && let Some(rows) = self.cache.get_mut(&i)
            && let Some(state) = &mut rows.live
        {
            state.update(&mut rows.lines, live, width);
        }
        // Hidden items must not turn a viewport-sized walk into retained history.
        if self.cached(i).is_empty() {
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
    pub(super) fn cached(&self, i: usize) -> &[Line] {
        self.cache.get(&i).map_or(&[], |r| &r.lines)
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
        self.cache.retain(|i, _| {
            shown
                .first()
                .zip(shown.last())
                .is_some_and(|(first, last)| (first.0..=last.0).contains(i))
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
