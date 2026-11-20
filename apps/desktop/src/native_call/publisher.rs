// SPDX-License-Identifier: EUPL-1.2
//! Each enabled source has its own portal grant, encoder and preview.

use super::{Display, Latest, Quality, Selection, purpose, stats, text, video_kind};
use futures_util::{FutureExt as _, future::BoxFuture, stream::FuturesUnordered};
use nuschl_client::native::sfu::{Media, MediaEvent, Purpose, VideoKind};
use nuschl_ipc::video::{Capture, CaptureLimits, Error, Source, VideoFrame, portal};
use std::{future::pending, path::Path, sync::Arc, time::Instant};
use tokio::{sync::mpsc, task::JoinHandle};

pub(super) const SOURCES: [Source; 2] = [Source::Camera, Source::Screen];

pub(super) fn index(source: Source) -> usize {
    match source {
        Source::Camera => 0,
        Source::Screen => 1,
    }
}

fn source(purpose: Purpose) -> Option<Source> {
    match purpose {
        Purpose::Camera => Some(Source::Camera),
        Purpose::ScreenVideo => Some(Source::Screen),
        _ => None,
    }
}

pub(super) struct Publisher {
    pub slots: [Option<LocalVideo>; 2],
    pub retiring: FuturesUnordered<BoxFuture<'static, ()>>,
    focused: Option<Source>,
    next_preview: u64,
    estimate: Option<u64>,
    direct: bool,
    relay_only: bool,
}

impl Publisher {
    pub fn new(relay_only: bool) -> Self {
        Self {
            slots: [None, None],
            retiring: FuturesUnordered::new(),
            focused: None,
            next_preview: 0,
            estimate: None,
            direct: false,
            relay_only,
        }
    }

    pub fn active(&self, source: Source) -> bool {
        self.slots[index(source)].is_some()
    }

    pub fn start(
        &mut self,
        grant: portal::Grant,
        worker: &Path,
        selection: Selection,
        updates: Arc<Latest>,
    ) -> Result<(), String> {
        let source = grant.source();
        self.next_preview = self
            .next_preview
            .checked_add(1)
            .ok_or("Video preview limit reached")?;
        let preview_id = self.next_preview;
        let encoding = Encoding {
            kind: video_kind(source),
            remote: None,
            bitrate_estimate: None,
            hardware: None,
        };
        let applied = encoding.ceiling(selection.quality);
        let (pictures, mut preview) = mpsc::channel(1);
        let capture = grant
            .start_with_preview(worker.to_owned(), applied, pictures)
            .map_err(text)?;
        let preview = tokio::spawn(async move {
            while let Some(frame) = preview.recv().await {
                updates.local_frame(preview_id, frame);
            }
        });
        let mut stats = stats::Stats::new(self.relay_only, Instant::now());
        stats.direct(self.direct);
        self.slots[index(source)] = Some(LocalVideo {
            capture,
            preview,
            preview_id,
            encoding,
            applied,
            stats,
            ready: false,
        });
        Ok(())
    }

    pub fn retire(&mut self, source: Source) {
        if let Some(track) = self.slots[index(source)].take() {
            self.retiring.push(track.stop().boxed());
        }
    }

    pub async fn stop(mut self) {
        use futures_util::StreamExt as _;
        for source in SOURCES {
            self.retire(source);
        }
        while self.retiring.next().await.is_some() {}
    }

    pub fn focus(&mut self, selection: Selection, updates: &Latest) -> bool {
        let next = selection
            .view
            .and_then(source)
            .filter(|source| self.active(*source))
            .or_else(|| {
                [Source::Screen, Source::Camera]
                    .into_iter()
                    .find(|source| self.active(*source))
            });
        if next != self.focused {
            self.focused = next;
            updates.local_view(next.and_then(|source| {
                self.slots[index(source)]
                    .as_ref()
                    .map(|track| (track.preview_id, video_kind(source), track.ready))
            }));
        }
        next.is_some()
    }

    pub fn preview(&self, visible: bool) {
        for source in SOURCES {
            if let Some(track) = &self.slots[index(source)] {
                track
                    .capture
                    .set_preview(visible && self.focused == Some(source));
            }
        }
    }

