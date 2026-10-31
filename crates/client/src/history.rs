//! Demand, immutable page sharing, and cancellation belong to the shared SDK.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use protocol::history::{Cut, Direction, Filter, Head, Page, Request, TextSpan};
use protocol::view::{Items, WeakItems};
use protocol::{ClientMsg, Decode};

/// A renderer's history anchor; moving it releases the preceding requested range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Anchor {
    #[default]
    Tail,
    Before(usize),
    After(usize),
    TextBefore {
        item: usize,
        offset: usize,
    },
    TextAfter {
        item: usize,
        offset: usize,
    },
}

/// Delivery allowance, independent of session length or a single item's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Demand {
    pub anchor: Anchor,
    pub filter: Filter,
    pub bytes: usize,
}

impl Default for Demand {
    fn default() -> Self {
        Self {
            anchor: Anchor::Tail,
            filter: Filter::Transcript,
            bytes: 64 << 10,
        }
    }
}

impl Demand {
    pub fn older(position: &protocol::history::Position) -> Option<Self> {
        Some(Self {
            anchor: adjacent(
                position.start..position.end,
                position.count,
                position.fragment.as_ref(),
                Direction::Before,
            )?,
            ..Self::default()
        })
    }

    pub fn newer(position: &protocol::history::Position) -> Option<Self> {
        Some(Self {
            anchor: adjacent(
                position.start..position.end,
                position.count,
                position.fragment.as_ref(),
                Direction::After,
            )?,
            ..Self::default()
        })
    }
}

/// An immutable range at a fixed log cut, including sparse item positions and
/// text continuations. Dropping the last reader releases its shared item storage.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryPage {
    pub cut: Cut,
    pub range: Range<usize>,
    pub total: usize,
    pub fragment: Option<TextSpan>,
    pub indices: Vec<usize>,
    pub items: Items,
    filter: Arc<Filter>,
    bytes: usize,
}

impl HistoryPage {
    pub fn older(&self) -> Option<Demand> {
        self.adjacent(Direction::Before)
    }
    pub fn newer(&self) -> Option<Demand> {
        self.adjacent(Direction::After)
    }

    fn adjacent(&self, direction: Direction) -> Option<Demand> {
        Some(Demand {
            anchor: adjacent(
                self.range.clone(),
                self.total,
                self.fragment.as_ref(),
                direction,
            )?,
            filter: self.filter.as_ref().clone(),
            bytes: self.bytes,
        })
    }
}

fn adjacent(
    range: Range<usize>,
    total: usize,
    fragment: Option<&TextSpan>,
    direction: Direction,
) -> Option<Anchor> {
    match direction {
        Direction::Before => {
            if let Some(fragment) = fragment
                && fragment.start > 0
            {
                Some(Anchor::TextBefore {
                    item: fragment.index,
                    offset: fragment.start,
                })
            } else {
                (range.start > 0).then_some(Anchor::Before(range.start))
            }
        }
        Direction::After => {
            if let Some(fragment) = fragment
                && fragment.end < fragment.total
            {
                Some(Anchor::TextAfter {
                    item: fragment.index,
                    offset: fragment.end,
                })
            } else {
                (range.end < total).then_some(Anchor::After(range.end))
            }
        }
    }
}

#[derive(Default, PartialEq)]
enum Tracking {
    #[default]
    Latest,
    Cut,
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 63);

