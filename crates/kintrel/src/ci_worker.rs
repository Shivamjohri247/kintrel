//! The daemon's CI worker (SPEC §9.2, §10.4): one thread that runs, in order, the lint tier on
//! the latest head (fast feedback), the gate checks on the latest promotion candidate, then the
//! typecheck and test tiers on the latest head. Gate and feedback runs share cache keys when they
//! reach the same root, so a check runs once.
//!
//! The combiner asks [`Gate::check`] without blocking: a candidate whose gate checks are all
//! cached is decided on the spot, otherwise it is queued and promoted later through
//! [`Gate::passed`], even if head has moved on by then.
//!
//! Check definitions come from the **promoted** root's `kintrel.toml`, never from the candidate
//! or head: otherwise an agent could weaken the gate, or have the daemon run a command, just by
//! editing the file. A candidate that changes `kintrel.toml` fails the gate until a person
//! approves that exact file (`kintrel promote --approve-config`); then it is checked with it.
//!
//! A failing candidate is bisected (SPEC §9.3): prefixes of its notes, in fold order, are folded
//! here and checked as snapshots (mostly cache hits), and the note that first breaks the gate is
//! rejected. Promoted roots get their deploy hooks (SPEC §9.4); every [`BACKSTOP_EVERY`]th
//! promotion also runs the full suite, and a failure there opens an incident: the gate widens to
//! every check until a candidate passes them all (SPEC §9.1).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use kintrel_ci::{Ci, Outcome, Snapshot, Tier};
use kintrel_core::Hash;
use kintrel_core::combine::{Gate, GateState};

type Callback<T> = OnceLock<Box<dyn Fn(T) + Send + Sync>>;

pub struct CiWorker {
    repo: PathBuf,
    ci: Arc<Ci>,
    want: Mutex<Want>,
    cv: Condvar,
    /// Wakes the combiner once a candidate's checks finish.
    pub wake_combiner: Callback<()>,
    /// Receives every failing outcome, for notices.
    pub report: Callback<Vec<Outcome>>,
    /// A rejected note: (its session, the note, the gate checks it breaks).
    pub report_rejected: Callback<(u64, Hash, Vec<Outcome>)>,
    /// The backstop run failed on a promoted root.
    pub report_incident: Callback<Vec<Outcome>>,
}

#[derive(Default)]
struct Want {
    gate: Option<(Hash, u64)>,
    feedback: Option<Hash>,
    /// Decisions by (candidate root, config used).
    decided: HashMap<(Hash, Option<Hash>), GateState>,
    passed: Option<(Hash, u64)>,
    /// Config changes a person approved, as (promoted config, new config): an approval lapses
    /// as soon as the promoted config is something else.
    approved: std::collections::HashSet<(Option<Hash>, Option<Hash>)>,
    /// Notes bisection found guilty, for [`Gate::rejected`].
    rejected: Vec<Hash>,
    /// Promoted roots waiting for deploy hooks and the backstop count.
    promoted: Vec<Hash>,
    promotions: u64,
    /// The backstop failed: the gate is every check until a candidate passes them all.
    incident: bool,
}

/// Every Nth promotion runs the full suite on the promoted root (SPEC §9.1).
pub const BACKSTOP_EVERY: u64 = 20;

/// The gate's answer for a candidate whose `kintrel.toml` differs from the promoted one.
pub const UNAPPROVED_CONFIG: &str = "kintrel.toml changed: a person must approve it (`kintrel promote --approve-config`)";

/// Decisions remembered per root; roots are content hashes, so they never go stale.
const DECIDED: usize = 1024;

impl CiWorker {
    pub fn start(repo: PathBuf, ci: Arc<Ci>) -> std::io::Result<Arc<CiWorker>> {
        let w = Arc::new(CiWorker {
            repo,
            ci,
            want: Mutex::new(Want::default()),
            cv: Condvar::new(),
            wake_combiner: OnceLock::new(),
            report: OnceLock::new(),
            report_rejected: OnceLock::new(),
            report_incident: OnceLock::new(),
        });
        let worker = w.clone();
        std::thread::Builder::new().name("kintrel-ci".into()).spawn(move || worker.run())?;
        Ok(w)
    }

