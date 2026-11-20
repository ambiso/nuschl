// SPDX-License-Identifier: EUPL-1.2
//! Select an incoming source and decode only the featured video.

use super::{Display, Selection, stats, text, video_kind};
use crate::{
    dialogs::{Tone, named_button},
    layout,
    state::Desktop,
};
use gpui_kit::{prelude::*, *};
use nuschl_client::native::sfu::{Media, MediaEvent, Purpose};
use nuschl_ipc::video::{DecodeError, Decoder, Source};
use std::{path::PathBuf, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Track {
    publisher: u64,
    purpose: Purpose,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Sources {
    camera: Option<u64>,
    screen: Option<u64>,
}

impl Sources {
    pub(super) fn available(self) -> (bool, bool) {
        (self.camera.is_some(), self.screen.is_some())
    }

    fn set(&mut self, purpose: Purpose, publisher: Option<u64>) -> bool {
        let slot = match purpose {
            Purpose::Camera => &mut self.camera,
            Purpose::ScreenVideo => &mut self.screen,
            _ => return false,
        };
        std::mem::replace(slot, publisher) != publisher
    }

    fn get(self, purpose: Purpose) -> Option<Track> {
        let publisher = match purpose {
            Purpose::Camera => self.camera?,
            Purpose::ScreenVideo => self.screen?,
            _ => return None,
        };
        Some(Track { publisher, purpose })
    }

    fn current(self, preferred: Option<Purpose>) -> Option<Track> {
        preferred
            .and_then(|purpose| self.get(purpose))
            .or_else(|| self.get(Purpose::ScreenVideo))
            .or_else(|| self.get(Purpose::Camera))
    }
}

pub(super) fn controls(
    camera_available: bool,
    screen_available: bool,
    preferred: Option<Purpose>,
    generation: u64,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    if !(camera_available && screen_available) {
        return None;
    }
    let current = preferred.unwrap_or(Purpose::ScreenVideo);
    Some(
        layout::actions()
            .id("native-call-view")
            .role(Role::Group)
            .aria_label("Video source")
            .w_auto()
            .max_w_full()
            .children(
                [
                    (
                        Purpose::Camera,
                        "native-view-camera",
                        "View camera",
                        "Viewing camera",
                    ),
                    (
                        Purpose::ScreenVideo,
                        "native-view-screen",
                        "View screen",
                        "Viewing screen",
                    ),
                ]
                .into_iter()
                .map(|(purpose, id, label, selected)| {
                    let active = current == purpose;
                    named_button(
                        id,
                        label,
                        if active { selected } else { label },
                        if active {
                            Tone::Primary
                        } else {
                            Tone::Secondary
                        },
                        move |this, _, cx| {
                            if this.native_call_generation == generation
                                && let Some(call) = &this.native_call
                                && !call.minimized
                                && call.quality.send_if_modified(|selection| {
                                    selection.view.replace(purpose) != Some(purpose)
                                })
                            {
                                cx.notify();
                            }
                        },
                        cx,
                    )
                }),
            )
            .into_any_element(),
    )
}

pub(super) struct Viewer {
    executable: PathBuf,
    sources: Sources,
    current: Option<Track>,
    decoder: Option<Decoder>,
    stats: stats::Stats,
}

impl Viewer {
    pub(super) fn new(executable: PathBuf, relay_only: bool) -> Self {
        Self {
            executable,
            sources: Sources::default(),
            current: None,
            decoder: None,
            stats: stats::Stats::new(relay_only, Instant::now()),
        }
    }

    pub(super) async fn handle_event(
        &mut self,
        media: &Media,
        display: &Display<'_>,
        event: MediaEvent,
    ) -> Result<(), String> {
        let changed = match event {
            MediaEvent::SourceAvailable { publisher, purpose } => {
                self.sources.set(purpose, Some(publisher))
            }
            MediaEvent::SourceEnded { purpose } => self.sources.set(purpose, None),
            MediaEvent::Direct { active } => {
                self.stats.direct(active);
                return Ok(());
            }
            MediaEvent::Frame {
                publisher,
                purpose,
                contiguous,
                key_frame,
                data,
                ..
            } => {
                let track = Some(Track { publisher, purpose });
                if self.current == track
                    && let Some(source) = capture_source(purpose)
                {
                    let decoder = match &mut self.decoder {
                        Some(decoder) => decoder,
                        None => self
                            .decoder
                            .insert(Decoder::start(self.executable.clone(), source).map_err(text)?),
                    };
                    if !contiguous {
                        decoder.reset();
                        if !key_frame {
                            media.request_keyframe(purpose).map_err(text)?;
                        }
                    }
                    self.stats.encoded(data.len());
                    let started = Instant::now();
                    let decoded = decoder.decode(key_frame, data).await;
                    if self.sources.current(display.quality.borrow().view) != track {
                        return Ok(());
                    }
                    match decoded {
                        Ok(Some(frame)) => {
                            self.stats
                                .frame(frame.width, frame.height, Some(started.elapsed()));
                            display.updates.frame(frame, true);
                        }
                        Ok(None) => {}
                        Err(DecodeError::Io(_) | DecodeError::Timeout | DecodeError::Busy) => {
                            display.updates.status("Waiting for a fresh video frame");
                            media.request_keyframe(purpose).map_err(text)?;
                        }
                        Err(error) => return Err(text(error)),
                    }
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        if !changed {
            return Ok(());
        }
        {
            let mut update = display
                .updates
                .update
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            update.viewer_sources = self.sources;
        }
        display.updates.ready.notify_one();
        let selection = *display.quality.borrow();
        self.selection_changed(media, display, selection).await
    }

    pub(super) async fn selection_changed(
        &mut self,
        media: &Media,
        display: &Display<'_>,
        selection: Selection,
    ) -> Result<(), String> {
        let current = self.sources.current(selection.view);
        if let Some(track) = current {
            let _ = media.set_quality(
                track.publisher,
                track.purpose,
                selection.quality,
                selection.pixels,
            );
        }
        if self.current != current {
            self.current = current;
            self.stats.clear(Instant::now());
            display.updates.stats(None);
            display
                .updates
                .clear(match current.map(|track| track.purpose) {
                    Some(Purpose::Camera) => "Waiting for camera video",
                    Some(Purpose::ScreenVideo) => "Waiting for screen video",
                    _ => "Waiting for video",
                });
            if let Some(decoder) = self.decoder.take() {
                decoder.stop().await.map_err(text)?;
            }
            if let Some(track) = current {
                media.request_keyframe(track.purpose).map_err(text)?;
            }
        }
        Ok(())
    }

    pub(super) fn report(&mut self, display: &Display<'_>) {
        if let Some(source) = self.current.and_then(|track| capture_source(track.purpose)) {
            display.report(video_kind(source), self.stats.sample(Instant::now()), None);
        }
    }

    pub(super) async fn stop(self) -> Result<(), String> {
        if let Some(decoder) = self.decoder {
            decoder.stop().await.map_err(text)?;
        }
        Ok(())
    }
}

fn capture_source(purpose: Purpose) -> Option<Source> {
    match purpose {
        Purpose::Camera => Some(Source::Camera),
        Purpose::ScreenVideo => Some(Source::Screen),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Purpose, Sources, Track};

    #[test]
    fn screen_is_featured_by_default_and_camera_can_be_selected() {
        let sources = Sources {
            screen: Some(3),
            camera: Some(3),
        };
        assert_eq!(
            sources.current(None),
            Some(Track {
                publisher: 3,
                purpose: Purpose::ScreenVideo
            })
        );
        assert_eq!(
            sources.current(Some(Purpose::Camera)),
            Some(Track {
                publisher: 3,
                purpose: Purpose::Camera
            })
        );
    }

    #[test]
    fn a_selected_source_returns_after_falling_back_to_the_other() {
        let preference = Some(Purpose::Camera);
        let mut sources = Sources {
            camera: Some(3),
            screen: Some(3),
        };
        sources.set(Purpose::Camera, None);
        assert_eq!(
            sources.current(preference),
            sources.get(Purpose::ScreenVideo)
        );
        sources.set(Purpose::Camera, Some(3));
        assert_eq!(sources.current(preference), sources.get(Purpose::Camera));
        sources.set(Purpose::Camera, None);
        sources.set(Purpose::ScreenVideo, None);
        assert_eq!(sources.current(preference), None);
    }
}
