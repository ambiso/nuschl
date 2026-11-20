// SPDX-License-Identifier: EUPL-1.2
//! Local pictures from a publisher's existing confined capture.

use nuschl_ipc::video::{DecodeError, DecodedFrame, Decoder, Source, VideoFrame};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};

const DISPLAY_INTERVAL: Duration = Duration::from_millis(32);
const MAX_QUEUED_AGE: Duration = Duration::from_millis(150);

pub(super) struct Preview {
    frames: mpsc::Sender<Sample>,
    task: JoinHandle<Result<(), DecodeError>>,
    awaiting_keyframe: bool,
}

struct Sample {
    queued: Instant,
    av1: Vec<u8>,
    keyframe: bool,
    contiguous: bool,
}

impl Preview {
    pub(super) fn start(
        executable: PathBuf,
        source: Source,
        mut on_frame: impl FnMut(DecodedFrame) + Send + 'static,
    ) -> Result<Self, DecodeError> {
        let runtime = Handle::try_current().map_err(|_| DecodeError::Runtime)?;
        let mut decoder = Decoder::start_preview(executable, source)?;
        let (frames, mut queue) = mpsc::channel::<Sample>(2);
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
                        if last_picture.is_none_or(|last| now - last >= DISPLAY_INTERVAL) {
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
            frames,
            task,
            awaiting_keyframe: true,
        })
    }

    pub(super) fn offer(&mut self, frame: &VideoFrame, visible: bool) {
        self.offer_frame(&frame.av1, frame.keyframe, visible);
    }

    fn offer_frame(&mut self, av1: &[u8], keyframe: bool, visible: bool) {
        if !visible || (!keyframe && self.awaiting_keyframe) {
            self.awaiting_keyframe = true;
            return;
        }

        // Reserve space before copying to keep preview work off the upload path.
        let Ok(slot) = self.frames.try_reserve() else {
            self.awaiting_keyframe = true;
            return;
        };
        slot.send(Sample {
            queued: Instant::now(),
            av1: av1.to_vec(),
            keyframe,
            contiguous: !self.awaiting_keyframe,
        });
        self.awaiting_keyframe = false;
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
            frames,
            task: tokio::spawn(async { Ok(()) }),
            awaiting_keyframe: true,
        };
        preview.offer_frame(b"hidden", true, false);
        preview.offer_frame(b"dependent", false, true);
        assert!(queue.is_empty());

        preview.offer_frame(b"key", true, true);
        preview.offer_frame(b"queue full", false, true);
        let sample = queue.try_recv().unwrap();
        assert_eq!(sample.av1, b"key");
        assert!(!sample.contiguous);

        preview.offer_frame(b"after a gap", false, true);
        assert!(queue.is_empty());

        preview.offer_frame(b"recovered", true, true);
        assert!(!queue.try_recv().unwrap().contiguous);
        preview.offer_frame(b"next", false, true);
        assert!(queue.try_recv().unwrap().contiguous);

        preview.stop().await.unwrap();
    }
}
