//! Accounting is a disposable projection of qualified execution facts. Prices
//! are supplied at query time; copying context never transfers execution.

use std::sync::Mutex;

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
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "SuppliedPrices")]
pub struct Prices {
    pub version: String,
    pub currency: String,
    pub models: Vec<(String, [i64; 10])>,
}

type SuppliedPrices = (String, String, Vec<(String, [i64; 10])>);

impl From<SuppliedPrices> for Prices {
    fn from((version, currency, models): SuppliedPrices) -> Self {
        Self {
            version,
            currency,
            models,
        }
    }
}

impl Prices {
    #[inline(never)]
    fn parse(args: &str) -> Result<Self, String> {
        let mut prices = if args.trim().is_empty() {
            Self::default()
        } else {
            serde_json::from_str::<Self>(args).map_err(|e| e.to_string())?
        };
        prices.models.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        if prices.models.windows(2).any(|p| p[0].0 == p[1].0) {
            return Err("duplicate model price".into());
        }
        Ok(prices)
    }

    #[inline(never)]
    fn cost(&self, fact: &CallFact, usage: [Option<u64>; 10]) -> Option<u64> {
        if let Some(charge) = &fact.charge {
            return (charge.currency == self.currency).then_some(charge.nanos);
        }
        let index = self
            .models
            .binary_search_by(|(model, _)| model.cmp(&fact.model))
            .ok()?;
        let rates = &self.models[index].1;
        let mut cost = 0i128;
        for (count, rate) in usage.into_iter().zip(rates.iter()) {
            if *rate != 0 {
                cost = cost.checked_add(i128::from(count?) * i128::from(*rate))?;
            }
        }
        u64::try_from(cost).ok()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Attempts {
    pub executed: u64,
    pub ambiguous: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Total {
    pub attempts: Attempts,
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
    logs: Vec<(SessionId, Conversation)>,
    cached: Option<(SessionId, bool, Filter, Prices, Total)>,
}

struct Conversation {
    parent: Option<SessionId>,
    covered: ext::LogCursor,
    facts: Vec<CallFact>,
    uncertain: Vec<(CallId, u16)>,
}

impl Projection {
    #[inline(never)]
    pub fn fold(&mut self, prefix: &ext::LogPrefix) {
        let index = match self
            .logs
            .binary_search_by_key(&prefix.session, |(id, _)| *id)
        {
            Ok(i) => i,
            Err(i) => {
                self.cached = None;
                self.logs.insert(
                    i,
                    (
                        prefix.session,
                        Conversation {
                            parent: prefix.parent,
                            covered: ext::LogCursor::default(),
                            facts: Vec::new(),
                            uncertain: Vec::new(),
                        },
                    ),
                );
                i
            }
        };
        let log = &mut self.logs[index].1;
        if prefix.reset {
            log.facts.clear();
            log.uncertain.clear();
            self.cached = None;
        }
        let after = if prefix.reset { 0 } else { log.covered.seq };
        for entry in &prefix.entries {
            if entry.seq.get() <= after.max(prefix.inherited_through) {
                continue;
            }
            let before = log.uncertain.len();
            let fact = entry.execution.as_ref().and_then(|j| CallFact::from_json(j).ok()).filter(|f| {
                f.id.executing_session == prefix.session && matches!(&entry.body,
                    Body::ModelResult { call, .. } | Body::Decision { call: Some(call), .. } if *call == f.id.call)
            });
            match &entry.body {
                Body::ModelCall {
                    r#for: None,
                    attempt,
                    ..
                } => {
                    log.uncertain.push((CallId(entry.seq), *attempt));
                }
                Body::Decision {
                    call: Some(call),
                    d:
                        protocol::Verdict::Retry { attempt, .. }
                        | protocol::Verdict::SideRetry { attempt, .. },
                    ..
                } if log.uncertain.iter().any(|(id, _)| id == call) => {
                    log.uncertain.push((*call, *attempt));
                }
                Body::ModelResult { call, outcome, .. }
                    if entry.execution.is_none()
                        && !matches!(outcome, protocol::Outcome::Interrupted { .. }) =>
                {
                    if let Some(i) = log.uncertain.iter().rposition(|(id, _)| id == call) {
                        log.uncertain.remove(i);
                    }
                }
                _ => {}
            }
            if let Some(fact) = &fact {
                log.uncertain
                    .retain(|id| *id != (fact.id.call, fact.id.attempt));
            }
            if before != log.uncertain.len() {
                self.cached = None;
            }
            if let Some(fact) = fact
                && let Err(i) = log.facts.binary_search_by_key(&fact.id, |f| f.id)
            {
                log.facts.insert(i, fact);
                self.cached = None;
            }
        }
        if prefix.reset || prefix.cursor.seq >= log.covered.seq {
            log.covered = prefix.cursor;
        }
    }

    pub fn cursors(&self) -> Vec<(SessionId, ext::LogCursor)> {
        self.logs
            .iter()
            .map(|(id, log)| (*id, log.covered))
            .collect()
    }

    fn selected(&self, session: SessionId, root: SessionId, descendants: bool) -> bool {
        session == root
            || descendants
                && std::iter::successors(Some(session), |id| {
                    self.logs
                        .binary_search_by_key(id, |(id, _)| *id)
                        .ok()
                        .and_then(|i| self.logs[i].1.parent)
                })
                .take(self.logs.len() + 1)
                .any(|id| id == root)
    }

    pub fn own(&mut self, session: SessionId, prices: &Prices) -> Total {
        self.query(session, false, &Filter::default(), prices)
    }

    pub fn subtree(&mut self, root: SessionId, prices: &Prices) -> Total {
        self.query(root, true, &Filter::default(), prices)
    }

    #[inline(never)]
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
        for (session, log) in &self.logs {
            if !self.selected(*session, root, descendants) {
                continue;
            }
            for fact in &log.facts {
                if filter.agent.as_deref().is_some_and(|a| a != fact.agent)
                    || filter.side.is_some_and(|s| s != fact.side)
                    || filter.call.is_some_and(|c| c != fact.id.call)
                {
                    continue;
                }
                total.attempts.executed += 1;
                let usage = fact
                    .usage
                    .as_ref()
                    .and_then(|j| llm::Usage::decode_counts(j).ok())
                    .unwrap_or([None; 10]);
                for (sum, count) in total.usage.iter_mut().zip(usage) {
                    sum.add(count);
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
            let missing = log
                .uncertain
                .iter()
                .filter(|(id, _)| filter.call.is_none_or(|c| c == *id))
                .count() as u64;
            total.attempts.ambiguous += missing;
            for count in &mut total.usage {
                count.missing += missing;
            }
            total.cost.missing += missing;
            total.call_ns.missing += missing;
            total.critical_ns.missing += missing;
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
impl Extension for Metrics {
    const NAME: &'static str = "metrics";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Query(Mutex::new(Projection::default())));
    }
}

struct Query(Mutex<Projection>);

impl Query {
    #[inline(never)]
    fn finish(
        &self,
        session: SessionId,
        prices: Prices,
        logs: Vec<ext::LogPrefix>,
    ) -> Result<Total, String> {
        let mut projection = self.0.lock().map_err(|e| e.to_string())?;
        for log in logs {
            projection.fold(&log);
        }
        Ok(projection.query(session, true, &Filter::default(), &prices))
    }
}

impl Tool for Query {
    type In = Args;
    type Out = Total;
    const NAME: &'static str = "metrics";
    const DESCRIPTION: &'static str =
        "derive subtree usage and timing; optional prices [version,currency,[[model,rates],...]]";
    const AUDIENCE: protocol::Audience = protocol::Audience::User;
    const REPLAY: ext::Replay = ext::Replay::Safe;
    async fn call(&self, Args { args }: &Args, cx: &Cx) -> Result<Total, String> {
        let prices = Prices::parse(args)?;
        let cursors = self.0.lock().map_err(|e| e.to_string())?.cursors();
        let logs = cx
            .service::<dyn ext::Logs>()
            .ok_or("no log reader")?
            .read(cx.session, true, cursors)
            .await?;
        self.finish(cx.session, prices, logs)
    }
}

#[cfg(test)]
mod tests;