struct Pending {
    id: u64,
    request: Request,
    invalidated: BTreeSet<usize>,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Side {
    pub history: History,
    pub reference: Option<(Cut, usize)>,
}

pub(crate) struct History {
    pub(crate) demand: Option<Demand>,
    cut: Option<Cut>,
    count: usize,
    loaded: Option<(usize, usize)>,
    pending: Option<Pending>,
    dirty: bool,
    tracking: Tracking,
}

impl Default for History {
    fn default() -> Self {
        Self {
            demand: Some(Demand::default()),
            cut: None,
            count: 0,
            loaded: None,
            pending: None,
            dirty: false,
            tracking: Tracking::Latest,
        }
    }
}

impl History {
    pub(crate) fn head(&mut self, head: &Head) -> Vec<ClientMsg> {
        let changed_generation = self.cut.is_some_and(|cut| {
            cut.session != head.cut.session || cut.generation != head.cut.generation
        });
        let mut messages = Vec::new();
        if self.cut.is_some_and(|cut| cut.session != head.cut.session) {
            self.demand = Some(Demand::default());
        }
        if changed_generation {
            if let Some(pending) = self.pending.take() {
                messages.push(ClientMsg::Cancel { id: pending.id });
            }
            self.loaded = None;
        }
        if let Some(pending) = &mut self.pending {
            pending.invalidated.extend(head.changed.iter().copied());
        }
        self.dirty |= self.demand.as_ref().is_some_and(|demand| {
            self.loaded.is_none_or(|(start, end)| {
                head.changed
                    .iter()
                    .any(|index| (start..end).contains(index))
            }) || (self.count != head.count
                && (demand.anchor == Anchor::Tail
                    || matches!(demand.anchor, Anchor::After(_))
                        && self.loaded.is_some_and(|(_, end)| end == self.count)))
        });
        self.cut = Some(head.cut);
        self.count = head.count;
        messages.extend(self.request());
        messages
    }

    pub(crate) fn demand(&mut self, demand: Demand) -> Vec<ClientMsg> {
        if self.demand.as_ref() == Some(&demand) && self.loaded.is_some() {
            return Vec::new();
        }
        let mut messages = Vec::new();
        if let Some(pending) = self.pending.take() {
            messages.push(ClientMsg::Cancel { id: pending.id });
        }
        self.demand = Some(demand);
        self.loaded = None;
        self.dirty = true;
        messages.extend(self.request());
        messages
    }

    pub(crate) fn release(&mut self) -> Option<ClientMsg> {
        self.demand = None;
        self.loaded = None;
        self.dirty = false;
        self.close()
    }

    pub(crate) fn owns(&self, id: u64) -> bool {
        self.pending.as_ref().is_some_and(|p| p.id == id)
    }

    pub(crate) fn same_generation(&self, cut: Cut) -> bool {
        self.cut
            .is_none_or(|known| known.session == cut.session && known.generation == cut.generation)
    }

    pub(crate) fn close(&mut self) -> Option<ClientMsg> {
        self.pending.take().map(|p| ClientMsg::Cancel { id: p.id })
    }

    pub(crate) fn at(&mut self, cut: Cut, count: usize, demand: Demand) -> Vec<ClientMsg> {
        if self.cut != Some(cut) {
            self.loaded = None;
        }
        self.tracking = Tracking::Cut;
        self.cut = Some(cut);
        self.count = count;
        self.demand(demand)
    }

    pub(crate) fn pending(&self) -> bool {
        self.cut.is_some() && (self.pending.is_some() || self.dirty)
    }

    fn request(&mut self) -> Option<ClientMsg> {
        let cut = self.cut?;
        let demand = self.demand.as_ref()?;
        if self.pending.is_some() || !self.dirty {
            return None;
        }
        let (anchor, direction, text_offset) = match demand.anchor {
            Anchor::Tail => (self.count, Direction::Before, None),
            Anchor::Before(anchor) => (anchor, Direction::Before, None),
            Anchor::After(anchor) => (anchor, Direction::After, None),
            Anchor::TextBefore { item, offset } => (item, Direction::Before, Some(offset)),
            Anchor::TextAfter { item, offset } => (item, Direction::After, Some(offset)),
        };
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = Request {
            cut,
            anchor,
            text_offset,
            direction,
            bytes: demand.bytes,
            filter: demand.filter.clone(),
        };
        self.pending = Some(Pending {
            id,
            request: request.clone(),
            invalidated: BTreeSet::new(),
            bytes: Vec::new(),
        });
        self.dirty = false;
        Some(ClientMsg::History { id, range: request })
    }

