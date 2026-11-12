//! Process-wide counters behind `kintrel daemon metrics` (SPEC §15), rendered as Prometheus
//! text. Plain atomics: the daemon is one process, and every crate counts into the same place.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Counter(AtomicU64::new(0))
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Counter::new()
    }
}

/// Upper bounds of the post latency buckets, in microseconds.
pub const POST_BUCKETS_US: [u64; 8] = [250, 500, 1_000, 2_000, 5_000, 10_000, 50_000, 100_000];
pub static POST_BUCKET: [Counter; 8] = [const { Counter::new() }; 8];
pub static POST_SUM_US: Counter = Counter::new();
pub static POSTS: Counter = Counter::new();
pub static POSTS_BUSY: Counter = Counter::new();

/// Merges by tier (`merge::Tier` as an index), and merges that conflicted. Every combiner pass
/// refolds the unpromoted notes, so these count merges computed, not distinct notes.
pub const TIERS: [&str; 5] = ["trivial", "lines", "symbols", "whole_file", "mergiraf"];
pub static MERGES: [Counter; 5] = [const { Counter::new() }; 5];
pub static MERGE_CONFLICTS: Counter = Counter::new();
/// Tier 0: ops written against exactly what head has, applied without merging.
pub static FAST_APPLIES: Counter = Counter::new();
pub static COMBINER_PASS_MS: Counter = Counter::new();
pub static PROMOTIONS: Counter = Counter::new();

pub static NOTICES_SENT: Counter = Counter::new();
pub static NOTICES_COALESCED: Counter = Counter::new();
pub static NOTICES_OVER_RATE: Counter = Counter::new();

pub static CHECKS_RUN: Counter = Counter::new();
pub static CHECKS_CACHED: Counter = Counter::new();
pub static CHECKS_FAILED: Counter = Counter::new();
pub static CHECK_MS: Counter = Counter::new();
pub static GATE_PASSES: Counter = Counter::new();
pub static GATE_FAILS: Counter = Counter::new();

/// Records one post that took `us` microseconds.
pub fn observe_post(us: u64) {
    POSTS.add(1);
    POST_SUM_US.add(us);
    for (bound, bucket) in POST_BUCKETS_US.iter().zip(&POST_BUCKET) {
        if us <= *bound {
            bucket.add(1);
        }
    }
}

/// Every counter, plus `gauges` (name, help, value), as Prometheus text.
pub fn render(gauges: &[(&str, &str, u64)]) -> String {
    let mut out = String::new();
    let mut one = |name: &str, kind: &str, help: &str, samples: &[(String, u64)]| {
        let _ = writeln!(out, "# HELP kintrel_{name} {help}\n# TYPE kintrel_{name} {kind}");
        for (labels, v) in samples {
            let _ = writeln!(out, "kintrel_{name}{labels} {v}");
        }
    };
    let plain = |v: u64| vec![(String::new(), v)];
    let mut buckets: Vec<(String, u64)> =
        POST_BUCKETS_US.iter().zip(&POST_BUCKET).map(|(b, c)| (format!("_bucket{{le=\"{}\"}}", *b as f64 / 1e6), c.get())).collect();
    buckets.push(("_bucket{le=\"+Inf\"}".into(), POSTS.get()));
    buckets.push(("_sum".into(), POST_SUM_US.get() / 1_000_000));
    buckets.push(("_count".into(), POSTS.get()));
    one("post_seconds", "histogram", "Post latency (seconds; the sum is whole seconds).", &buckets);
    one("posts_busy_total", "counter", "Posts refused as busy (rate limit or a full log queue).", &plain(POSTS_BUSY.get()));
    let mut tiers: Vec<(String, u64)> = vec![("{tier=\"fast\"}".into(), FAST_APPLIES.get())];
    tiers.extend(TIERS.iter().zip(&MERGES).map(|(t, c)| (format!("{{tier=\"{t}\"}}"), c.get())));
    one("merges_total", "counter", "Ops applied, by tier (fast: tier 0, no merge needed).", &tiers);
    one("merge_conflicts_total", "counter", "Merges that conflicted.", &plain(MERGE_CONFLICTS.get()));
    one("combiner_pass_ms_total", "counter", "Time spent in combiner passes.", &plain(COMBINER_PASS_MS.get()));
    one("promotions_total", "counter", "Promotions of head.", &plain(PROMOTIONS.get()));
    one("notices_sent_total", "counter", "Notices delivered to inboxes.", &plain(NOTICES_SENT.get()));
    one("notices_coalesced_total", "counter", "Notices merged into an unread one about the same atom.", &plain(NOTICES_COALESCED.get()));
    one("notices_over_rate_total", "counter", "Notices summarized because an inbox hit its rate limit.", &plain(NOTICES_OVER_RATE.get()));
    one("checks_total", "counter", "Check runs, cached or not.", &plain(CHECKS_RUN.get()));
    one("checks_cached_total", "counter", "Check runs answered from the cache.", &plain(CHECKS_CACHED.get()));
    one("checks_failed_total", "counter", "Check runs that failed.", &plain(CHECKS_FAILED.get()));
    one("check_ms_total", "counter", "Time spent running checks (not cached).", &plain(CHECK_MS.get()));
    one("gate_passes_total", "counter", "Gate decisions that passed.", &plain(GATE_PASSES.get()));
    one("gate_fails_total", "counter", "Gate decisions that failed.", &plain(GATE_FAILS.get()));
    for (name, help, v) in gauges {
        one(name, "gauge", help, &plain(*v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        observe_post(700);
        let text = render(&[("sessions", "Live sessions.", 3)]);
        assert!(text.contains("# TYPE kintrel_post_seconds histogram"));
        assert!(text.contains("kintrel_post_seconds_bucket{le=\"0.001\"} "), "{text}");
        assert!(text.contains("kintrel_merges_total{tier=\"symbols\"} "));
        assert!(text.contains("# TYPE kintrel_sessions gauge\nkintrel_sessions 3\n"));
        // A 700 µs post is in the 1 ms bucket and above, not the 500 µs one.
        let le = |b: &str| text.lines().find(|l| l.starts_with(&format!("kintrel_post_seconds_bucket{{le=\"{b}\"}}"))).unwrap().to_owned();
        assert!(le("0.0005").ends_with(" 0") || POSTS.get() > 1, "{}", le("0.0005"));
    }
}