    pub fn event(&mut self, event: &MediaEvent, updates: &Latest) {
        match *event {
            MediaEvent::SourceReady { purpose } => {
                if let Some(source) = source(purpose)
                    && let Some(track) = &mut self.slots[index(source)]
                {
                    track.ready = true;
                    track.capture.request_keyframe();
                    if self.focused == Some(source) {
                        updates.status(label(track.encoding.kind, true));
                    }
                }
            }
            MediaEvent::SourceKeyframeRequired { purpose } => {
                if let Some(track) =
                    source(purpose).and_then(|source| self.slots[index(source)].as_ref())
                {
                    track.capture.request_keyframe();
                }
            }
            MediaEvent::SourceLimits {
                purpose,
                height,
                fps,
                bitrate,
            } => {
                if let Some(track) =
                    source(purpose).and_then(|source| self.slots[index(source)].as_mut())
                {
                    track.encoding.remote = Some(CaptureLimits {
                        height,
                        fps: fps.into(),
                        bitrate: video_rate(bitrate),
                    });
                }
            }
            MediaEvent::BandwidthEstimate { bitrate } => self.estimate = Some(bitrate),
            MediaEvent::Direct { active } => {
                self.direct = active;
                self.estimate = None;
                for track in self.slots.iter_mut().flatten() {
                    track.stats.direct(active);
                }
            }
            _ => {}
        }
    }

    pub fn retune(&mut self, display: &Display<'_>, selection: Selection, report: bool) {
        let count = self.slots.iter().flatten().count().max(1) as u64;
        // RTC charges packet and SFrame overhead and splits the video budget across enabled sources.
        let share = self
            .estimate
            .map(|rate| (rate.saturating_mul(72) / 100 / count).min(50_000_000) as u32);
        for source in SOURCES {
            let Some(track) = self.slots[index(source)].as_mut() else {
                continue;
            };
            track.encoding.bitrate_estimate = share;
            let next = track.encoding.limits(selection.quality, track.applied);
            // Capture closure is reported by Capture::next.
            if next != track.applied && track.capture.set_limits(next).is_ok() {
                track.applied = next;
            }
            if report && self.focused == Some(source) {
                display.report(
                    track.encoding.kind,
                    track.stats.sample(Instant::now()),
                    Some(stats::Encoder {
                        budget_bps: track.encoding.bitrate_estimate,
                        target: track.applied,
                        ceiling: track.encoding.remote,
                        hardware: track.encoding.hardware,
                    }),
                );
            }
        }
    }

    pub fn frame(
        &mut self,
        source: Source,
        frame: VideoFrame,
        media: &Media,
    ) -> Result<(), String> {
        let Some(track) = &mut self.slots[index(source)] else {
            return Ok(());
        };
        track.encoding.hardware = Some(frame.hardware);
        track.stats.frame(frame.width, frame.height, None);
        if track.ready {
            let bytes = frame.av1.len();
            let ticks = frame.capture_time_us.saturating_mul(9) / 100;
            if media
                .send_source_frame(purpose(source), ticks, frame.keyframe, frame.av1)
                .map_err(text)?
            {
                track.stats.encoded(bytes);
            } else {
                track.capture.request_keyframe();
            }
        }
        Ok(())
    }
}

pub(super) struct LocalVideo {
    capture: Capture,
    preview: JoinHandle<()>,
    preview_id: u64,
    encoding: Encoding,
    applied: CaptureLimits,
    stats: stats::Stats,
    ready: bool,
}

impl LocalVideo {
    pub async fn next(track: &mut Option<Self>) -> Result<VideoFrame, Error> {
        match track {
            Some(track) => track.capture.next().await,
            None => pending().await,
        }
    }

    async fn stop(self) {
        if let Err(error) = self.capture.stop().await {
            tracing::debug!(%error, "could not stop capture");
        }
        if let Err(error) = self.preview.await {
            tracing::debug!(%error, "could not stop the local preview");
        }
    }
}