    pub(crate) fn part(
        &mut self,
        id: u64,
        offset: u64,
        text: &str,
        done: bool,
    ) -> io::Result<(Option<HistoryPage>, Vec<ClientMsg>)> {
        let Some(pending) = &mut self.pending else {
            return Ok((None, Vec::new()));
        };
        if pending.id != id {
            return Ok((None, Vec::new()));
        }
        if offset != pending.bytes.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history chunks arrived out of order",
            ));
        }
        pending.bytes.extend_from_slice(text.as_bytes());
        if !done {
            return Ok((None, Vec::new()));
        }
        let pending = self.pending.take().expect("completed pending history");
        let page = Page::from_json(&pending.bytes)?;
        if page.cut != pending.request.cut {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "history page belongs to another cut",
            ));
        }
        if pending
            .invalidated
            .range(page.start..page.end)
            .next()
            .is_some()
        {
            self.dirty = true;
            return Ok((None, self.request().into_iter().collect()));
        }
        self.loaded = Some((page.start, page.end));
        let start = page.start;
        let end = page.end;
        let page_cut = page.cut;
        let fragment = page.fragment.clone();
        let total = page.total;
        let indices = page.items.iter().map(|item| item.index).collect();
        let filter = Arc::new(pending.request.filter);
        let items = shared(page, filter.clone());
        self.dirty = self.demand.as_ref().is_some_and(|demand| {
            self.tracking == Tracking::Latest && demand.anchor == Anchor::Tail && end != self.count
        });
        Ok((
            Some(HistoryPage {
                cut: page_cut,
                fragment,
                items,
                range: start..end,
                total,
                indices,
                filter,
                bytes: pending.request.bytes,
            }),
            self.request().into_iter().collect(),
        ))
    }

    pub(crate) fn failed(&mut self, id: u64) -> bool {
        if self.pending.as_ref().is_none_or(|pending| pending.id != id) {
            return false;
        }
        self.pending = None;
        self.dirty = false;
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    session: protocol::SessionId,
    generation: protocol::Epoch,
    version: u64,
    start: usize,
    end: usize,
    filter: Arc<Filter>,
    fragment: Option<(usize, usize)>,
}

