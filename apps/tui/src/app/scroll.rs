//! The transcript's rows, wrapped lazily: a frame wraps only the items it
//! shows (from the bottom, or from where the view was scrolled to), so a
//! long session costs no more to draw, resize or resume than a short one.

use protocol::view::Item;

use super::super::text::Line;
use super::super::view::{self, Live, Look};
use super::{App, Rows};

impl App {
    /// Item `i`'s rows at `width`, wrapped now if out of date.
    pub(super) fn rows(&mut self, i: usize, width: usize) -> &[Line] {
        let len = self.client.view().items.len();
        if self.cache.len() != len {
            self.cache.resize_with(len, || None);
        }
        let item = &self.client.view().items[i];
        let streaming = self
            .client
            .view()
            .streaming
            .as_ref()
            .filter(|s| matches!(item, Item::Reply { call, .. } if *call == s.call));
        let slot = &mut self.cache[i];
        if let Some(live) = streaming {
            let rows = match slot {
                Some(rows) if rows.width == width && rows.live.is_some() => rows,
                _ => slot.insert(Rows {
                    width,
                    lines: Vec::new(),
                    live: Some(Live::default()),
                }),
            };
            if let Some(state) = &mut rows.live {
                state.update(&mut rows.lines, &live.text, width);
            }
        } else if slot
            .as_ref()
            .is_none_or(|r| r.width != width || r.live.is_some())
        {
            let look = Look {
                collapsed: self.collapsed.contains(&item.key()),
                verbose: self.verbose,
            };
            *slot = Some(Rows {
                width,
                lines: view::rows(item, width, look),
                live: None,
            });
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
        self.cache
            .get(i)
            .and_then(Option::as_ref)
            .map_or(&[], |r| &r.lines)
    }

    /// The rows a `body`-tall transcript shows, as (item, row within it):
    /// from the anchor when scrolled, else the latest; reaching the end
    /// follows it again.
    pub(super) fn window(&mut self, width: usize, body: usize) -> Vec<(usize, usize)> {
        let (mut i, mut row) = match self.anchor {
            Some(at) => at,
            None => self.bottom_start(width, body),
        };
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
        out
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
                row -= len;
                i += 1;
            }
            self.anchor = Some((i, row));
        }
    }
}
