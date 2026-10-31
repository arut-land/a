//! Seek metadata shares the reducer's decisions, never its settled payloads.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use crate::history::{Cut, Direction, Filter, Head, Located, Page, Request};
use crate::{Body, Entry, Epoch, HostPath, Seq, SessionId};

use super::{Change, Item, SessionView, Usage};

#[derive(Debug, Default)]
struct Record {
    facts: Vec<Seq>,
    dependencies: Vec<(usize, u64)>,
    keys: Vec<(u64, Seq)>,
    usage: Usage,
    answers: Vec<crate::history::Answer>,
}

/// A log-derived seek index with only reducer metadata and fact positions.
#[derive(Debug)]
pub struct Index {
    view: SessionView,
    records: Vec<Record>,
    sides: BTreeMap<String, crate::history::SideRef>,
    usage: Usage,
    recap: super::Recap,
    recap_start: usize,
}

impl Index {
    pub fn new(session: SessionId, workspace: HostPath) -> Self {
        Self {
            view: SessionView::from_log(session, workspace, std::iter::empty::<Entry>()),
            records: Vec::new(),
            sides: BTreeMap::new(),
            usage: Usage::default(),
            recap: super::Recap::default(),
            recap_start: 0,
        }
    }

    pub const fn through(&self) -> u64 {
        self.view.seq
    }

    pub fn apply(&mut self, entry: &Entry) {
        if entry.seq.get() <= self.through() {
            return;
        }
        let dependency = match &entry.body {
            Body::Decision {
                call: Some(call), ..
            } => self.view.call(*call),
            _ => None,
        };
        if let Body::Answer { call, value, .. } = &entry.body
            && let Some(index) = self.view.call(*call)
            && let Ok(answer) = value.parse::<crate::QuestionAnswer>()
        {
            self.records[index].answers.push(crate::history::Answer {
                seq: entry.seq,
                call: *call,
                approved: answer.accepted(),
            });
        }
        let old_len = self.view.items.len();
        let change = self.view.entry(entry);
        let mut changed = Vec::new();
        if let Some(change) = &change {
            affected(change, &mut changed);
        }
        drop(change);
        for index in changed {
            if index == self.records.len() {
                self.records.push(Record::default());
                if let Some(other) = dependency {
                    self.records[index]
                        .dependencies
                        .push((other, entry.seq.get()));
                }
                if let Body::Take { input, .. } = entry.body {
                    self.records[index].facts.push(input);
                }
            }
            self.completed_side(index, entry.seq.get());
            let record = &mut self.records[index];
            if record.facts.last() != Some(&entry.seq) {
                record.facts.push(entry.seq);
            }
            let item = &self.view.items[index];
            let key = item.key();
            if record.keys.last().is_none_or(|(_, old)| *old != key) {
                record.keys.push((entry.seq.get(), key));
            }
            if let Item::User { text, .. } = item {
                self.recap = super::Recap {
                    prompt: Some(super::short(text)),
                    ..super::Recap::default()
                };
                self.recap_start = index;
            }
            if index >= old_len
                && let Item::Tool {
                    tool,
                    by: crate::Audience::Model,
                    ..
                } = item
            {
                self.recap.tools.push(tool.clone());
            }
            match item {
                Item::Assistant { text, .. }
                | Item::Reply {
                    text,
                    purpose: None,
                    status: super::Status::Done,
                    ..
                } if !text.is_empty() => self.recap.reply = Some(super::short(text)),
                _ => {}
            }
            if let Item::Reply {
                usage,
                purpose: None,
                ..
            } = item
            {
                self.usage.input -= record.usage.input;
                self.usage.output -= record.usage.output;
                record.usage = usage.unwrap_or_default();
                self.usage.input += record.usage.input;
                self.usage.output += record.usage.output;
            }
            thin(&mut self.view.items[index]);
        }
        if old_len != self.view.items.len() && old_len != 0 {
            thin(&mut self.view.items[old_len - 1]);
        }
    }

    fn completed_side(&mut self, index: usize, revision: u64) {
        if let Item::Reply {
            purpose: Some(purpose),
            text,
            status: super::Status::Done,
            ..
        } = &self.view.items[index]
            && !text.trim().trim_matches('"').trim().is_empty()
        {
            self.sides
                .insert(purpose.clone(), crate::history::SideRef { index, revision });
        }
    }