fn shared(page: Page, filter: Arc<Filter>) -> Items {
    static PAGES: OnceLock<Mutex<HashMap<Key, WeakItems>>> = OnceLock::new();
    let cache = PAGES.get_or_init(Mutex::default);
    let mut cache = cache.lock().expect("page cache lock poisoned");
    let key = Key {
        session: page.cut.session,
        generation: page.cut.generation,
        version: page.version,
        start: page.start,
        end: page.end,
        filter,
        fragment: page.fragment.as_ref().map(|f| (f.start, f.end)),
    };
    cache.retain(|_, items| items.upgrade().is_some());
    if let Some(items) = cache.get(&key).and_then(WeakItems::upgrade) {
        return items;
    }
    let items: Items = page.items.into_iter().map(|located| located.item).collect();
    cache.insert(key, items.downgrade());
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::view::Index;
    use protocol::{Body, Entry, Epoch, HostPath, Seq, SessionId};

    #[test]
    fn demanded_spawn_pages_deliver_child_items_without_tool_duplicates() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../protocol/tests/vectors/view.json")).unwrap();
        for vector in vectors.as_array().unwrap().iter().filter(|vector| {
            vector["name"]
                .as_str()
                .unwrap()
                .ends_with("spawns replace their tool rows")
        }) {
            let entries: Vec<_> = vector["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["t"] == "entry")
                .map(|message| Entry::from_json(&serde_json::to_vec(message).unwrap()).unwrap())
                .collect();
            let mut index = Index::new(SessionId::new(), HostPath::default());
            for entry in &entries {
                index.apply(entry);
            }
            let mut history = History::default();
            let (id, range) = request(&mut history, &index.head(Epoch(1)));
            let page = index
                .page(&range, |seq| {
                    Ok(entries
                        .iter()
                        .find(|entry| entry.seq == seq)
                        .unwrap()
                        .clone())
                })
                .unwrap();
            let (loaded, retry) = history
                .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
                .unwrap();
            let loaded = loaded.unwrap();
            assert!(retry.is_empty());
            assert_eq!(loaded.items.len(), 2);
            assert_eq!(
                loaded
                    .items
                    .iter()
                    .map(protocol::view::Item::key)
                    .collect::<BTreeSet<_>>()
                    .len(),
                2
            );
            assert!(
                loaded
                    .items
                    .iter()
                    .all(|item| matches!(item, protocol::view::Item::Child { .. }))
            );
        }
    }

    fn log() -> (Index, Vec<Entry>) {
        let entries: Vec<_> = (1..=3)
            .map(|seq| {
                Entry::new(
                    Seq::new(seq).unwrap(),
                    Epoch(1),
                    Body::Assistant {
                        text: format!("message {seq}"),
                    },
                )
            })
            .collect();
        let mut index = Index::new(SessionId::new(), HostPath::default());
        for entry in &entries {
            index.apply(entry);
        }
        (index, entries)
    }

    fn request(history: &mut History, head: &Head) -> (u64, Request) {
        history
            .head(head)
            .into_iter()
            .find_map(|msg| match msg {
                ClientMsg::History { id, range } => Some((id, range)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn a_fixed_cut_tail_finishes_even_when_the_live_session_has_grown() {
        let (mut index, mut entries) = log();
        let cut = index.head(Epoch(1)).cut;
        entries.push(Entry::new(
            Seq::new(4).unwrap(),
            Epoch(1),
            Body::Assistant {
                text: "later".into(),
            },
        ));
        index.apply(entries.last().unwrap());
        let mut history = History::default();
        let messages = history.at(cut, 4, Demand::default());
        let (id, range) = messages
            .into_iter()
            .find_map(|message| match message {
                ClientMsg::History { id, range } => Some((id, range)),
                _ => None,
            })
            .unwrap();
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        let (loaded, retry) = history
            .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
            .unwrap();
        assert_eq!(loaded.unwrap().total, 3);
        assert!(retry.is_empty());
        assert!(!history.pending());
    }

    #[test]
    fn identical_requested_ranges_share_storage_until_the_last_reader_releases_them() {
        let (index, entries) = log();
        let head = index.head(Epoch(1));
        let mut readers = [History::default(), History::default()];
        let mut pages = Vec::new();
        for history in &mut readers {
            let (id, range) = request(history, &head);
            let page = index
                .page(&range, |seq| {
                    Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
                })
                .unwrap();
            let encoded = serde_json::to_string(&page).unwrap();
            pages.push(
                history
                    .part(id, 0, &encoded, true)
                    .unwrap()
                    .0
                    .unwrap()
                    .items,
            );
        }
        assert!(std::ptr::eq(pages[0].as_ptr(), pages[1].as_ptr()));
        let weak = pages[0].downgrade();
        pages.pop();
        assert!(weak.upgrade().is_some());
        pages.pop();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn released_ranges_do_not_read_history_again_when_new_turns_commit() {
        let (mut index, mut entries) = log();
        let mut history = History::default();
        let (id, range) = request(&mut history, &index.head(Epoch(1)));
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        history
            .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
            .unwrap();
        assert!(history.release().is_none());
        entries.push(Entry::new(
            Seq::new(4).unwrap(),
            Epoch(1),
            Body::Assistant {
                text: "later".into(),
            },
        ));
        index.apply(entries.last().unwrap());
        let head = index.head(Epoch(1));
        assert!(history.head(&head).is_empty());
        assert!(!history.pending());
        assert!(
            matches!(history.demand(Demand { anchor: Anchor::After(3), ..Demand::default() }).as_slice(),
            [ClientMsg::History { range, .. }] if range.cut == head.cut && range.anchor == 3)
        );
    }

    #[test]
    fn restarting_a_scan_at_a_new_cut_requests_the_range_again() {
        let (mut index, mut entries) = log();
        let head = index.head(Epoch(1));
        let mut history = History::default();
        let (id, range) = request(&mut history, &head);
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        history
            .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
            .unwrap();
        entries.push(Entry::new(
            Seq::new(4).unwrap(),
            Epoch(1),
            Body::Assistant {
                text: "new committed reply".into(),
            },
        ));
        index.apply(entries.last().unwrap());
        let newer = index.head(Epoch(1));
        let sent = history.at(newer.cut, newer.count, Demand::default());
        assert!(
            matches!(sent.as_slice(), [ClientMsg::History { range, .. }] if range.cut == newer.cut)
        );
    }

    #[test]
    fn an_earlier_reply_completion_does_not_invalidate_an_unrelated_requested_range() {
        let mut index = Index::new(SessionId::new(), HostPath::default());
        let mut entries: Vec<_> = (1..=2)
            .map(|seq| {
                Entry::new(
                    Seq::new(seq).unwrap(),
                    Epoch(1),
                    Body::ModelCall {
                        request: None,
                        model: "m".into(),
                        attempt: 0,
                        choice: protocol::ToolChoice::Auto,
                        r#for: None,
                        purpose: None,
                    },
                )
            })
            .collect();
        for entry in &entries {
            index.apply(entry);
        }
        let original = index.head(Epoch(1));
        let mut history = History {
            demand: Some(Demand {
                anchor: Anchor::After(1),
                bytes: 1,
                ..Demand::default()
            }),
            ..History::default()
        };
        let (id, range) = request(&mut history, &original);
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        assert_eq!((page.start, page.end), (1, 2));
        entries.push(Entry::new(
            Seq::new(3).unwrap(),
            Epoch(1),
            Body::ModelResult {
                r#for: None,
                failure: None,
                call: protocol::CallId::new(1).unwrap(),
                outcome: protocol::Outcome::Done {
                    output: protocol::Json::encode(
                        &serde_json::json!({"text":"completed earlier"}),
                    )
                    .unwrap(),
                },
            },
        ));
        index.apply(entries.last().unwrap());
        let next = index.head_after(Epoch(1), original.cut.seq);
        assert_eq!(next.changed, [0]);
        history.head(&next);
        let (loaded, retry) = history
            .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
            .unwrap();
        assert!(loaded.is_some());
        assert!(retry.is_empty());
    }

    #[test]
    fn a_superseded_generation_cannot_install_a_late_page() {
        let (index, entries) = log();
        let mut history = History::default();
        let (id, range) = request(&mut history, &index.head(Epoch(1)));
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        let messages = history.head(&index.head(Epoch(2)));
        assert!(
            messages
                .iter()
                .any(|msg| matches!(msg, ClientMsg::Cancel { id: old } if *old == id))
        );
        let (loaded, _) = history
            .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
            .unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn an_item_changed_while_its_page_is_in_flight_is_requested_again() {
        let (index, entries) = log();
        let head = index.head(Epoch(1));
        let mut history = History::default();
        let (id, range) = request(&mut history, &head);
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        let mut replacement = head;
        replacement.cut.seq += 1;
        replacement.changed_from = 1;
        replacement.changed.push(1);
        history.head(&replacement);
        let (loaded, messages) = history
            .part(id, 0, &serde_json::to_string(&page).unwrap(), true)
            .unwrap();
        assert!(loaded.is_none());
        assert!(messages.iter().any(
            |msg| matches!(msg, ClientMsg::History { range, .. } if range.cut == replacement.cut)
        ));
    }
}