    /// Check runs queued or running (a gate candidate, head feedback) and deploys waiting.
    pub fn pending(&self) -> u64 {
        let w = self.want.lock().unwrap();
        w.gate.is_some() as u64 + w.feedback.is_some() as u64 + w.promoted.len() as u64
    }

    /// Approves changing the config from `trusted` (the promoted one) to `theirs`.
    pub fn approve_config(&self, trusted: Option<Hash>, theirs: Option<Hash>) {
        let mut w = self.want.lock().unwrap();
        w.approved.insert((trusted, theirs));
        w.decided.clear();
    }

    /// Asks for feedback on `root` (the latest head); only the latest request is kept.
    pub fn feedback(&self, root: Hash) {
        self.want.lock().unwrap().feedback = Some(root);
        self.cv.notify_one();
    }

    fn run(&self) {
        loop {
            let (gate, feedback, promoted) = {
                let mut w = self.want.lock().unwrap();
                while w.gate.is_none() && w.feedback.is_none() && w.promoted.is_empty() {
                    w = self.cv.wait(w).unwrap();
                }
                (w.gate.take(), w.feedback.take(), std::mem::take(&mut w.promoted))
            };
            for root in promoted {
                self.after_promotion(root);
            }
            if let Some(root) = feedback {
                self.tiers(root, &|c| c.tier == Tier::Lint);
            }
            if let Some((root, gseq)) = gate {
                let incident = self.want.lock().unwrap().incident;
                let (decision, config) = match self.policy(root) {
                    Ok(Ok(config)) => match self.checks(root, config, &|c| c.gate || incident) {
                        Ok(out) => (decide(&out), config),
                        Err(e) => (GateState::Fail(vec![format!("kintrel.toml: {e}")]), config),
                    },
                    Ok(Err(fail)) => (fail, None),
                    Err(e) => (GateState::Fail(vec![e]), None),
                };
                let culprit = match &decision {
                    GateState::Fail(checks) if !checks.iter().any(|c| c == UNAPPROVED_CONFIG) => self.bisect(gseq, config, incident),
                    _ => None,
                };
                let mut w = self.want.lock().unwrap();
                if decision == GateState::Pass {
                    w.passed = Some((root, gseq));
                    if w.incident {
                        w.incident = false; // a candidate passed the full suite
                        w.decided.clear();
                    }
                }
                remember(&mut w.decided, (root, config), decision);
                if let Some((_, note, _)) = &culprit {
                    w.rejected.push(*note);
                }
                drop(w);
                if let (Some(c), Some(report)) = (culprit, self.report_rejected.get()) {
                    report(c);
                }
                if let Some(wake) = self.wake_combiner.get() {
                    wake(());
                }
            }
            if let Some(root) = feedback {
                self.tiers(root, &|c| c.tier != Tier::Lint);
            }
        }
    }