pub(super) fn label(kind: VideoKind, ready: bool) -> &'static str {
    match (kind, ready) {
        (VideoKind::Camera, true) => "Your camera",
        (VideoKind::Screen, true) => "Your screen",
        (VideoKind::Camera, false) => "Your camera · waiting for a viewer",
        (VideoKind::Screen, false) => "Your screen · waiting for a viewer",
    }
}

struct Encoding {
    kind: VideoKind,
    remote: Option<CaptureLimits>,
    bitrate_estimate: Option<u32>,
    hardware: Option<bool>,
}

impl Encoding {
    fn preferred(&self, quality: Quality) -> CaptureLimits {
        let tier = self.kind.ladder()[self.kind.request(quality, u32::MAX)];
        CaptureLimits {
            height: tier.height,
            fps: tier.fps.into(),
            bitrate: video_rate(tier.ceiling),
        }
    }

    fn ceiling(&self, quality: Quality) -> CaptureLimits {
        let local = self.preferred(quality);
        let initial_bitrate = match self.kind {
            VideoKind::Camera => 2_000_000,
            VideoKind::Screen => 4_000_000,
        };
        let remote = self.remote.unwrap_or(CaptureLimits {
            bitrate: initial_bitrate,
            ..local
        });
        CaptureLimits {
            height: local.height.min(remote.height),
            fps: local.fps.min(remote.fps),
            bitrate: local.bitrate.min(remote.bitrate),
        }
    }

    fn limits(&self, quality: Quality, applied: CaptureLimits) -> CaptureLimits {
        let mut limits = self.ceiling(quality);
        let Some(upload) = self.bitrate_estimate else {
            return limits;
        };
        let budget = u64::from(limits.bitrate.min(upload));
        let current = self.kind.tier_of(u32::from(applied.height));
        let affordable = self.kind.affordable(budget);
        // Reserve headroom for an upgrade to keep small estimates from repeatedly
        // changing the capture format.
        let tier = if affordable > current {
            current.max(self.kind.affordable(budget * 10 / 13))
        } else {
            affordable
        };
        let tier = self.kind.ladder()[tier];
        let base = self.kind.ladder()[0];
        let recovering = applied.height < base.height && limits.height >= base.height;
        let floor = if recovering {
            base.floor * 13 / 10
        } else {
            base.floor
        };
        let (height, fps) = if self.kind == VideoKind::Screen && budget < floor {
            (base.height / 2, base.fps / 2)
        } else {
            (tier.height, tier.fps)
        };
        limits.height = limits.height.min(height);
        limits.fps = limits.fps.min(u16::from(fps));
        limits.bitrate = video_rate(budget.min(tier.ceiling));
        limits
    }
}

fn video_rate(bitrate: u64) -> u32 {
    bitrate.clamp(64_000, 50_000_000) as u32
}

#[cfg(test)]
mod tests {
    use super::{CaptureLimits, Encoding, Quality, VideoKind};

    #[test]
    fn a_constrained_screen_reduces_dimensions_and_cadence_then_recovers() {
        let mut encoding = Encoding {
            kind: VideoKind::Screen,
            remote: Some(CaptureLimits {
                height: 1440,
                fps: 60,
                bitrate: 8_000_000,
            }),
            bitrate_estimate: None,
            hardware: None,
        };
        let quality = Quality::Tier(2);
        let full = encoding.ceiling(quality);
        assert_eq!((full.height, full.fps, full.bitrate), (1440, 60, 8_000_000));
        encoding.bitrate_estimate = Some(100_000);
        let reduced = encoding.limits(quality, full);
        assert_eq!(
            (reduced.height, reduced.fps, reduced.bitrate),
            (360, 7, 100_000)
        );
        encoding.bitrate_estimate = Some(450_000);
        assert_eq!(encoding.limits(quality, reduced).height, 360);
        encoding.bitrate_estimate = Some(6_000_000);
        assert_eq!(
            encoding.limits(quality, reduced),
            CaptureLimits {
                height: 1440,
                fps: 60,
                bitrate: 6_000_000,
            }
        );
    }
}
