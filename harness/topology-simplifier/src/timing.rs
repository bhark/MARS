//! per-stage wallclock timings + peak RSS sampling.
//!
//! the gate report needs per-million-feature CPU cost per stage so the
//! integration follow-up has concrete numbers for "what does it cost to do
//! this in the compiler?". peak RSS comes from /proc/self/status VmHWM
//! (linux). non-linux falls back to None and the report just prints n/a —
//! the spike is run on linux operator boxes.

use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone)]
pub struct StageTimings {
    pub stages: Vec<(String, Duration)>,
}

impl StageTimings {
    pub fn record<T>(&mut self, name: &str, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let v = f();
        self.stages.push((name.to_string(), t.elapsed()));
        v
    }

    pub fn total(&self) -> Duration {
        self.stages.iter().map(|(_, d)| *d).sum()
    }
}

/// peak RSS in KiB since process start. reads /proc/self/status VmHWM on
/// linux; returns None elsewhere or on read failure.
#[cfg(target_os = "linux")]
pub fn peak_rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let v: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(v);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn peak_rss_kib() -> Option<u64> {
    None
}

/// pretty-print a per-million-feature normalised timing line for one stage.
pub fn fmt_stage_normalised(name: &str, dur: Duration, feature_count: u64) -> String {
    if feature_count == 0 {
        return format!(
            "  {:<24} {:>10.3} ms (n/a per-Mfeat: feature_count=0)",
            name,
            dur_to_ms(dur)
        );
    }
    let per_million = dur_to_ms(dur) * 1_000_000.0 / (feature_count as f64);
    format!(
        "  {:<24} {:>10.3} ms (= {:>8.3} ms/Mfeat)",
        name,
        dur_to_ms(dur),
        per_million
    )
}

#[inline]
pub fn dur_to_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn record_accumulates_total() {
        let mut t = StageTimings::default();
        t.record("a", || std::thread::sleep(Duration::from_millis(2)));
        t.record("b", || std::thread::sleep(Duration::from_millis(3)));
        assert_eq!(t.stages.len(), 2);
        assert!(t.total() >= Duration::from_millis(4));
    }

    #[test]
    fn fmt_handles_zero_features() {
        let line = fmt_stage_normalised("ingest", Duration::from_millis(10), 0);
        assert!(line.contains("n/a"));
    }

    #[test]
    fn fmt_per_million_features() {
        // 1 ms over 1000 features => 1000 ms/Mfeat.
        let line = fmt_stage_normalised("graph", Duration::from_millis(1), 1000);
        assert!(line.contains("ms/Mfeat"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peak_rss_reports_some() {
        let r = peak_rss_kib();
        assert!(r.is_some());
        assert!(r.unwrap() > 0);
    }
}
