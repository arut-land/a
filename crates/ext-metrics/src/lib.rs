//! Accounting is a disposable projection of qualified execution facts. Prices
//! are supplied at query time; copying context never transfers execution.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ext::{Args, Cx, Extension, Registrar, Tool};
use protocol::{Body, CallFact, CallId, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct UsageTotal {
    pub known: u64,
    pub missing: u64,
}

impl UsageTotal {
    fn add(&mut self, value: Option<u64>) {
        match value.and_then(|n| self.known.checked_add(n)) {
            Some(n) => self.known = n,
            None => self.missing += 1,
        }
    }
}

/// Nanocurrency per count in `llm::Count::ALL` order. Signed adjustments
/// price subsets (such as cached input) without billing inclusive totals twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prices {
    pub version: String,
    pub currency: String,
    pub models: BTreeMap<String, [i64; 10]>,
}

impl Prices {
    fn cost(&self, fact: &CallFact, mut usage: llm::Usage) -> Option<u64> {
        if let Some(charge) = &fact.charge {
            return (charge.currency == self.currency).then_some(charge.nanos);
        }
        let rates = self.models.get(&fact.model)?;
        let mut cost = 0i128;
        for (count, rate) in llm::Count::ALL.into_iter().zip(rates) {
            if *rate != 0 {
                cost = cost.checked_add(i128::from((*usage.count(count))?) * i128::from(*rate))?;
            }
        }
        u64::try_from(cost).ok()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Total {
    pub attempts: u64,
    pub usage: [UsageTotal; 10],
    pub cost: UsageTotal,
    pub call_ns: UsageTotal,
    /// Sum of epoch-local elapsed envelopes. Cross-epoch gaps are unknown.
    pub critical_ns: UsageTotal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Filter {
    pub agent: Option<String>,
    pub side: Option<bool>,
    pub call: Option<CallId>,
}

#[derive(Default)]
pub struct Projection {
    logs: BTreeMap<SessionId, Conversation>,
    cached: Option<(SessionId, bool, Filter, Prices, Total)>,
}

struct Conversation {
    parent: Option<SessionId>,
    covered: ext::LogCursor,
    facts: Vec<CallFact>,
    uncertain: Vec<CallId>,
}

impl Projection {
    pub fn fold(&mut self, prefix: &ext::LogPrefix) {
        let log = self.logs.entry(prefix.session).or_insert_with(|| {
            self.cached = None;
            Conversation {
                parent: prefix.parent,
                covered: ext::LogCursor::default(),
                facts: Vec::new(),
                uncertain: Vec::new(),
            }
        });
        if prefix.reset {
            log.facts.clear();
            log.uncertain.clear();
            self.cached = None;
        }
        for entry in &prefix.entries {
            if entry.seq.get() <= prefix.inherited_through {
                continue;
            }
            let before = log.uncertain.len();
            match &entry.body {
                Body::ModelCall { r#for: None, .. } => log.uncertain.push(CallId(entry.seq)),
                Body::ModelResult { call, outcome, .. }
                    if entry.execution.is_some()
                        || !matches!(outcome, protocol::Outcome::Interrupted { .. }) =>
                {
                    log.uncertain.retain(|id| id != call)
                }
                Body::Decision {
                    call: Some(call),
                    d: protocol::Verdict::Retry { .. },
                    ..
                } => log.uncertain.retain(|id| id != call),
                _ => {}
            }
            if before != log.uncertain.len() {
                self.cached = None;
            }
            if let Some(fact) = entry
                .execution
                .as_ref()
                .and_then(|j| CallFact::from_json(j).ok())
                && fact.id.executing_session == prefix.session
            {
                match log.facts.binary_search_by_key(&fact.id, |f| f.id) {
                    Err(i) => {
                        log.facts.insert(i, fact);
                        self.cached = None;
                    }
                    Ok(_) => {}
                }
            }
        }
        log.covered = prefix.cursor;
    }

    pub fn cursors(&self) -> Vec<(SessionId, ext::LogCursor)> {
        self.logs
            .iter()
            .map(|(id, log)| (*id, log.covered))
            .collect()
    }

    pub fn own(&mut self, session: SessionId, prices: &Prices) -> Total {
        self.query(session, false, &Filter::default(), prices)
    }

    pub fn subtree(&mut self, root: SessionId, prices: &Prices) -> Total {
        self.query(root, true, &Filter::default(), prices)
    }

    pub fn query(
        &mut self,
        root: SessionId,
        descendants: bool,
        filter: &Filter,
        prices: &Prices,
    ) -> Total {
        if let Some((r, d, f, p, total)) = &self.cached
            && (*r, *d, f, p) == (root, descendants, filter, prices)
        {
            return total.clone();
        }
        let mut total = Total::default();
        let mut epochs = Vec::<(SessionId, u64, u64)>::new();
        for fact in self
            .logs
            .iter()
            .filter(|(session, _)| {
                **session == root
                    || descendants
                        && std::iter::successors(Some(**session), |id| self.logs.get(id)?.parent)
                            .take(self.logs.len() + 1)
                            .any(|id| id == root)
            })
            .flat_map(|(_, log)| &log.facts)
            .filter(|f| {
                filter.agent.as_deref().is_none_or(|a| a == f.agent)
                    && filter.side.is_none_or(|s| s == f.side)
                    && filter.call.is_none_or(|c| c == f.id.call)
            })
        {
            total.attempts += 1;
            let mut usage = fact
                .usage
                .as_ref()
                .and_then(|u| u.parse::<llm::Usage>().ok())
                .unwrap_or_default();
            for (sum, count) in total.usage.iter_mut().zip(llm::Count::ALL) {
                sum.add(*usage.count(count));
            }
            total.cost.add(prices.cost(fact, usage));
            total.call_ns.add(fact.elapsed_ns);
            if let Some(end) = fact.elapsed_ns.and_then(|n| fact.start_ns.checked_add(n)) {
                if let Some((_, start, last)) = epochs
                    .iter_mut()
                    .find(|(epoch, _, _)| *epoch == fact.clock_epoch)
                {
                    *start = (*start).min(fact.start_ns);
                    *last = (*last).max(end);
                } else {
                    epochs.push((fact.clock_epoch, fact.start_ns, end));
                }
            } else {
                total.critical_ns.missing += 1;
            }
        }
        for (session, log) in &self.logs {
            if *session == root
                || descendants
                    && std::iter::successors(Some(*session), |id| self.logs.get(id)?.parent)
                        .take(self.logs.len() + 1)
                        .any(|id| id == root)
            {
                let missing = log
                    .uncertain
                    .iter()
                    .filter(|id| filter.call.is_none_or(|c| c == **id))
                    .count() as u64;
                for count in &mut total.usage {
                    count.missing += missing;
                }
                total.cost.missing += missing;
                total.call_ns.missing += missing;
                total.critical_ns.missing += missing;
            }
        }
        total.critical_ns.missing += epochs.len().saturating_sub(1) as u64;
        for (_, start, end) in epochs {
            total.critical_ns.add(Some(end - start));
        }
        self.cached = Some((
            root,
            descendants,
            filter.clone(),
            prices.clone(),
            total.clone(),
        ));
        total
    }
}

pub struct Metrics;
impl ext::Provides<ext::CallFacts> for Metrics {}
impl Extension for Metrics {
    const NAME: &'static str = "metrics";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<ext::CallFacts>(Arc::new(ext::CallFacts))
            .tool(Query(Arc::new(Mutex::new(Projection::default()))));
    }
}

struct Query(Arc<Mutex<Projection>>);
#[derive(Deserialize)]
struct Selection {
    #[serde(default)]
    subtree: bool,
    agent: Option<String>,
    side: Option<bool>,
    call: Option<CallId>,
    prices: Prices,
}
impl Tool for Query {
    type In = Args;
    type Out = Total;
    const NAME: &'static str = "metrics";
    const DESCRIPTION: &'static str = "derive usage, timing and cost; JSON: subtree, agent, side, prices {version,currency,models}";
    const AUDIENCE: protocol::Audience = protocol::Audience::User;
    const REPLAY: ext::Replay = ext::Replay::Safe;
    async fn call(&self, Args { args }: &Args, cx: &Cx) -> Result<Total, String> {
        let selection: Selection = if args.trim().is_empty() {
            Selection {
                subtree: true,
                agent: None,
                side: None,
                call: None,
                prices: Prices {
                    version: "unpriced".into(),
                    currency: String::new(),
                    models: BTreeMap::new(),
                },
            }
        } else {
            serde_json::from_str(args).map_err(|e| e.to_string())?
        };
        let cursors = self.0.lock().map_err(|e| e.to_string())?.cursors();
        let logs = cx
            .service::<dyn ext::Logs>()
            .ok_or("no log reader")?
            .read(cx.session, selection.subtree, cursors)
            .await?;
        let mut projection = self.0.lock().map_err(|e| e.to_string())?;
        for log in &logs {
            projection.fold(log);
        }
        Ok(projection.query(
            cx.session,
            selection.subtree,
            &Filter {
                agent: selection.agent,
                side: selection.side,
                call: selection.call,
            },
            &selection.prices,
        ))
    }
}

#[cfg(test)]
mod tests;