    /// The note that first breaks the gate among the notes up to `gseq`, found by bisecting
    /// prefixes in fold order; `None` if the promoted root fails on its own (no culprit).
    fn bisect(&self, gseq: u64, config: Option<Hash>, incident: bool) -> Option<(u64, Hash, Vec<Outcome>)> {
        let run = || -> Result<Option<(u64, Hash, Vec<Outcome>)>, String> {
            let st = self.reader()?;
            let e = |e: kintrel_core::Error| e.to_string();
            let promoted = st.get_ref(kintrel_core::state::PROMOTED).map_err(e)?.ok_or("no promoted ref")?;
            let mark = st.get_mark(kintrel_core::state::PROMOTED_GSEQ).map_err(e)?;
            let rejected = st.rejected_notes().map_err(e)?;
            let notes: Vec<(Hash, kintrel_core::note::Note)> = st
                .log_notes_after(mark)
                .map_err(e)?
                .into_iter()
                .filter(|(g, id, _)| *g <= gseq && !rejected.contains(id))
                .map(|(_, id, n)| (id, n))
                .collect();
            let ordered: Vec<(Hash, kintrel_core::note::Note)> =
                kintrel_core::combine::fold_order(&notes).into_iter().map(|i| notes[i].clone()).collect();
            // The gate's failing outcomes on promoted + the first k notes, if any.
            let fails = |k: usize| -> Result<Option<Vec<Outcome>>, String> {
                let src = kintrel_core::combine::Promoted { st: &st, root: promoted };
                let folded = kintrel_core::combine::fold(&src, &ordered[..k]).map_err(e)?;
                let snap = Snapshot { root: promoted, overlay: folded.files, blobs: folded.blobs.into_iter().collect() };
                let out = self.ci.run_where(&st, &self.repo, config, &snap, &|c| c.gate || incident).map_err(|e| e.to_string())?;
                let failing: Vec<Outcome> = out.into_iter().filter(|o| !o.ok).collect();
                Ok((!failing.is_empty()).then_some(failing))
            };
            if ordered.is_empty() || fails(0)?.is_some() {
                return Ok(None);
            }
            let (mut lo, mut hi) = (0, ordered.len()); // fails(lo) is clean, fails(hi) is not
            let mut failing = match fails(hi)? {
                Some(f) => f,
                None => return Ok(None), // flaky: the whole candidate passes now
            };
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                match fails(mid)? {
                    Some(f) => (hi, failing) = (mid, f),
                    None => lo = mid,
                }
            }
            let (note, n) = &ordered[hi - 1];
            Ok(Some((n.session, *note, failing)))
        };
        run().unwrap_or_else(|e| {
            tracing::error!("bisection failed: {e}");
            None
        })
    }

    /// Deploy hooks, then every [`BACKSTOP_EVERY`]th promotion the full suite, on `root`. The
    /// root is promoted, so its own `kintrel.toml` is the trusted config now.
    fn after_promotion(&self, root: Hash) {
        let run = || -> Result<(), String> {
            let st = self.reader()?;
            let config = kintrel_ci::config_of(&st, root).map_err(|e| e.to_string())?;
            let deployed = self.ci.deploy(&st, &self.repo, config, root).map_err(|e| e.to_string())?;
            let failed: Vec<Outcome> = deployed.into_iter().filter(|o| !o.ok).collect();
            if !failed.is_empty()
                && let Some(report) = self.report.get()
            {
                tracing::warn!("deploy failed on {}", root.short());
                report(failed);
            }
            let due = {
                let mut w = self.want.lock().unwrap();
                w.promotions += 1;
                w.promotions.is_multiple_of(BACKSTOP_EVERY)
            };
            if due {
                let out = self.checks(root, config, &|_| true)?;
                let failed: Vec<Outcome> = out.into_iter().filter(|o| !o.ok).collect();
                if !failed.is_empty() {
                    let mut w = self.want.lock().unwrap();
                    w.incident = true;
                    w.decided.clear();
                    drop(w);
                    if let Some(report) = self.report_incident.get() {
                        report(failed);
                    }
                }
            }
            Ok(())
        };
        if let Err(e) = run() {
            tracing::error!("after promotion: {e}");
        }
    }

    /// Feedback uses the promoted config only.
    fn tiers(&self, root: Hash, keep: &dyn Fn(&kintrel_ci::Check) -> bool) {
        let r = self.reader().and_then(|st| promoted_config(&st)).and_then(|config| self.checks(root, config, keep));
        if let Err(e) = r {
            tracing::error!("{e}");
        }
    }

    fn reader(&self) -> Result<kintrel_core::state::State, String> {
        kintrel_core::state::State::open_board(&self.repo).map_err(|e| e.to_string())
    }

    /// The config to judge `root` with: the promoted one, or the candidate's own if a person
    /// approved it. A changed, unapproved config is an immediate `Fail`.
    fn policy(&self, root: Hash) -> Result<Result<Option<Hash>, GateState>, String> {
        let st = self.reader()?;
        let trusted = promoted_config(&st)?;
        let theirs = kintrel_ci::config_of(&st, root).map_err(|e| e.to_string())?;
        if theirs == trusted || self.want.lock().unwrap().approved.contains(&(trusted, theirs)) {
            Ok(Ok(theirs))
        } else {
            Ok(Err(GateState::Fail(vec![UNAPPROVED_CONFIG.into()])))
        }
    }

    /// Runs the checks `keep` selects on `root`, reporting failures.
    fn checks(&self, root: Hash, config: Option<Hash>, keep: &dyn Fn(&kintrel_ci::Check) -> bool) -> Result<Vec<Outcome>, String> {
        let snap = Snapshot { root, ..Default::default() };
        let out = self.ci.run_where(&self.reader()?, &self.repo, config, &snap, keep).map_err(|e| e.to_string())?;
        self.report_failed(&out);
        Ok(out)
    }

    /// Like `checks`, but only if every one is cached: `None` otherwise, and nothing runs.
    fn cached_checks(&self, root: Hash, config: Option<Hash>, keep: &dyn Fn(&kintrel_ci::Check) -> bool) -> Result<Option<Vec<Outcome>>, String> {
        let snap = Snapshot { root, ..Default::default() };
        let out = self.ci.cached_where(&self.reader()?, &self.repo, config, &snap, keep).map_err(|e| e.to_string())?;
        if let Some(out) = &out {
            self.report_failed(out);
        }
        Ok(out)
    }

    fn report_failed(&self, out: &[Outcome]) {
        let failed: Vec<Outcome> = out.iter().filter(|o| !o.ok).cloned().collect();
        if let (false, Some(report)) = (failed.is_empty(), self.report.get()) {
            report(failed);
        }
    }
}