    pub fn head(&self, generation: Epoch) -> Head {
        self.head_after(generation, 0)
    }

    pub fn head_after(&self, generation: Epoch, since: u64) -> Head {
        self.head_for(generation, since, &[])
    }

    pub fn head_for(&self, generation: Epoch, since: u64, purposes: &[String]) -> Head {
        Head {
            conversation: crate::history::Conversation::default(),
            cut: Cut {
                session: self.view.session.expect("an index belongs to a session"),
                generation,
                seq: self.through(),
            },
            count: self.records.len(),
            changed_from: self
                .records
                .iter()
                .position(|record| record.facts.last().is_some_and(|seq| seq.get() > since))
                .unwrap_or(self.records.len()),
            changed: self
                .records
                .iter()
                .enumerate()
                .filter_map(|(index, record)| {
                    (record.keys[0].0 <= since
                        && record.facts.last().is_some_and(|seq| seq.get() > since))
                    .then_some(index)
                })
                .collect(),
            answers: if since == 0 {
                Vec::new()
            } else {
                let mut answers: Vec<_> = self
                    .records
                    .iter()
                    .filter(|record| record.keys[0].0 <= since)
                    .flat_map(|record| record.answers.iter())
                    .filter(|answer| answer.seq.get() > since)
                    .cloned()
                    .collect();
                answers.sort_by_key(|answer| answer.seq);
                answers
            },
            state: self.view.state.clone(),
            sides: purposes
                .iter()
                .filter_map(|purpose| {
                    self.sides
                        .get(purpose)
                        .map(|reference| (purpose.clone(), *reference))
                })
                .collect(),
            model: self.view.items.iter().rev().find_map(|item| match item {
                Item::Reply {
                    model,
                    purpose: None,
                    ..
                } => Some(model.clone()),
                _ => None,
            }),
            usage: self.usage,
            recap: (since < self.through()).then(|| {
                let mut recap = self.recap.clone();
                recap.asks = u32::try_from(self.view.state.inbox().count()).unwrap_or(u32::MAX);
                for item in &self.view.items[self.recap_start.min(self.view.items.len())..] {
                    match item.phase() {
                        Some(crate::ledger::Phase::Settled(
                            crate::ledger::OutcomeKind::Failed
                            | crate::ledger::OutcomeKind::Interrupted,
                        )) => recap.failed += 1,
                        Some(phase)
                            if !matches!(phase, crate::ledger::Phase::Settled(_))
                                && !phase.asking() =>
                        {
                            recap.running += 1;
                        }
                        _ => {}
                    }
                }
                recap
            }),
        }
    }

    /// Reads only facts that contributed to the requested items at this cut.
    pub fn page(
        &self,
        request: &Request,
        mut read: impl FnMut(Seq) -> io::Result<Entry>,
    ) -> io::Result<Page> {
        let total = self
            .records
            .partition_point(|r| r.keys[0].0 <= request.cut.seq);
        let anchor = request.anchor.min(total);
        let mut page = Page {
            cut: request.cut,
            start: anchor,
            end: anchor,
            total,
            version: 0,
            fragment: None,
            items: Vec::new(),
        };
        let mut used = 0;
        let indices: Box<dyn Iterator<Item = usize>> = match request.direction {
            _ if request.text_offset.is_some() => Box::new(anchor..(anchor + 1).min(total)),
            Direction::After => Box::new(anchor..total),
            Direction::Before => Box::new((0..anchor).rev()),
        };
        for index in indices {
            let mut item = self.reconstruct(index, request, &mut read)?;
            let selected = match &request.filter {
                Filter::Transcript => true,
                Filter::Inputs => matches!(
                    item,
                    Item::User { .. }
                        | Item::Tool {
                            by: crate::Audience::User,
                            ..
                        }
                ),
                Filter::Search(query) => {
                    let shown = item.present();
                    shown.title.contains(query) || shown.body.text().contains(query)
                }
            };
            if selected && request.filter != Filter::Inputs {
                page.fragment = excerpt(&mut item, index, request, used);
                if page.fragment.is_some() {
                    page.start = page.start.min(index);
                    page.end = page.end.max(index + 1);
                }
            }
            let bytes = if selected {
                serde_json::to_vec(&item).map_err(io::Error::other)?.len()
            } else {
                0
            };
            if selected
                && !page.items.is_empty()
                && page.fragment.is_none()
                && used + bytes > request.bytes
            {
                break;
            }
            page.version = page.version.max(
                self.records[index]
                    .facts
                    .iter()
                    .filter(|seq| seq.get() <= request.cut.seq)
                    .map(|seq| seq.get())
                    .max()
                    .unwrap_or(0),
            );
            match request.direction {
                Direction::Before => page.start = index,
                Direction::After => page.end = index + 1,
            }
            if selected {
                used += bytes;
                page.items.push(Located { index, item });
                if used >= request.bytes || page.fragment.is_some() {
                    break;
                }
            }
        }
        if request.direction == Direction::Before {
            page.items.reverse();
        }
        Ok(page)
    }

