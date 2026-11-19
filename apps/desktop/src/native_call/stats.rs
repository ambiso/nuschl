// SPDX-License-Identifier: EUPL-1.2

use std::{
    mem,
    time::{Duration, Instant},
};

pub(super) const INTERVAL: Duration = Duration::from_secs(1);
const MAX_SAMPLE_SPAN: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Sfu,
    Direct,
    Turn,
}

impl Path {
    fn label(self) -> &'static str {
        match self {
            Self::Sfu => "Call server (SFU)",
            Self::Direct => "Direct",
            Self::Turn => "TURN relay",
        }
    }
}

#[derive(Default)]
struct Interval {
    bytes: u64,
    frames: u32,
    decode_time: Duration,
    decoded: u32,
}

/// Per-source video counters. The call task snapshots them at its stats tick.
pub(super) struct Stats {
    since: Instant,
    resolution: Option<(u16, u16)>,
    window: Interval,
    path: Path,
}

impl Stats {
    pub(super) fn new(relay_only: bool, now: Instant) -> Self {
        Self {
            since: now,
            resolution: None,
            window: Interval::default(),
            path: if relay_only { Path::Turn } else { Path::Sfu },
        }
    }

    pub(super) fn encoded(&mut self, bytes: usize) {
        self.window.bytes = self.window.bytes.saturating_add(bytes as u64);
    }

    /// For received video, pass the time spent waiting for the confined decoder.
    pub(super) fn frame(&mut self, width: u16, height: u16, decode_time: Option<Duration>) {
        self.resolution = Some((width, height));
        self.window.frames = self.window.frames.saturating_add(1);
        if let Some(time) = decode_time {
            self.window.decode_time = self.window.decode_time.saturating_add(time);
            self.window.decoded = self.window.decoded.saturating_add(1);
        }
    }

    pub(super) fn direct(&mut self, active: bool) {
        if self.path != Path::Turn {
            self.path = if active { Path::Direct } else { Path::Sfu };
        }
    }

    pub(super) fn clear(&mut self, now: Instant) {
        self.since = now;
        self.resolution = None;
        self.window = Interval::default();
    }

    pub(super) fn sample(&mut self, now: Instant) -> Snapshot {
        let elapsed = now.saturating_duration_since(mem::replace(&mut self.since, now));
        let window = mem::take(&mut self.window);
        let valid_span = !elapsed.is_zero() && elapsed <= MAX_SAMPLE_SPAN;
        Snapshot {
            resolution: self.resolution,
            fps: (valid_span && self.resolution.is_some())
                .then(|| f64::from(window.frames) / elapsed.as_secs_f64()),
            bitrate: (valid_span && (self.resolution.is_some() || window.bytes > 0))
                .then(|| 8. * window.bytes as f64 / elapsed.as_secs_f64()),
            decode_ms: (valid_span && window.decoded > 0)
                .then(|| 1000. * window.decode_time.as_secs_f64() / f64::from(window.decoded)),
            path: self.path,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Snapshot {
    resolution: Option<(u16, u16)>,
    fps: Option<f64>,
    bitrate: Option<f64>,
    decode_ms: Option<f64>,
    path: Path,
}

impl Snapshot {
    pub(super) fn rows(self, owned: bool) -> impl Iterator<Item = (&'static str, String)> {
        let resolution = self
            .resolution
            .map_or_else(unavailable, |(width, height)| format!("{width} × {height}"));
        let frame_label = if owned { "Encoded FPS" } else { "Decoded FPS" };
        let bitrate = self.bitrate.map_or_else(unavailable, |rate| {
            if rate >= 1_000_000. {
                format!("{:.1} Mb/s", rate / 1_000_000.)
            } else {
                format!("{:.1} kb/s", rate / 1000.)
            }
        });
        [
            ("Resolution", resolution),
            (frame_label, number(self.fps, "fps")),
            ("Video payload bitrate", bitrate),
            ("Connection", self.path.label().into()),
        ]
        .into_iter()
        .chain((!owned).then(|| ("Local decode delay", number(self.decode_ms, "ms"))))
    }
}

fn unavailable() -> String {
    "—".into()
}

fn number(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(unavailable, |value| format!("{value:.1} {unit}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_counts_video_over_the_elapsed_interval() {
        let start = Instant::now();
        let mut stats = Stats::new(false, start);
        for _ in 0..60 {
            stats.frame(1920, 1080, None);
        }
        stats.encoded(375_000);
        let at = start + Duration::from_secs(2);
        let rows = stats.sample(at).rows(true).collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("Resolution", "1920 × 1080".into()),
                ("Encoded FPS", "30.0 fps".into()),
                ("Video payload bitrate", "1.5 Mb/s".into()),
                ("Connection", "Call server (SFU)".into()),
            ]
        );
        let quiet = stats.sample(at + INTERVAL);
        assert_eq!(quiet.fps, Some(0.));
        assert_eq!(quiet.bitrate, Some(0.));
    }

    #[test]
    fn receiver_records_decode_delay_and_clears_a_departed_source() {
        let start = Instant::now();
        let mut stats = Stats::new(false, start);
        stats.encoded(32_000);
        stats.frame(1920, 1080, Some(Duration::from_millis(10)));
        stats.frame(1280, 720, Some(Duration::from_millis(20)));
        let at = start + INTERVAL;
        let rows = stats.sample(at).rows(false).collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("Resolution", "1280 × 720".into()),
                ("Decoded FPS", "2.0 fps".into()),
                ("Video payload bitrate", "256.0 kb/s".into()),
                ("Connection", "Call server (SFU)".into()),
                ("Local decode delay", "15.0 ms".into()),
            ]
        );
        stats.clear(at);
        let cleared = stats.sample(at + INTERVAL);
        assert_eq!(cleared.resolution, None);
        assert_eq!(cleared.fps, None);
        assert_eq!(cleared.bitrate, None);
        assert_eq!(cleared.decode_ms, None);
    }

    #[test]
    fn long_gaps_clear_rates_and_route_changes_follow_the_call_policy() {
        let start = Instant::now();
        let mut stats = Stats::new(false, start);
        stats.frame(640, 480, None);
        stats.encoded(16_000);
        stats.direct(true);
        let delayed = stats.sample(start + Duration::from_secs(6));
        assert_eq!(delayed.path, Path::Direct);
        assert_eq!(delayed.fps, None);
        assert_eq!(delayed.bitrate, None);
        stats.direct(false);
        assert_eq!(stats.sample(start + Duration::from_secs(7)).path, Path::Sfu);

        let mut forced = Stats::new(true, start);
        forced.direct(true);
        assert_eq!(forced.sample(start + INTERVAL).path, Path::Turn);
    }
}
