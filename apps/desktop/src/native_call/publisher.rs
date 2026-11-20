// SPDX-License-Identifier: EUPL-1.2
//! Each enabled source has its own portal grant, encoder and preview.

use super::{Display, Latest, Quality, Selection, purpose, stats, text, video_kind};
use futures_util::{FutureExt as _, future::BoxFuture, stream::FuturesUnordered};
use nuschl_client::native::sfu::{Media, MediaEvent, Purpose, VideoKind};
use nuschl_ipc::video::{
    Capture, CaptureFailure, CaptureLimits, Error, Source, VideoFrame, portal,
};
use std::{
    future::pending,
    os::unix::process::ExitStatusExt as _,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, timeout},
};

pub(super) const SOURCES: [Source; 2] = [Source::Camera, Source::Screen];
const REOPEN_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];
const REOPEN_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTHY_CAPTURE: Duration = Duration::from_secs(30);
pub(super) type Reopening = BoxFuture<'static, (CaptureAttempt, Option<portal::Grant>)>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CaptureMode {
    #[default]
    HardwareWithPreview,
    Hardware,
    Software,
}

impl CaptureMode {
    fn after_failure(self, error: &Error, hardware: Option<bool>) -> Self {
        let hardware_failure = matches!(
            error,
            Error::Capture(
                CaptureFailure::Hardware
                    | CaptureFailure::HardwareFormat
                    | CaptureFailure::HardwareResource
            )
        ) || matches!(
            error,
            Error::CaptureExit(status) if status.code() == Some(139) || status.signal() == Some(11)
        );
        if !hardware_failure {
            return self;
        }
        match (self, hardware) {
            (Self::HardwareWithPreview, Some(false)) | (Self::Hardware | Self::Software, _) => {
                Self::Software
            }
            (Self::HardwareWithPreview, _) => Self::Hardware,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CaptureAttempt {
    pub number: usize,
    mode: CaptureMode,
}

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
    pub reopening: Option<Reopening>,
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
            reopening: None,
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

    pub fn enabled(&self, source: Source) -> bool {
        self.active(source) || source == Source::Screen && self.reopening.is_some()
    }

    pub fn start(
        &mut self,
        grant: portal::Grant,
        worker: &Path,
        selection: Selection,
        updates: Arc<Latest>,
        attempt: CaptureAttempt,
    ) -> Result<(), String> {
        let source = grant.source();
        let screen = grant.screen_reopen();
        self.next_preview = self
            .next_preview
            .checked_add(1)
            .ok_or("Video preview limit reached")?;
        let preview_id = self.next_preview;
        let encoding = Encoding {
            kind: video_kind(source),
            remote: None,
            remote_limit: None,
            bitrate_estimate: None,
            hardware: None,
            capture_format: None,
        };
        let applied = encoding.ceiling(selection.quality);
        let (capture, preview) = if attempt.mode == CaptureMode::Hardware {
            (
                grant
                    .start_with_limits(worker.to_owned(), applied)
                    .map_err(text)?,
                None,
            )
        } else {
            let (pictures, mut preview) = mpsc::channel(1);
            let capture = match attempt.mode {
                CaptureMode::Software => {
                    grant.start_software_with_preview(worker.to_owned(), applied, pictures)
                }
                _ => grant.start_with_preview(worker.to_owned(), applied, pictures),
            }
            .map_err(text)?;
            let preview = tokio::spawn(async move {
                while let Some(frame) = preview.recv().await {
                    updates.local_frame(preview_id, frame);
                }
            });
            (capture, Some(preview))
        };
        tracing::info!(target: "nuschl_capture", ?source, mode = ?attempt.mode, attempt = attempt.number, "capture worker started");
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
            screen,
            attempt,
            recovering_since: None,
        });
        Ok(())
    }

    pub fn retire(&mut self, source: Source) {
        if source == Source::Screen {
            self.reopening = None;
        }
        if let Some(track) = self.slots[index(source)].take() {
            self.retiring.push(track.stop().boxed());
        }
    }