    fn reconstruct(
        &self,
        index: usize,
        request: &Request,
        read: &mut impl FnMut(Seq) -> io::Result<Entry>,
    ) -> io::Result<Item> {
        let mut facts = BTreeSet::new();
        self.sources(index, request.cut.seq, &mut facts);
        let entries = facts
            .into_iter()
            .map(read)
            .collect::<io::Result<Vec<_>>>()?;
        let key = self.records[index]
            .keys
            .iter()
            .rev()
            .find(|(at, _)| *at <= request.cut.seq)
            .expect("item exists at cut")
            .1;
        // Recall edits the command, independently of its child-operation presentation.
        let recall = request.filter == Filter::Inputs
            && entries.iter().any(|entry| {
                entry.seq == key
                    && matches!(
                        entry.body,
                        Body::ToolCall {
                            by: crate::Audience::User,
                            ..
                        }
                    )
            });
        SessionView::from_log(
            request.cut.session,
            self.view.workspace.clone(),
            entries
                .iter()
                .filter(|entry| !recall || !matches!(entry.body, Body::Link { .. })),
        )
        .items
        .into_iter()
        .find(|item| item.key() == key)
        .ok_or_else(|| io::Error::other("history index did not reconstruct its item"))
    }

    fn sources(&self, index: usize, through: u64, facts: &mut BTreeSet<Seq>) {
        let record = &self.records[index];
        facts.extend(
            record
                .facts
                .iter()
                .copied()
                .filter(|seq| seq.get() <= through),
        );
        for &(other, at) in &record.dependencies {
            self.sources(other, at.min(through), facts);
        }
    }
}

