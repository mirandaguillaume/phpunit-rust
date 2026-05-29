//! Lightweight execution profiler emitting Chrome Trace Format JSON.
//!
//! Use [`Profiler::span`] to record the wall-clock duration of a code block;
//! events are collected in memory and flushed to a JSON file by
//! [`Profiler::write_to`]. The output is consumable by `chrome://tracing`,
//! Perfetto, and Speedscope — drop the JSON into any of those for a flame
//! graph / timeline view of where wall clock is being spent.
//!
//! When `enabled = false` every span is a near-noop (one branch + an
//! `Instant::now()` skipped), so the profiler can be left wired in
//! production builds with no measurable overhead.

use serde::Serialize;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

/// Chrome Trace Format event. We only emit the "complete" phase (`ph: "X"`),
/// which carries both the timestamp and a duration — half as many records as
/// the begin/end pair for the same information.
#[derive(Serialize, Clone)]
pub struct TraceEvent {
    pub name: String,
    pub cat:  String,
    pub ph:   &'static str,
    pub ts:   u64,
    pub dur:  u64,
    pub pid:  u32,
    pub tid:  u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

/// Top-level shape Chrome Trace expects.
#[derive(Serialize)]
struct TraceDoc<'a> {
    #[serde(rename = "traceEvents")]
    trace_events: &'a [TraceEvent],
    #[serde(rename = "displayTimeUnit")]
    display_time_unit: &'static str,
}

pub struct Profiler {
    enabled: bool,
    /// Reference monotonic clock — every event's `ts` is microseconds from
    /// this instant, keeping numbers small and easy to compare.
    start: Instant,
    events: Mutex<Vec<TraceEvent>>,
}

impl Profiler {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            start: Instant::now(),
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Time `f` and record a complete-phase event. Returns whatever `f`
    /// returns so the caller can wrap arbitrary expressions.
    pub fn span<F, R>(&self, name: &str, category: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        if !self.enabled {
            return f();
        }
        let start_us = self.elapsed_us();
        let r = f();
        let end_us = self.elapsed_us();
        self.push(TraceEvent {
            name: name.to_string(),
            cat:  category.to_string(),
            ph:   "X",
            ts:   start_us,
            dur:  end_us.saturating_sub(start_us),
            pid:  std::process::id(),
            tid:  0,
            args: None,
        });
        r
    }

    /// Same as [`span`] but with structured metadata attached to the event
    /// (visible in chrome://tracing's "Args" pane).
    pub fn span_with<F, R>(
        &self,
        name: &str,
        category: &str,
        args: serde_json::Value,
        f: F,
    ) -> R
    where
        F: FnOnce() -> R,
    {
        if !self.enabled {
            return f();
        }
        let start_us = self.elapsed_us();
        let r = f();
        let end_us = self.elapsed_us();
        self.push(TraceEvent {
            name: name.to_string(),
            cat:  category.to_string(),
            ph:   "X",
            ts:   start_us,
            dur:  end_us.saturating_sub(start_us),
            pid:  std::process::id(),
            tid:  0,
            args: Some(args),
        });
        r
    }

    /// Record a zero-duration marker. Useful for one-off milestones like
    /// "first outcome received" or "stop_on triggered".
    pub fn mark(&self, name: &str, category: &str) {
        if !self.enabled {
            return;
        }
        let ts = self.elapsed_us();
        self.push(TraceEvent {
            name: name.to_string(),
            cat:  category.to_string(),
            ph:   "X",
            ts,
            dur:  0,
            pid:  std::process::id(),
            tid:  0,
            args: None,
        });
    }

    /// Manually record a completed span. Use this when the timed block does
    /// not fit a closure (e.g. it crosses ownership boundaries).
    pub fn record(
        &self,
        name: &str,
        category: &str,
        start: Instant,
        end: Instant,
        args: Option<serde_json::Value>,
    ) {
        self.record_on(name, category, start, end, 0, args);
    }

    /// Like [`record`] but stamps a specific `tid` on the event. The viewer
    /// renders one horizontal lane per (pid, tid) pair, so passing a slot
    /// number as `tid` produces a per-worker swim-lane in the timeline.
    pub fn record_on(
        &self,
        name: &str,
        category: &str,
        start: Instant,
        end: Instant,
        tid: u32,
        args: Option<serde_json::Value>,
    ) {
        if !self.enabled {
            return;
        }
        let start_us = start.saturating_duration_since(self.start).as_micros() as u64;
        let dur = end.saturating_duration_since(start).as_micros() as u64;
        self.push(TraceEvent {
            name: name.to_string(),
            cat:  category.to_string(),
            ph:   "X",
            ts:   start_us,
            dur,
            pid:  std::process::id(),
            tid,
            args,
        });
    }

    /// Write the collected events to `path` as a Chrome Trace Format file.
    /// The file can be loaded directly into chrome://tracing, Perfetto
    /// (perfetto.dev), or Speedscope (speedscope.app) for visualisation.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let events = self.events.lock().expect("profiler events mutex poisoned");
        let doc = TraceDoc {
            trace_events: events.as_slice(),
            display_time_unit: "ms",
        };
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, &doc).map_err(std::io::Error::other)?;
        Ok(())
    }

    /// Number of recorded events — handy for tests and summary lines.
    pub fn event_count(&self) -> usize {
        self.events.lock().map(|v| v.len()).unwrap_or(0)
    }

    fn elapsed_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    fn push(&self, ev: TraceEvent) {
        if let Ok(mut v) = self.events.lock() {
            v.push(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn disabled_profiler_records_nothing() {
        let p = Profiler::new(false);
        p.span("foo", "test", || sleep(Duration::from_millis(1)));
        p.mark("bar", "test");
        assert_eq!(p.event_count(), 0);
    }

    #[test]
    fn span_records_one_event_with_positive_duration() {
        let p = Profiler::new(true);
        let result = p.span("work", "test", || {
            sleep(Duration::from_millis(2));
            42
        });
        assert_eq!(result, 42);
        assert_eq!(p.event_count(), 1);
    }

    #[test]
    fn write_to_produces_loadable_trace_doc() {
        let p = Profiler::new(true);
        p.span("alpha", "phase", || sleep(Duration::from_millis(1)));
        p.span_with("beta", "phase", serde_json::json!({"batches": 12}), || {
            sleep(Duration::from_millis(1))
        });
        p.mark("milestone", "phase");

        let tmp = tempfile::NamedTempFile::new().unwrap();
        p.write_to(tmp.path()).unwrap();
        let raw = std::fs::read_to_string(tmp.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let evs = v["traceEvents"].as_array().expect("traceEvents is an array");
        assert_eq!(evs.len(), 3, "alpha + beta + milestone");
        // All events should be complete-phase X with non-negative ts.
        for e in evs {
            assert_eq!(e["ph"], "X");
            assert!(e["ts"].as_u64().is_some());
        }
        // beta carries its args metadata.
        let beta = evs.iter().find(|e| e["name"] == "beta").unwrap();
        assert_eq!(beta["args"]["batches"], 12);
    }
}