    pub fn recover(&mut self, source: Source, allowed: bool, error: &Error) -> bool {
        let Some(mut track) = self.slots[index(source)].take() else {
            return false;
        };
        tracing::warn!(target: "nuschl_capture", ?source, mode = ?track.attempt.mode, hardware = ?track.encoding.hardware, %error, "capture stopped");
        if allowed
            && recoverable(error)
            && let Some(delay) = REOPEN_DELAYS.get(track.attempt.number).copied()
            && let Some(grant) = track.screen.take()
        {
            let attempt = CaptureAttempt {
                number: track.attempt.number + 1,
                mode: track
                    .attempt
                    .mode
                    .after_failure(error, track.encoding.hardware),
            };
            let (retired, wait) = oneshot::channel();
            self.retiring.push(
                async move {
                    track.stop().await;
                    let _ = retired.send(());
                }
                .boxed(),
            );
            self.reopening = Some(
                async move {
                    if wait.await.is_err() {
                        return (attempt, None);
                    }
                    sleep(delay).await;
                    tracing::info!(target: "nuschl_capture", mode = ?attempt.mode, attempt = attempt.number, "reopening screen capture");
                    let grant = timeout(REOPEN_TIMEOUT, grant.reopen())
                        .await
                        .ok()
                        .and_then(Result::ok);
                    (attempt, grant)
                }
                .boxed(),
            );
            true
        } else {
            self.retiring.push(track.stop().boxed());
            false
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
                    .map(|track| (track.preview_id, video_kind(source), track.label()))
            }));
        }
        next.is_some()
    }

    pub fn preview(&self, visible: bool) {
        for source in SOURCES {
            if let Some(track) = &self.slots[index(source)]
                && track.preview.is_some()
            {
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
                        updates.status(track.label());
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
                limit,
            } => {
                if let Some(track) =
                    source(purpose).and_then(|source| self.slots[index(source)].as_mut())
                {
                    track.encoding.remote_limit = limit;
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
                let sample = track.stats.sample(Instant::now());
                display.report(
                    track.encoding.kind,
                    sample,
                    Some(stats::Encoder {
                        requested: track.encoding.preferred(selection.quality),
                        limit: track
                            .encoding
                            .active_limit(selection.quality, track.applied),
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
        if track.attempt.number > 0 {
            let since = track.recovering_since.get_or_insert_with(|| {
                tracing::info!(
                    target: "nuschl_capture",
                    attempt = track.attempt.number,
                    mode = ?track.attempt.mode,
                    hardware = frame.hardware,
                    "screen capture resumed"
                );
                Instant::now()
            });
            if since.elapsed() >= HEALTHY_CAPTURE {
                track.attempt.number = 0;
            }
        }
        track.encoding.hardware = Some(frame.hardware);
        track.encoding.capture_format = Some((frame.width.min(frame.height), frame.fps));
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
    preview: Option<JoinHandle<()>>,
    preview_id: u64,
    encoding: Encoding,
    applied: CaptureLimits,
    stats: stats::Stats,
    ready: bool,
    screen: Option<portal::ScreenGrant>,
    attempt: CaptureAttempt,
    recovering_since: Option<Instant>,
}

impl LocalVideo {
    fn label(&self) -> &'static str {
        match (self.encoding.kind, self.ready, self.preview.is_some()) {
            (VideoKind::Screen, _, false) => "Your screen · local preview off",
            (VideoKind::Camera, true, _) => "Your camera",
            (VideoKind::Screen, true, _) => "Your screen",
            (VideoKind::Camera, false, _) => "Your camera · waiting for a viewer",
            (VideoKind::Screen, false, _) => "Your screen · waiting for a viewer",
        }
    }

    pub async fn next(track: &mut Option<Self>) -> Result<VideoFrame, Error> {
        match track {
            Some(track) => track.capture.next().await,
            None => pending().await,
        }
    }

    async fn stop(self) {
        drop(self.screen);
        if let Err(error) = self.capture.stop().await {
            tracing::debug!(%error, "could not stop capture");
        }
        if let Some(preview) = self.preview
            && let Err(error) = preview.await
        {
            tracing::debug!(%error, "could not stop the local preview");
        }
    }
}

pub(super) async fn next_reopen(
    reopening: &mut Option<Reopening>,
) -> (CaptureAttempt, Option<portal::Grant>) {
    match reopening {
        Some(reopening) => reopening.await,
        None => pending().await,
    }
}

fn recoverable(error: &Error) -> bool {
    match error {
        Error::CaptureClosed
        | Error::Capture(_)
        | Error::CaptureExit(_)
        | Error::Timeout
        | Error::Stopped => true,
        Error::Io(error) => matches!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

struct Encoding {
    kind: VideoKind,
    remote: Option<CaptureLimits>,
    remote_limit: Option<nuschl_protocol::sfu::VideoLimit>,
    bitrate_estimate: Option<u32>,
    hardware: Option<bool>,
    capture_format: Option<(u16, u16)>,
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

    fn active_limit(&self, quality: Quality, applied: CaptureLimits) -> &'static str {
        let ceiling = self.ceiling(quality);
        if applied != ceiling
            && self
                .bitrate_estimate
                .is_some_and(|estimate| estimate < ceiling.bitrate)
        {
            "Sender upload"
        } else if ceiling != self.preferred(quality) {
            match (self.remote, self.remote_limit) {
                (None, _) => "Initial encoder ceiling",
                (_, Some(limit)) => stats::limit_label(limit),
                _ => "Server encoder ceiling · reason unavailable",
            }
        } else {
            match self.capture_format {
                Some((height, _)) if height.saturating_add(2) < applied.height => {
                    "Capture resolution"
                }
                Some((_, fps)) if fps < applied.fps => match self.hardware {
                    Some(false) => "Software encoder frame rate",
                    _ => "Capture frame rate",
                },
                Some(_) => "Within selected quality",
                None => "Unavailable",
            }
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
    use super::{
        CaptureFailure, CaptureLimits, CaptureMode, Encoding, Error, Publisher, Quality, Source,
        VideoKind, recoverable,
    };
    use futures_util::FutureExt as _;
    use std::{future::pending, io, os::unix::process::ExitStatusExt as _, process::ExitStatus};
    use tokio::sync::oneshot;

    #[test]
    fn closed_capture_can_reopen_and_protocol_errors_stop() {
        assert!(recoverable(&Error::CaptureClosed));
        assert!(recoverable(&Error::Timeout));
        assert!(recoverable(
            &io::Error::from(io::ErrorKind::BrokenPipe).into()
        ));
        assert!(!recoverable(&Error::Confinement));
        assert!(!recoverable(
            &io::Error::from(io::ErrorKind::InvalidData).into()
        ));
    }

    #[test]
    fn hardware_failures_try_encoding_alone_then_software() {
        let mut mode = CaptureMode::HardwareWithPreview;
        let segfault = Error::CaptureExit(ExitStatus::from_raw(139 << 8));
        mode = mode.after_failure(&segfault, Some(true));
        assert_eq!(mode, CaptureMode::Hardware);
        mode = mode.after_failure(&Error::Capture(CaptureFailure::HardwareFormat), None);
        assert_eq!(mode, CaptureMode::Software);
        assert_eq!(
            CaptureMode::HardwareWithPreview.after_failure(&segfault, Some(false)),
            CaptureMode::Software,
        );
        assert_eq!(
            CaptureMode::HardwareWithPreview
                .after_failure(&Error::CaptureExit(ExitStatus::from_raw(11)), None),
            CaptureMode::Hardware,
        );
        assert_eq!(
            mode.after_failure(&segfault, Some(false)),
            CaptureMode::Software,
        );
    }

    #[test]
    fn ordinary_capture_closure_keeps_the_current_mode() {
        for mode in [
            CaptureMode::HardwareWithPreview,
            CaptureMode::Hardware,
            CaptureMode::Software,
        ] {
            assert_eq!(mode.after_failure(&Error::CaptureClosed, None), mode);
            assert_eq!(
                mode.after_failure(&Error::CaptureExit(ExitStatus::from_raw(137 << 8)), None),
                mode,
            );
        }
    }

    #[test]
    fn stopping_screen_drops_pending_reopen_and_retains_retirement() {
        let mut publisher = Publisher::new(false);
        let (reopening, mut cancelled) = oneshot::channel::<()>();
        publisher.reopening = Some(
            async move {
                let _reopening = reopening;
                pending().await
            }
            .boxed(),
        );
        publisher.retiring.push(pending().boxed());
        assert!(publisher.enabled(Source::Screen));
        assert!(!publisher.enabled(Source::Camera));
        publisher.retire(Source::Camera);
        assert!(publisher.enabled(Source::Screen));
        publisher.retire(Source::Screen);
        assert!(!publisher.enabled(Source::Screen));
        assert!(matches!(
            cancelled.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert_eq!(publisher.retiring.len(), 1);
    }

    #[test]
    fn a_constrained_screen_reduces_dimensions_and_cadence_then_recovers() {
        let mut encoding = Encoding {
            kind: VideoKind::Screen,
            remote: Some(CaptureLimits {
                height: 1440,
                fps: 60,
                bitrate: 8_000_000,
            }),
            remote_limit: None,
            bitrate_estimate: None,
            hardware: None,
            capture_format: None,
        };
        let quality = Quality::Tier(2);
        let full = encoding.ceiling(quality);
        assert_eq!((full.height, full.fps, full.bitrate), (1440, 60, 8_000_000));
        encoding.capture_format = Some((1440, 60));
        assert_eq!(
            encoding.active_limit(quality, full),
            "Within selected quality"
        );
        encoding.capture_format = Some((1080, 60));
        assert_eq!(encoding.active_limit(quality, full), "Capture resolution");
        encoding.capture_format = Some((1440, 15));
        encoding.hardware = Some(false);
        assert_eq!(
            encoding.active_limit(quality, full),
            "Software encoder frame rate"
        );
        encoding.bitrate_estimate = Some(100_000);
        let reduced = encoding.limits(quality, full);
        assert_eq!(
            (reduced.height, reduced.fps, reduced.bitrate),
            (360, 7, 100_000)
        );
        assert_eq!(encoding.active_limit(quality, reduced), "Sender upload");
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