fn excerpt(
    item: &mut Item,
    index: usize,
    request: &Request,
    used: usize,
) -> Option<crate::history::TextSpan> {
    let key = item.key();
    let (Item::User { text, .. } | Item::Assistant { text, .. } | Item::Reply { text, .. }) = item
    else {
        return None;
    };
    if text.len() <= request.bytes && request.text_offset.is_none() {
        return None;
    }
    let total = text.len();
    let matching = match (&request.filter, request.text_offset) {
        (Filter::Search(query), None) => text.find(query),
        _ => None,
    };
    let direction = if matching.is_some() {
        Direction::After
    } else {
        request.direction
    };
    let offset = request
        .text_offset
        .or(matching)
        .unwrap_or(match direction {
            Direction::Before => total,
            Direction::After => 0,
        })
        .min(total);
    let allowance = request.bytes.saturating_sub(used).max(4);
    let (mut start, mut end) = match direction {
        Direction::After => (offset, offset.saturating_add(allowance).min(total)),
        Direction::Before => (offset.saturating_sub(allowance), offset),
    };
    while !text.is_char_boundary(start) {
        start += 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match direction {
        Direction::After => {
            if end < total
                && matching.is_none()
                && let Some(newline) = text[start..end].rfind('\n')
            {
                end = start + newline + 1;
            }
            if text.as_bytes().get(end) == Some(&b'*') {
                let delimiters = text[..end]
                    .bytes()
                    .rev()
                    .take_while(|byte| *byte == b'*')
                    .count();
                end -= delimiters % 2;
            }
        }
        Direction::Before => {
            if start > 0
                && let Some(newline) = text[start..end].find('\n')
                && start + newline + 1 < end
            {
                start += newline + 1;
            } else if text.as_bytes().get(start) == Some(&b'*') {
                let delimiters = text[..start]
                    .bytes()
                    .rev()
                    .take_while(|byte| *byte == b'*')
                    .count();
                start -= delimiters % 2;
            }
        }
    }
    let markdown = super::md::Checkpoint::at(text, start);
    *text = text[start..end].to_owned();
    Some(crate::history::TextSpan {
        index,
        key,
        start,
        end,
        total,
        markdown,
    })
}

fn affected(change: &Change, out: &mut Vec<usize>) {
    match change {
        Change::Added { index, .. } | Change::Updated { index, .. } => out.push(*index),
        Change::Batch { changes } => {
            for change in changes {
                affected(change, out);
            }
        }
        _ => {}
    }
}

fn thin(item: &mut Item) {
    match item {
        Item::User { text, .. } => {
            let command = text
                .strip_prefix('/')
                .and_then(|s| s.split_whitespace().next())
                .map(|name| format!("/{name}"));
            *text = command.unwrap_or_default();
        }
        Item::Assistant { text, .. } => *text = String::new(),
        Item::Reply { text, status, .. } => {
            *text = String::new();
            if let super::Status::Failed { error } = status {
                *error = String::new();
            }
        }
        Item::Tool {
            input,
            outcome,
            reason,
            card,
            ..
        } => {
            *input = crate::Json::null();
            *outcome = None;
            *reason = None;
            *card = None;
        }
        Item::Child { input, fired, .. } => {
            *input = String::new();
            *fired = fired.take().map(|outcome| match outcome {
                crate::Outcome::Done { .. } => crate::Outcome::Done {
                    output: crate::Json::null().into(),
                },
                crate::Outcome::Failed { .. } => crate::Outcome::Failed {
                    error: String::new(),
                },
                crate::Outcome::Interrupted { .. } => crate::Outcome::Interrupted { output: None },
            });
        }
        Item::Hook { decision, .. } => *decision = String::new(),
        Item::Environment { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decode;

    #[test]
    fn a_child_command_remains_recallable_without_reintroducing_its_tool_card() {
        let session = SessionId::new();
        let call = crate::CallId::new(1).unwrap();
        let child = SessionId::new();
        let entries: Vec<_> = [
            Body::ToolCall {
                invoke: None,
                tool: "delegate".into(),
                input: crate::Json::encode(&serde_json::json!({"args":"do the work"})).unwrap(),
                id: None,
                card: None,
                by: crate::Audience::User,
            },
            Body::Link {
                call,
                child,
                input: "do the work".into(),
                intent: crate::Json::null(),
            },
            Body::Watch { child },
            Body::Fired {
                watch: Seq::new(3).unwrap(),
                outcome: crate::Outcome::Done {
                    output: crate::Json::encode("finished").unwrap().into(),
                },
            },
            Body::ToolResult {
                call,
                outcome: crate::Outcome::Done {
                    output: crate::Json::null().into(),
                },
                card: None,
                held: Vec::new(),
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(at, body)| Entry::new(Seq::new(at as u64 + 1).unwrap(), Epoch(1), body))
        .collect();
        let mut index = Index::new(session, "/w".into());
        for entry in &entries {
            index.apply(entry);
        }
        for filter in [Filter::Transcript, Filter::Inputs] {
            let page = index
                .page(
                    &Request {
                        cut: index.head(Epoch(1)).cut,
                        anchor: 0,
                        text_offset: None,
                        direction: Direction::After,
                        bytes: 65536,
                        filter: filter.clone(),
                    },
                    |seq| Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone()),
                )
                .unwrap();
            assert_eq!(page.items.len(), 1);
            match (&filter, &page.items[0].item) {
                (Filter::Transcript, Item::Child { .. }) => {}
                (
                    Filter::Inputs,
                    Item::Tool {
                        by: crate::Audience::User,
                        input,
                        ..
                    },
                ) => assert_eq!(Item::args(input), "do the work"),
                _ => panic!("unexpected {filter:?} page: {page:?}"),
            }
        }
    }

    #[test]
    fn public_heads_never_carry_internal_envelope_payloads() {
        let mut index = Index::new(SessionId::new(), HostPath::default());
        let entry = Entry::from_json(include_bytes!(
            "../../../../fixtures/wire/v40/daemon/entry_thread_input.json"
        ))
        .unwrap();
        index.apply(&entry);
        let head = index.head(Epoch(1));
        assert!(head.state.queued().all(|queued| queued.text.is_empty()));
        let encoded = serde_json::to_string(&head).unwrap();
        assert!(!encoded.contains("published context"));
        assert!(!encoded.contains("publications"));
        assert!(!encoded.contains("application/vnd.a.context"));
    }

    #[test]
    fn seeked_items_equal_canonical_replay_at_every_cut() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/vectors/view.json")).unwrap();
        for vector in vectors.as_array().unwrap() {
            let session = SessionId::new();
            let entries: Vec<Entry> = vector["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["t"] == "entry")
                .map(|message| Entry::from_json(&serde_json::to_vec(message).unwrap()).unwrap())
                .collect();
            let mut index = Index::new(session, HostPath::default());
            let mut canonical =
                SessionView::from_log(session, HostPath::default(), std::iter::empty::<Entry>());
            for entry in &entries {
                index.apply(entry);
                canonical.extend([entry]);
                let page = index
                    .page(
                        &Request {
                            text_offset: None,
                            cut: index.head(Epoch(0)).cut,
                            anchor: 0,
                            direction: Direction::After,
                            bytes: usize::MAX,
                            filter: Filter::Transcript,
                        },
                        |seq| Ok(entries.iter().find(|e| e.seq == seq).unwrap().clone()),
                    )
                    .unwrap();
                let items: Vec<_> = page.items.into_iter().map(|located| located.item).collect();
                assert_eq!(
                    items, canonical.items,
                    "{} at {}",
                    vector["name"], entry.seq
                );
            }
        }
    }

    #[test]
    fn seeking_an_old_cut_does_not_observe_a_later_command_replacement() {
        let session = SessionId::new();
        let mut index = Index::new(session, HostPath::default());
        let input = Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::Input {
                segments: "/bash hi".into(),
                mode: crate::Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        );
        index.apply(&input);
        let request = Request {
            text_offset: None,
            cut: index.head(Epoch(0)).cut,
            anchor: 0,
            direction: Direction::After,
            bytes: 4096,
            filter: Filter::Transcript,
        };
        let command = Entry::new(
            Seq::new(2).unwrap(),
            Epoch(0),
            Body::ToolCall {
                invoke: None,
                tool: "bash".into(),
                input: crate::Json::encode(&"hi").unwrap(),
                id: None,
                card: None,
                by: crate::Audience::User,
            },
        );
        index.apply(&command);
        let page = index
            .page(&request, |seq| {
                Ok(if seq == input.seq {
                    input.clone()
                } else {
                    command.clone()
                })
            })
            .unwrap();
        assert!(
            matches!(&page.items[..], [Located { item: Item::User { text, .. }, .. }] if text == "/bash hi")
        );
    }

    #[test]
    fn markdown_continuations_preserve_fences_and_inline_styles_at_range_boundaries() {
        use super::super::md::{self, Inline};
        let text = format!(
            "# {}\n  - bullet **bold** and `code`\n> quote\n```rust\n{}\n```\nend",
            "heading ***bold** `code` text ".repeat(20),
            "let value = 1; ".repeat(20)
        );
        let entry = Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::Assistant { text: text.clone() },
        );
        let mut index = Index::new(SessionId::new(), HostPath::default());
        index.apply(&entry);
        let characters = |runs: Vec<Inline<'_>>| {
            runs.into_iter()
                .flat_map(|run| {
                    let (style, text) = match run {
                        Inline::Text(text) => (0, text),
                        Inline::Bold(text) => (1, text),
                        Inline::Code(text) => (2, text),
                    };
                    text.chars().map(move |ch| (style, ch))
                })
                .collect::<Vec<_>>()
        };
        let whole = characters(md::fragment(&text, md::Checkpoint::default()));
        for bytes in [4, 17, 64, 256] {
            let mut offset = 0;
            let mut rendered = Vec::new();
            while offset < text.len() {
                let page = index
                    .page(
                        &Request {
                            cut: index.head(Epoch(0)).cut,
                            anchor: 0,
                            text_offset: Some(offset),
                            direction: Direction::After,
                            bytes,
                            filter: Filter::Transcript,
                        },
                        |_| Ok(entry.clone()),
                    )
                    .unwrap();
                let span = page.fragment.unwrap();
                let Item::Assistant { text, .. } = &page.items[0].item else {
                    panic!("assistant range")
                };
                rendered.extend(characters(md::fragment(text, span.markdown)));
                assert!(span.end > offset);
                offset = span.end;
            }
            assert_eq!(rendered, whole, "range allowance {bytes}");
        }
    }

    #[test]
    fn search_hits_deliver_the_match_with_continuations_for_the_remaining_context() {
        let text = format!(
            "{}needle{}",
            "prefix €\n".repeat(1024),
            "suffix\n".repeat(1024)
        );
        let entry = Entry::new(
            Seq::new(1).unwrap(),
            Epoch(1),
            Body::Assistant { text: text.clone() },
        );
        let mut index = Index::new(SessionId::new(), HostPath::default());
        index.apply(&entry);
        let request = Request {
            cut: index.head(Epoch(1)).cut,
            anchor: 0,
            text_offset: None,
            direction: Direction::After,
            bytes: 256,
            filter: Filter::Search("needle".into()),
        };
        let page = index.page(&request, |_| Ok(entry.clone())).unwrap();
        let fragment = page
            .fragment
            .as_ref()
            .expect("a search hit is a requested text range");
        assert!(fragment.start > 0 && fragment.end < text.len());
        let Item::Assistant {
            text: delivered, ..
        } = &page.items[0].item
        else {
            panic!("assistant text")
        };
        assert!(delivered.contains("needle"));
        assert!(delivered.len() <= request.bytes);
        assert_eq!(delivered, &text[fragment.start..fragment.end]);
        let preceding = index
            .page(
                &Request {
                    filter: Filter::Transcript,
                    text_offset: Some(fragment.start),
                    direction: Direction::Before,
                    ..request
                },
                |_| Ok(entry.clone()),
            )
            .unwrap();
        assert_eq!(preceding.fragment.as_ref().unwrap().end, fragment.start);
    }

    #[test]
    fn a_range_covering_its_whole_item_keeps_both_boundary_lines() {
        let session = SessionId::new();
        let text = "## heading\ncomplete final line";
        let entries = [
            Entry::new(
                Seq::new(1).unwrap(),
                Epoch(0),
                Body::Assistant { text: text.into() },
            ),
            Entry::new(
                Seq::new(2).unwrap(),
                Epoch(0),
                Body::Assistant {
                    text: "another item".into(),
                },
            ),
        ];
        let mut index = Index::new(session, HostPath::default());
        for entry in &entries {
            index.apply(entry);
        }
        for (direction, offset) in [(Direction::After, 0), (Direction::Before, text.len())] {
            let request = Request {
                cut: index.head(Epoch(0)).cut,
                anchor: 0,
                text_offset: Some(offset),
                direction,
                bytes: text.len(),
                filter: Filter::Transcript,
            };
            let page = index
                .page(&request, |seq| {
                    assert_eq!(seq.get(), 1);
                    Ok(entries[0].clone())
                })
                .unwrap();
            assert_eq!(page.items.len(), 1);
            let span = page.fragment.unwrap();
            assert_eq!((span.start, span.end), (0, text.len()));
            assert_eq!(page.items[0].item.present().body.text().as_ref(), text);
        }
    }

    #[test]
    fn an_oversized_item_is_read_to_completion_through_continuations() {
        let session = SessionId::new();
        let mut index = Index::new(session, HostPath::default());
        let text = "é **styled** text\n```rust\nlet value = 1;\n```\n".repeat(30_000);
        for seq in 1..=100 {
            index.apply(&Entry::new(
                Seq::new(seq).unwrap(),
                Epoch(0),
                Body::Assistant { text: text.clone() },
            ));
        }
        assert!(
            index
                .view
                .items
                .iter()
                .all(|item| matches!(item, Item::Assistant { text, .. } if text.is_empty()))
        );
        let mut request = Request {
            text_offset: Some(0),
            cut: index.head(Epoch(0)).cut,
            anchor: 99,
            direction: Direction::After,
            bytes: 4096,
            filter: Filter::Transcript,
        };
        let mut recovered = String::new();
        loop {
            let page = index
                .page(&request, |seq| {
                    assert_eq!(seq.get(), 100, "a one-item seek read another history item");
                    Ok(Entry::new(
                        seq,
                        Epoch(0),
                        Body::Assistant { text: text.clone() },
                    ))
                })
                .unwrap();
            assert_eq!((page.start, page.end), (99, 100));
            let span = page.fragment.unwrap();
            assert_eq!(span.start, recovered.len());
            let Item::Assistant { text: fragment, .. } = &page.items[0].item else {
                panic!("assistant range")
            };
            assert!(fragment.len() <= 4096);
            assert_eq!(
                span.markdown,
                super::super::md::Checkpoint::at(&text, span.start)
            );
            recovered.push_str(fragment);
            if span.end == span.total {
                break;
            }
            request.text_offset = Some(span.end);
        }
        assert_eq!(recovered, text);
    }
    #[test]
    fn side_heads_contain_only_declared_completed_coordinates_and_no_payloads() {
        let mut index = Index::new(SessionId::new(), "/w".into());
        let mut seq = 0;
        for (purpose, text) in [
            ("caption", "visible label".to_owned()),
            ("internal-note", "private-payload-marker".repeat(65_536)),
        ] {
            seq += 1;
            index.apply(&Entry::new(
                Seq::new(seq).unwrap(),
                Epoch(1),
                Body::ModelCall {
                    model: "m".into(),
                    purpose: Some(purpose.into()),
                    r#for: None,
                    attempt: 0,
                    choice: crate::ToolChoice::None,
                    request: None,
                },
            ));
            let call = crate::CallId::new(seq).unwrap();
            seq += 1;
            index.apply(&Entry::new(Seq::new(seq).unwrap(), Epoch(1), Body::ModelResult {
                call, r#for: None, failure: None,
                outcome: crate::Outcome::Done { output: crate::Json::encode(&serde_json::json!({"message":{"content":[{"type":"text","text":text}]}})).unwrap() },
            }));
        }
        assert!(index.head(Epoch(1)).sides.is_empty());
        let head = index.head_for(Epoch(1), 0, &["caption".into()]);
        assert_eq!(
            head.sides,
            BTreeMap::from([(
                "caption".into(),
                crate::history::SideRef {
                    index: 0,
                    revision: 2
                }
            )])
        );
        let encoded = serde_json::to_string(&head).unwrap();
        assert!(!encoded.contains("visible label"));
        assert!(!encoded.contains("private-payload-marker"));
        assert!(!format!("{index:?}").contains("private-payload-marker"));
        assert!(
            index
                .view
                .items
                .iter()
                .all(|item| matches!(item, Item::Reply { text, .. } if text.is_empty()))
        );
        index.apply(&Entry::new(
            Seq::new(5).unwrap(),
            Epoch(1),
            Body::ModelCall {
                model: "m".into(),
                purpose: Some("caption".into()),
                r#for: None,
                attempt: 0,
                choice: crate::ToolChoice::None,
                request: None,
            },
        ));
        assert_eq!(
            index.head_for(Epoch(1), 2, &["caption".into()]).sides,
            head.sides
        );
        index.apply(&Entry::new(
            Seq::new(6).unwrap(),
            Epoch(1),
            Body::ModelResult {
                call: crate::CallId::new(5).unwrap(),
                r#for: None,
                failure: None,
                outcome: crate::Outcome::Failed {
                    error: "failed replacement".into(),
                },
            },
        ));
        assert_eq!(
            index.head_for(Epoch(1), 2, &["caption".into()]).sides,
            head.sides
        );
    }
    #[test]
    fn blank_side_results_preserve_the_latest_usable_coordinates() {
        let mut index = Index::new(SessionId::new(), "/w".into());
        for (at, text) in ["usable value", " \" \" "].into_iter().enumerate() {
            let call = crate::CallId::new((at as u64) * 2 + 1).unwrap();
            index.apply(&Entry::new(
                call.0,
                Epoch(1),
                Body::ModelCall {
                    model: "m".into(),
                    purpose: Some("caption".into()),
                    r#for: None,
                    attempt: 0,
                    choice: crate::ToolChoice::None,
                    request: None,
                },
            ));
            index.apply(&Entry::new(Seq::new(call.get()+1).unwrap(),Epoch(1),Body::ModelResult {
                call,r#for:None,failure:None,
                outcome:crate::Outcome::Done {output:crate::Json::encode(&serde_json::json!({"message":{"content":[{"type":"text","text":text}]}})).unwrap()},
            }));
        }
        assert_eq!(
            index.head_for(Epoch(1), 0, &["caption".into()]).sides["caption"],
            crate::history::SideRef {
                index: 0,
                revision: 2
            }
        );
    }
}
