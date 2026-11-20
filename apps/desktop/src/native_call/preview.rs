// SPDX-License-Identifier: EUPL-1.2
//! Local pictures from a publisher's existing confined capture.

use nuschl_ipc::video::{DecodeError, DecodedFrame, Decoder, Source, VideoFrame};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);
const CAMERA_DISPLAY_INTERVAL: Duration = Duration::from_millis(84);
const CAMERA_PREVIEW_PIXELS: u32 = 1280 * 720;
const CAMERA_MIN_FRAME_US: u64 = 25_000;
const MAX_QUEUED_AGE: Duration = Duration::from_millis(500);

pub(super) struct Preview {
    source: Source,
    frames: mpsc::Sender<Sample>,
    task: JoinHandle<Result<(), DecodeError>>,
    last_capture_us: Option<u64>,
    last_offer: Option<Instant>,
    awaiting_keyframe: bool,
}

struct Sample {
    queued: Instant,
    av1: Vec<u8>,
    keyframe: bool,
    contiguous: bool,
    smooth: bool,
}

impl Preview {
    pub(super) fn start(
        executable: PathBuf,
        source: Source,
        mut on_frame: impl FnMut(DecodedFrame) + Send + 'static,
    ) -> Result<Self, DecodeError> {
        let runtime = Handle::try_current().map_err(|_| DecodeError::Runtime)?;
        let mut decoder = Decoder::start(executable, source)?;
        let (frames, mut queue) = mpsc::channel::<Sample>(1);
        let task = runtime.spawn(async move {
            let mut last_picture: Option<Instant> = None;
            while let Some(sample) = queue.recv().await {
                if queue.is_closed() {
                    break;
                }
                let current = sample.queued.elapsed() <= MAX_QUEUED_AGE;
                if !current || !sample.contiguous {
                    decoder.reset();
                }
                if !current {
                    continue;
                }
                match decoder.decode(sample.keyframe, sample.av1).await {
                    Ok(Some(frame)) if !queue.is_closed() => {
                        let now = Instant::now();
                        let interval = if sample.smooth {
                            CAMERA_DISPLAY_INTERVAL
                        } else {
                            SNAPSHOT_INTERVAL
                        };
                        if last_picture.is_none_or(|last| now - last >= interval) {
                            on_frame(frame);
                            last_picture = Some(now);
                        }
                    }
                    Ok(_) | Err(DecodeError::Io(_) | DecodeError::Timeout | DecodeError::Busy) => {}
                    Err(error) => {
                        decoder.stop().await?;
                        return Err(error);
                    }
                }
            }
            decoder.stop().await
        });
        Ok(Self {
            source,
            frames,
            task,
            last_capture_us: None,
            last_offer: None,
            awaiting_keyframe: true,
        })
    }

    pub(super) fn offer(&mut self, frame: &VideoFrame, visible: bool) {
        let previous = self.last_capture_us.replace(frame.capture_time_us);
        // Decode contiguous frames for modest cameras. Larger and faster
        // sources only use the encoder's naturally occurring keyframes.
        let smooth = self.source == Source::Camera
            && u32::from(frame.width) * u32::from(frame.height) <= CAMERA_PREVIEW_PIXELS
            && previous.is_none_or(|previous| {
                frame.capture_time_us.saturating_sub(previous) >= CAMERA_MIN_FRAME_US
            });
        self.offer_frame(&frame.av1, frame.keyframe, visible, smooth);
    }

    fn offer_frame(&mut self, av1: &[u8], keyframe: bool, visible: bool, smooth: bool) {
        let now = Instant::now();
        if !visible
            || (!keyframe && (!smooth || self.awaiting_keyframe))
            || (!smooth
                && self
                    .last_offer
                    .is_some_and(|last| now - last < SNAPSHOT_INTERVAL))
        {
            self.awaiting_keyframe = true;
            return;
        }

        // Reserve space before copying to keep preview work off the upload path.
        let Ok(slot) = self.frames.try_reserve() else {
            self.awaiting_keyframe = true;
            return;
        };
        slot.send(Sample {
            queued: now,
            av1: av1.to_vec(),
            keyframe,
            contiguous: !self.awaiting_keyframe,
            smooth,
        });
        self.awaiting_keyframe = false;
        self.last_offer = Some(now);
    }

    pub(super) async fn stop(self) -> Result<(), DecodeError> {
        drop(self.frames);
        self.task.await.map_err(|_| DecodeError::Stopped)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn previews_skip_hidden_and_excess_frames_and_recover_at_a_keyframe() {
        let (frames, mut queue) = mpsc::channel(1);
        let mut preview = Preview {
            source: Source::Camera,
            frames,
            task: tokio::spawn(async { Ok(()) }),
            last_capture_us: None,
            last_offer: None,
            awaiting_keyframe: true,
        };
        preview.offer_frame(b"hidden", true, false, true);
        preview.offer_frame(b"dependent", false, true, true);
        assert!(queue.is_empty());

        preview.offer_frame(b"key", true, true, true);
        preview.offer_frame(b"queue full", false, true, true);
        let sample = queue.try_recv().unwrap();
        assert_eq!(sample.av1, b"key");
        assert!(!sample.contiguous);

        preview.offer_frame(b"after a gap", false, true, true);
        assert!(queue.is_empty());

        preview.offer_frame(b"recovered", true, true, true);
        assert!(!queue.try_recv().unwrap().contiguous);
        preview.offer_frame(b"next", false, true, true);
        assert!(queue.try_recv().unwrap().contiguous);

        preview.offer_frame(b"too soon", true, true, false);
        assert!(queue.is_empty());
        preview.last_offer = Instant::now().checked_sub(SNAPSHOT_INTERVAL);
        preview.offer_frame(b"snapshot", true, true, false);
        assert_eq!(queue.try_recv().unwrap().av1, b"snapshot");
        preview.stop().await.unwrap();
    }
}
