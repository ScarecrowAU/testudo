//! UI performance tracing: slow frames and scroll/drag bursts.
//!
//! gpui has no hook for "frame finished", so two measures are used:
//! - build: time spent in our `render` (element construction, plus `tick`:
//!   playback, frame decoding);
//! - interval: time between consecutive renders. During a burst of input
//!   (wheel or drag events arriving back to back) that is the whole frame:
//!   build, layout, paint and present.
//!
//! Spans go to the OTLP exporter when one is configured (`slow_frame`,
//! `ui_scroll`); scroll bursts are also logged, so `target/dev/testudo.log`
//! shows them without a collector.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::RenderImage;
use opentelemetry::trace::{Span as _, Tracer as _};
use opentelemetry::{KeyValue, global};

/// A build slower than this is reported as a slow frame (one 60 Hz frame).
const SLOW_BUILD: Duration = Duration::from_millis(16);
/// During a burst, an interval slower than this is a dropped frame (30 fps).
const SLOW_INTERVAL: Duration = Duration::from_millis(33);
/// A burst ends after this long without input.
const BURST_QUIET: Duration = Duration::from_millis(300);
/// Long bursts (playback, long scrolls) are reported in chunks of this length.
const BURST_CHUNK: Duration = Duration::from_secs(5);

thread_local! {
    static DECODES: Cell<(u32, Duration)> = const { Cell::new((0, Duration::ZERO)) };
}

/// Decode a JPEG on the UI thread, counting the time towards this frame.
pub fn ui_decode(jpeg: &[u8]) -> Option<Arc<RenderImage>> {
    let started = Instant::now();
    let out = crate::bridge::decode_jpeg(jpeg);
    DECODES.with(|d| {
        let (n, t) = d.get();
        d.set((n + 1, t + started.elapsed()));
    });
    out
}

/// A frame decoded off the UI thread reached the screen.
pub fn count_decode() {
    DECODES.with(|d| {
        let (n, t) = d.get();
        d.set((n + 1, t));
    });
}

fn take_decodes() -> (u32, Duration) {
    DECODES.with(|d| d.replace((0, Duration::ZERO)))
}

/// What was on screen, attached to slow-frame spans.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameCtx {
    pub history: bool,
    pub focus: bool,
    pub pages: usize,
    pub test_rows: usize,
    pub compare_lanes: usize,
}

impl FrameCtx {
    fn attrs(&self) -> Vec<KeyValue> {
        vec![
            KeyValue::new("ui.mode", if self.history { "history" } else { "live" }),
            KeyValue::new("ui.focus", self.focus),
            KeyValue::new("ui.pages", self.pages as i64),
            KeyValue::new("ui.test_rows", self.test_rows as i64),
            KeyValue::new("ui.compare_lanes", self.compare_lanes as i64),
        ]
    }
}

struct Burst {
    area: &'static str,
    started: SystemTime,
    last_input: Instant,
    events: u32,
    frames: u32,
    slowest_interval: Duration,
    total_interval: Duration,
    slowest_build: Duration,
    total_build: Duration,
    /// Frames decoded on the UI thread during the burst.
    decodes: u32,
    ctx: FrameCtx,
}

#[derive(Default)]
pub struct Perf {
    burst: Option<Burst>,
    last_frame: Option<(Instant, SystemTime)>,
}

impl Perf {
    /// A wheel or drag event in `area` (e.g. "run_list", "focus_timeline").
    pub fn input(&mut self, area: &'static str) {
        let now = Instant::now();
        match &mut self.burst {
            Some(b) if b.area == area => {
                b.events += 1;
                b.last_input = now;
            }
            _ => {
                self.finish_burst();
                self.burst = Some(Burst {
                    area,
                    started: SystemTime::now(),
                    last_input: now,
                    events: 1,
                    frames: 0,
                    slowest_interval: Duration::ZERO,
                    total_interval: Duration::ZERO,
                    slowest_build: Duration::ZERO,
                    total_build: Duration::ZERO,
                    decodes: 0,
                    ctx: FrameCtx::default(),
                });
            }
        }
    }

    /// Call at the start of `render`.
    pub fn frame_start(&self) -> (Instant, SystemTime) {
        (Instant::now(), SystemTime::now())
    }