fn decide(outcomes: &[Outcome]) -> GateState {
    let mut failing: Vec<String> = outcomes.iter().filter(|o| !o.ok).map(|o| o.check.clone()).collect();
    let counter = if failing.is_empty() { &kintrel_core::metrics::GATE_PASSES } else { &kintrel_core::metrics::GATE_FAILS };
    counter.add(1);
    failing.sort();
    failing.dedup();
    if failing.is_empty() { GateState::Pass } else { GateState::Fail(failing) }
}

fn promoted_config(st: &kintrel_core::state::State) -> Result<Option<Hash>, String> {
    let promoted = st.get_ref(kintrel_core::state::PROMOTED).map_err(|e| e.to_string())?.ok_or("no promoted ref")?;
    kintrel_ci::config_of(st, promoted).map_err(|e| e.to_string())
}

fn remember(decided: &mut HashMap<(Hash, Option<Hash>), GateState>, root: (Hash, Option<Hash>), d: GateState) {
    if decided.len() >= DECIDED {
        decided.clear();
    }
    decided.insert(root, d);
}

impl Gate for CiWorker {
    fn check(&self, root: Hash, gseq: u64) -> GateState {
        let config = match self.policy(root) {
            Ok(Ok(config)) => config,
            Ok(Err(fail)) => return fail,
            Err(e) => return GateState::Fail(vec![e]),
        };
        if let Some(d) = self.want.lock().unwrap().decided.get(&(root, config)) {
            return d.clone();
        }
        let incident = self.want.lock().unwrap().incident;
        let decision = match self.cached_checks(root, config, &|c| c.gate || incident) {
            Ok(Some(out)) => decide(&out),
            Ok(None) => {
                self.want.lock().unwrap().gate = Some((root, gseq));
                self.cv.notify_one();
                return GateState::Pending;
            }
            Err(e) => GateState::Fail(vec![format!("kintrel.toml: {e}")]),
        };
        remember(&mut self.want.lock().unwrap().decided, (root, config), decision.clone());
        decision
    }

    fn passed(&self) -> Option<(Hash, u64)> {
        self.want.lock().unwrap().passed.take()
    }

    fn rejected(&self) -> Vec<Hash> {
        std::mem::take(&mut self.want.lock().unwrap().rejected)
    }

    fn promoted(&self, root: Hash) {
        self.want.lock().unwrap().promoted.push(root);
        self.cv.notify_one();
    }
}
