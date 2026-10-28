//! Disposable decision traces shared with the native admission tests.

pub mod trace {
    use std::fmt;
    use std::str::FromStr;

    #[derive(Debug)]
    pub enum Failure {
        Invalid(String),
        Invariant(String),
    }

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Invalid(why) => write!(f, "invalid trace: {why}"),
                Self::Invariant(why) => write!(f, "invariant failed: {why}"),
            }
        }
    }

    pub type Checked<T> = std::result::Result<T, Failure>;

    pub fn require(condition: bool, why: &str) -> Checked<()> {
        if condition {
            Ok(())
        } else {
            Err(Failure::Invariant(why.into()))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Result {
        Success,
        Failure,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Decision {
        Input,
        Progress,
        Complete,
        Cancel,
        Batch,
        Flush { epoch: usize, result: Result },
        Crash { cut: usize },
        RecoveredComplete,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Trace(pub Vec<Decision>);

    impl Trace {
        pub fn validate(&self) -> std::result::Result<(), String> {
            if self.0.first() != Some(&Decision::Input) {
                return Err("trace must start with its input arrival".into());
            }
            let mut seen = [false; 7];
            let mut batches = 0;
            for step in &self.0 {
                let slot = match step {
                    Decision::Input => 0,
                    Decision::Progress if !seen[2] && !seen[6] => 1,
                    Decision::Complete if !seen[6] => 2,
                    Decision::Cancel if !seen[6] => 3,
                    Decision::Batch if !seen[6] => {
                        batches += 1;
                        continue;
                    }
                    Decision::Flush { epoch, result } if *epoch < 2 && batches >= 2 && !seen[6] => {
                        if *epoch == 1 && *result != Result::Success {
                            return Err("the later epoch must succeed in this workload".into());
                        }
                        4 + *epoch
                    }
                    Decision::Crash { .. } if seen[2] && seen[4] && seen[5] => 6,
                    Decision::RecoveredComplete if seen[6] => continue,
                    _ => return Err(format!("unbound decision: {step:?}")),
                };
                if std::mem::replace(&mut seen[slot], true) {
                    return Err(format!("duplicate decision: {step:?}"));
                }
            }
            if !seen[2] || !seen[4] || !seen[5] {
                return Err("script ended before all completions were bound".into());
            }
            Ok(())
        }
    }

    impl fmt::Display for Trace {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for (i, step) in self.0.iter().enumerate() {
                if i != 0 {
                    write!(f, " ")?;
                }
                match step {
                    Decision::Input => write!(f, "input")?,
                    Decision::Progress => write!(f, "progress")?,
                    Decision::Complete => write!(f, "complete")?,
                    Decision::Cancel => write!(f, "cancel")?,
                    Decision::Batch => write!(f, "batch")?,
                    Decision::Flush { epoch, result } => write!(
                        f,
                        "flush:{epoch}:{}",
                        match result {
                            Result::Success => "ok",
                            Result::Failure => "err",
                        }
                    )?,
                    Decision::Crash { cut } => write!(f, "crash:{cut}")?,
                    Decision::RecoveredComplete => write!(f, "recovered-complete")?,
                }
            }
            Ok(())
        }
    }

    impl FromStr for Trace {
        type Err = String;

        fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
            let steps = text.split_whitespace().map(|token| {
                Ok(match token {
                    "input" => Decision::Input,
                    "progress" => Decision::Progress,
                    "complete" => Decision::Complete,
                    "cancel" => Decision::Cancel,
                    "batch" => Decision::Batch,
                    "recovered-complete" => Decision::RecoveredComplete,
                    _ => {
                        let parts: Vec<_> = token.split(':').collect();
                        match parts.as_slice() {
                            ["crash", cut] => Decision::Crash {
                                cut: cut.parse().map_err(|_| format!("invalid cut: {token}"))?,
                            },
                            ["flush", epoch, result] => Decision::Flush {
                                epoch: epoch
                                    .parse()
                                    .map_err(|_| format!("invalid epoch: {token}"))?,
                                result: match *result {
                                    "ok" => Result::Success,
                                    "err" => Result::Failure,
                                    _ => return Err(format!("invalid result: {token}")),
                                },
                            },
                            _ => return Err(format!("unknown decision: {token}")),
                        }
                    }
                })
            });
            Ok(Self(steps.collect::<std::result::Result<_, _>>()?))
        }
    }

    /// Enumerates merges without permuting a source's own decisions.
    pub fn interleave(sources: &[Vec<Decision>]) -> Vec<Trace> {
        fn visit(
            sources: &[Vec<Decision>],
            at: &mut [usize],
            prefix: &mut Vec<Decision>,
            out: &mut Vec<Trace>,
        ) {
            if sources.iter().zip(at.iter()).all(|(s, i)| s.len() == *i) {
                out.push(Trace(prefix.clone()));
                return;
            }
            for source in 0..sources.len() {
                if let Some(step) = sources[source].get(at[source]) {
                    prefix.push(*step);
                    at[source] += 1;
                    visit(sources, at, prefix, out);
                    at[source] -= 1;
                    prefix.pop();
                }
            }
        }
        let mut out = Vec::new();
        visit(
            sources,
            &mut vec![0; sources.len()],
            &mut Vec::new(),
            &mut out,
        );
        out
    }

    /// Subsequence deletion preserves source order; smaller cuts retain less log.
    /// The caller rejects candidates whose causal prerequisites were removed.
    pub fn shrink(mut trace: Trace, fails: impl Fn(&Trace) -> bool) -> Trace {
        loop {
            let smaller = (0..trace.0.len()).find_map(|i| {
                let mut candidate = trace.clone();
                candidate.0.remove(i);
                if fails(&candidate) {
                    return Some(candidate);
                }
                if let Decision::Crash { cut } = trace.0[i] {
                    for earlier in 0..cut {
                        let mut candidate = trace.clone();
                        candidate.0[i] = Decision::Crash { cut: earlier };
                        if fails(&candidate) {
                            return Some(candidate);
                        }
                    }
                }
                None
            });
            let Some(smaller) = smaller else { return trace };
            trace = smaller;
        }
    }

    pub fn held_epochs(result: Result) -> Vec<Trace> {
        interleave(&[
            vec![Decision::Cancel],
            vec![Decision::Complete],
            vec![
                Decision::Flush {
                    epoch: 1,
                    result: Result::Success,
                },
                Decision::Flush { epoch: 0, result },
            ],
        ])
        .into_iter()
        .flat_map(|tail| {
            (0..16).map(move |batches| {
                let mut steps = vec![
                    Decision::Input,
                    Decision::Batch,
                    Decision::Progress,
                    Decision::Batch,
                ];
                for (i, step) in tail.0.iter().enumerate() {
                    steps.push(*step);
                    if batches & (1 << i) != 0 {
                        steps.push(Decision::Batch);
                    }
                }
                Trace(steps)
            })
        })
        .collect()
    }
}