    /// Call at the end of `render`. Returns true while a burst is open, so the
    /// caller keeps frames coming until it can be closed.
    pub fn frame_end(&mut self, start: (Instant, SystemTime), ctx: FrameCtx) -> bool {
        let build = start.0.elapsed();
        let (decodes, decode_time) = take_decodes();
        let interval = self.last_frame.map(|(at, _)| start.0.duration_since(at));
        let prev_start = self.last_frame.map(|(_, wall)| wall);
        self.last_frame = Some(start);

        let in_burst = self.burst.as_ref().is_some_and(|b| b.last_input.elapsed() < BURST_QUIET);
        if build >= SLOW_BUILD {
            let mut attrs = ctx.attrs();
            attrs.push(KeyValue::new("frame.measure", "build"));
            attrs.push(KeyValue::new("frame.decodes", decodes as i64));
            attrs.push(KeyValue::new("frame.decode_ms", ms(decode_time)));
            record("slow_frame", start.1, start.1 + build, attrs);
        } else if in_burst
            && let (Some(iv), Some(prev)) = (interval, prev_start)
            && iv >= SLOW_INTERVAL
        {
            // Previous frame start to this one: build + layout + paint.
            let mut attrs = ctx.attrs();
            attrs.push(KeyValue::new("frame.measure", "interval"));
            attrs.push(KeyValue::new("frame.build_ms", ms(build)));
            record("slow_frame", prev, prev + iv, attrs);
        }

        if in_burst
            && let Some(b) = &self.burst
            && b.started.elapsed().is_ok_and(|e| e >= BURST_CHUNK)
        {
            // Report this chunk; the next input starts a fresh one.
            let area = b.area;
            self.finish_burst();
            self.input(area);
        }
        match &mut self.burst {
            Some(b) if in_burst => {
                b.frames += 1;
                b.decodes += decodes;
                b.ctx = ctx;
                b.slowest_build = b.slowest_build.max(build);
                b.total_build += build;
                if let Some(iv) = interval.filter(|_| b.frames > 1) {
                    b.slowest_interval = b.slowest_interval.max(iv);
                    b.total_interval += iv;
                }
                true
            }
            Some(_) => {
                self.finish_burst();
                false
            }
            None => false,
        }
    }

    fn finish_burst(&mut self) {
        let Some(b) = self.burst.take() else { return };
        let wall_end = SystemTime::now() - b.last_input.elapsed();
        let intervals = b.frames.saturating_sub(1).max(1);
        let avg_interval = b.total_interval / intervals;
        let avg_build = b.total_build / b.frames.max(1);
        let span_secs = wall_end.duration_since(b.started).unwrap_or_default().as_secs_f64().max(0.001);
        let fps = f64::from(b.frames) / span_secs;
        tracing::info!(
            area = b.area,
            events = b.events,
            frames = b.frames,
            fps = format!("{fps:.0}"),
            avg_frame_ms = format!("{:.1}", ms(avg_interval)),
            slowest_frame_ms = format!("{:.1}", ms(b.slowest_interval)),
            avg_build_ms = format!("{:.1}", ms(avg_build)),
            slowest_build_ms = format!("{:.1}", ms(b.slowest_build)),
            decodes = b.decodes,
            test_rows = b.ctx.test_rows,
            "scroll burst"
        );
        let mut attrs = b.ctx.attrs();
        attrs.extend([
            KeyValue::new("ui.area", b.area),
            KeyValue::new("input.events", i64::from(b.events)),
            KeyValue::new("frames", i64::from(b.frames)),
            KeyValue::new("fps", fps),
            KeyValue::new("frame.avg_ms", ms(avg_interval)),
            KeyValue::new("frame.slowest_ms", ms(b.slowest_interval)),
            KeyValue::new("build.avg_ms", ms(avg_build)),
            KeyValue::new("build.slowest_ms", ms(b.slowest_build)),
            KeyValue::new("frame.decodes", i64::from(b.decodes)),
        ]);
        record("ui_scroll", b.started, wall_end, attrs);
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// A span with explicit start and end, recorded after the fact (only for the
/// frames worth reporting). No-op unless OTLP export is configured.
fn record(name: &'static str, start: SystemTime, end: SystemTime, attrs: Vec<KeyValue>) {
    let tracer = global::tracer("testudo");
    let mut span = tracer.span_builder(name).with_start_time(start).with_attributes(attrs).start(&tracer);
    span.end_with_timestamp(end);
}
