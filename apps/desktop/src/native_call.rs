// SPDX-License-Identifier: EUPL-1.2
//! Linux calls on the desktop. Capture and decoding use confined video workers.

mod preview;
mod stats;

use crate::{
    dialogs::{Tone, styled_button},
    layout,
    services::ServiceEvent,
    state::Desktop,
    theme::{BORDER, MAIN, TEXT, TEXT_MUTED, color},
};
use gpui_kit::{prelude::*, *};
use nuschl_client::native::sfu::{
    Connect, Connection, Media, MediaEvent, MediaOptions, Purpose, Quality, VideoKind,
};
use nuschl_core::{model::TextTarget, network::PeerPolicy};
use nuschl_ipc::video::{
    Capture, CaptureLimits, DecodeError, DecodedFrame, Decoder, Source, portal,
};
use nuschl_protocol::sfu::Codec;
use nuschl_session::{config::ClientSettings, identity::CryptoHandle};
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

const SFU_PORT: u16 = 3479;
static WORKERS: LazyLock<Option<(PathBuf, PathBuf)>> = LazyLock::new(|| {
    let rtc = nuschl_sandbox::sibling("nuschl-rtc-worker").ok()?;
    let video = nuschl_sandbox::sibling("nuschl-video-worker").ok()?;
    (rtc.is_file() && video.is_file()).then_some((rtc, video))
});

pub struct Active {
    owner: bool,
    policy: PeerPolicy,
    room: String,
    server: String,
    conversation: TextTarget,
    voice_generation: u64,
    surface: Entity<Surface>,
    quality: watch::Sender<Selection>,
    quality_open: bool,
    stats_open: bool,
    own_video: Option<VideoKind>,
    updates: Arc<Latest>,
    stop: Option<oneshot::Sender<()>>,
    minimized: bool,
    expanded: bool,
    monitor: bool,
    confirm_monitor: Option<oneshot::Sender<()>>,
}

pub struct Start {
    crypto: CryptoHandle,
    generation: u64,
    url: String,
    host: String,
    policy: PeerPolicy,
    source: Option<Source>,
    updates: Arc<Latest>,
    quality: watch::Receiver<Selection>,
    stop: oneshot::Receiver<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Selection {
    quality: Quality,
    pixels: u32,
}

struct VideoStats {
    kind: VideoKind,
    sample: stats::Snapshot,
    limits: Option<String>,
}

struct Display {
    updates: Arc<Latest>,
    quality: watch::Receiver<Selection>,
    stats: stats::Stats,
}

impl Display {
    fn report(&mut self, kind: VideoKind, limits: Option<String>) {
        self.updates.stats(Some(Arc::new(VideoStats {
            kind,
            sample: self.stats.sample(Instant::now()),
            limits,
        })));
    }
}

struct Update {
    status: String,
    frame: Option<DecodedFrame>,
    stats: Option<Arc<VideoStats>>,
    visible: bool,
    clear: bool,
}

struct Latest {
    update: Mutex<Update>,
    ready: Notify,
}

impl Latest {
    fn show_video(&self, visible: bool) {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .visible = visible;
        self.ready.notify_one();
    }

    fn status(&self, status: impl Into<String>) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update.status = status.into();
        drop(update);
        self.ready.notify_one();
    }

    fn clear(&self, status: impl Into<String>) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update.status = status.into();
        update.frame = None;
        update.clear = true;
        drop(update);
        self.ready.notify_one();
    }

    fn visible(&self) -> bool {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .visible
    }

    fn stats(&self, stats: Option<Arc<VideoStats>>) {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats = stats;
        self.ready.notify_one();
    }

    fn frame(&self, frame: DecodedFrame, watching: bool) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if watching && update.status != "Watching video" {
            update.status = "Watching video".into();
        }
        update.frame = Some(frame);
        drop(update);
        self.ready.notify_one();
    }
}

struct Surface {
    status: String,
    image: Option<Arc<RenderImage>>,
    stats: Option<Arc<VideoStats>>,
    own_video: Option<VideoKind>,
    expanded: bool,
    _updates: Task<()>,
}

impl Surface {
    fn new(
        updates: Arc<Latest>,
        status: &str,
        own_video: Option<VideoKind>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.spawn(async move |this, cx| {
            loop {
                updates.ready.notified().await;
                let (status, frame, clear, stats) = {
                    let mut update = updates
                        .update
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    (
                        update.status.clone(),
                        if update.visible {
                            update.frame.take()
                        } else {
                            None
                        },
                        std::mem::take(&mut update.clear),
                        update.stats.clone(),
                    )
                };
                if this
                    .update(cx, |this, cx| {
                        this.status = status;
                        this.stats = stats;
                        if let Some(frame) = frame.and_then(render_frame) {
                            this.image = Some(frame);
                        } else if clear {
                            this.image = None;
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            status: status.into(),
            image: None,
            stats: None,
            own_video,
            expanded: false,
            _updates: observe,
        }
    }
}

impl Render for Surface {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        crate::images::collect(window);
        let height = picture_height(window, self.expanded);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_h_0()
            .w_full()
            .when(self.expanded, |surface| surface.flex_1())
            .child(
                div()
                    .id("native-call-status")
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(gpui_kit::text!(self.status.clone())),
            )
            .when_some(self.image.clone(), |view, image| {
                view.child(
                    div()
                        .id("native-call-picture")
                        .aria_label(match self.own_video {
                            Some(VideoKind::Camera) => "Your camera",
                            Some(VideoKind::Screen) => "Your screen",
                            None => "Incoming video",
                        })
                        .w_full()
                        .min_h_0()
                        .when(self.expanded, |picture| picture.flex_1())
                        .when(!self.expanded, |picture| picture.h(height))
                        .bg(rgb(0x08090c))
                        .child(img(image).size_full().object_fit(ObjectFit::Contain)),
                )
            })
    }
}

fn picture_height(window: &Window, expanded: bool) -> Pixels {
    let viewport = f32::from(window.viewport_size().height);
    if expanded {
        px((viewport - 64.).max(0.))
    } else {
        px((viewport * 0.36).min(400.))
    }
}

fn render_frame(frame: DecodedFrame) -> Option<Arc<RenderImage>> {
    let (width, height) = (u32::from(frame.width), u32::from(frame.height));
    let image = image::RgbaImage::from_raw(width, height, frame.bgra)?;
    Some(crate::images::track(Arc::new(RenderImage::new(vec![
        image::Frame::new(image),
    ]))))
}

impl Desktop {
    pub fn native_available(&self, host: &str) -> bool {
        WORKERS.is_some()
            && self.call_host().eq_ignore_ascii_case(host)
            && !matches!(host.split(':').next(), Some("localhost" | "127.0.0.1"))
    }

    pub fn native_call_in(&self, server: &str, target: &TextTarget) -> bool {
        self.native_call
            .as_ref()
            .is_some_and(|call| call.server == server && call.conversation == *target)
    }

    pub fn open_native_call(
        &mut self,
        server: String,
        conversation: TextTarget,
        link: &nuschl_stream::Link,
        source: Option<Source>,
        cx: &mut Context<Self>,
    ) {
        if self.native_call.is_some() || !self.native_available(link.host()) {
            return;
        }
        let Some((server_index, channel_index)) = self.selected_indices() else {
            return;
        };
        let selected = &self.client.servers[server_index];
        if selected.id != server || selected.channels[channel_index].target != conversation {
            return;
        }
        let Some(generation) = self.native_call_generation.checked_add(1) else {
            return;
        };
        let voice_generation = selected.generation;
        let owner = link.publisher().is_some();
        let room = link.url();
        let url = if owner {
            let Some(url) = link.creator_url() else {
                return;
            };
            url
        } else {
            room.clone()
        };
        let own_video = source.map(video_kind);
        let status = match source {
            Some(Source::Camera) => "Waiting for camera permission",
            Some(Source::Screen) => "Choose a screen or window",
            None => "Joining video call",
        };
        let updates = Arc::new(Latest {
            update: Mutex::new(Update {
                status: status.into(),
                frame: None,
                stats: None,
                visible: true,
                clear: false,
            }),
            ready: Notify::new(),
        });
        let (stop, stopped) = oneshot::channel();
        let (selection, quality) = watch::channel(Selection {
            quality: Quality::Auto,
            pixels: 0,
        });
        let policy = PeerPolicy {
            relay_only: self.settings.relay_only,
        };
        self.native_call_generation = generation;
        self.native_call = Some(Active {
            owner,
            policy,
            room,
            server,
            conversation,
            voice_generation,
            surface: cx.new(|cx| Surface::new(updates.clone(), status, own_video, cx)),
            quality: selection,
            quality_open: false,
            stats_open: false,
            own_video,
            updates: updates.clone(),
            stop: Some(stop),
            minimized: false,
            expanded: false,
            monitor: false,
            confirm_monitor: None,
        });
        self.services.native_call(Start {
            crypto: self.driver.call_crypto(),
            generation,
            url,
            host: link.host().to_owned(),
            policy,
            source,
            updates,
            quality,
            stop: stopped,
        });
    }

    pub fn start_native_call(
        &mut self,
        index: usize,
        target: TextTarget,
        source: Source,
        cx: &mut Context<Self>,
    ) {
        if self.native_call.is_some() || !self.native_available(self.call_host()) {
            return;
        }
        match self.create_call(index, target.clone()) {
            Ok(link) => {
                let server = self.client.servers[index].id.clone();
                self.open_native_call(server, target, &link, Some(source), cx);
                self.announce_call(link);
            }
            Err(error) => self.client.servers[index].set_notice(error),
        }
    }

    fn stop_native(&mut self) {
        if let Some(call) = &mut self.native_call
            && let Some(stop) = call.stop.take()
        {
            let _ = stop.send(());
        }
    }

    pub fn stop_native_owner(&mut self) {
        if self.native_call.as_ref().is_some_and(|call| call.owner) {
            self.stop_native();
        }
    }

    pub fn sync_native_call(&mut self) {
        let Some(call) = &self.native_call else {
            return;
        };
        let current = self
            .client
            .servers
            .iter()
            .enumerate()
            .any(|(index, server)| {
                server.status == nuschl_client::server::Status::Connected
                    && server.id == call.server
                    && server.generation == call.voice_generation
                    && if call.owner {
                        self.client
                            .own_call()
                            .is_some_and(|own| own.link == call.room)
                    } else {
                        self.client
                            .calls_in(index, &call.conversation)
                            .iter()
                            .any(|announced| announced.link == call.room)
                    }
            });
        if !current || (self.settings.relay_only && !call.policy.relay_only) {
            self.stop_native();
        }
    }

    fn set_call_expanded(&mut self, expanded: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(call) = &mut self.native_call else {
            return;
        };
        call.expanded = expanded;
        call.surface.update(cx, |surface, cx| {
            surface.expanded = expanded;
            cx.notify();
        });
        if expanded {
            window.focus(&self.window_focus, cx);
            if !window.is_fullscreen() && self.native_call_fullscreen.is_none() {
                self.native_call_fullscreen = Some(Instant::now());
                window.toggle_fullscreen();
            }
        } else {
            self.restore_call_window(window);
        }
        cx.notify();
    }

    fn restore_call_window(&mut self, window: &Window) {
        if let Some(started) = self.native_call_fullscreen {
            if window.is_fullscreen() {
                self.native_call_fullscreen = None;
                window.toggle_fullscreen();
            } else if started.elapsed() >= Duration::from_secs(2) {
                self.native_call_fullscreen = None;
            }
        }
    }

    fn minimize_native_call(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_call_expanded(false, window, cx);
        if let Some(call) = &mut self.native_call {
            call.minimized = true;
            call.updates.show_video(false);
            cx.notify();
        }
    }

    fn restore_native_call(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected_indices();
        let Some(call) = &mut self.native_call else {
            return;
        };
        if let Some((index, server)) = self
            .client
            .servers
            .iter()
            .enumerate()
            .find(|(_, server)| server.id == call.server)
            && let Some((channel_index, channel)) = server
                .channels
                .iter()
                .enumerate()
                .find(|(_, channel)| channel.target == call.conversation)
        {
            call.minimized = false;
            call.updates.show_video(true);
            if selected != Some((index, channel_index)) {
                self.browse_channel(index, channel.id, window, cx);
            }
            cx.notify();
        }
    }

    pub fn confirm_monitor_share(&mut self, generation: u64, ready: oneshot::Sender<()>) {
        if generation == self.native_call_generation
            && let Some(call) = &mut self.native_call
            && call.stop.is_some()
        {
            call.minimized = false;
            call.updates.show_video(true);
            call.confirm_monitor = Some(ready);
        }
    }

    pub fn native_finished(&mut self, generation: u64, error: Option<String>) {
        if generation != self.native_call_generation {
            return;
        }
        let Some(call) = self.native_call.take() else {
            return;
        };
        if call.owner
            && self
                .client
                .own_call()
                .is_some_and(|own| own.link == call.room)
        {
            self.end_call();
        }
        if call.stop.is_some()
            && let Some(server) = self.client.servers.iter_mut().find(|server| {
                server.id == call.server && server.generation == call.voice_generation
            })
        {
            if let Some(error) = error {
                server.set_notice(format!("Video unavailable. {error}"));
            } else if !call.owner {
                server
                    .calls
                    .retain(|card| card.link != call.room || card.target != call.conversation);
            }
        }
    }
}

/// The user can focus video on demand. The ordinary view keeps the chat visible.
pub fn expanded(
    this: &mut Desktop,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    if !this.native_call.as_ref().is_some_and(|call| call.expanded) {
        this.restore_call_window(window);
        return None;
    }
    let cover = div()
        .id("expanded-video-call")
        .size_full()
        .flex()
        .flex_col()
        .text_color(color(TEXT))
        .bg(color(MAIN))
        .track_focus(&this.window_focus)
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "escape" {
                this.set_call_expanded(false, window, cx);
                cx.stop_propagation();
            }
        }))
        .when_some(panel(this, window, cx), |cover, panel| cover.child(panel));
    Some(crate::chrome::frame(window, cover.into_any_element()))
}

pub fn panel(this: &Desktop, window: &Window, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if call.minimized {
        return None;
    }
    let expanded = call.expanded;
    let stats = call.surface.read(cx).stats.clone();
    let kind = call
        .own_video
        .or_else(|| stats.as_ref().map(|stats| stats.kind));
    let pixels = (f32::from(picture_height(window, expanded)) * window.scale_factor()) as u32;
    call.quality
        .send_if_modified(|selection| std::mem::replace(&mut selection.pixels, pixels) != pixels);
    let quality = call.quality.borrow().quality;
    let actions = layout::actions()
        .when(call.confirm_monitor.is_some(), |actions| {
            actions.child(styled_button(
                "native-call-confirm-monitor",
                "Share entire monitor",
                Tone::Primary,
                |this, _, cx| {
                    if let Some(call) = &mut this.native_call
                        && let Some(ready) = call.confirm_monitor.take()
                    {
                        let _ = ready.send(());
                        call.monitor = true;
                        cx.notify();
                    }
                },
                cx,
            ))
        })
        .child(styled_button(
            "native-call-expand",
            if expanded {
                "Exit full screen"
            } else {
                "Full screen"
            },
            Tone::Secondary,
            move |this, window, cx| this.set_call_expanded(!expanded, window, cx),
            cx,
        ))
        .child(styled_button(
            "native-call-minimize",
            "Minimize call",
            Tone::Secondary,
            |this, window, cx| this.minimize_native_call(window, cx),
            cx,
        ))
        .when_some(kind, |actions, kind| {
            actions.child(styled_button(
                "native-call-quality",
                format!("Quality · {}", quality_label(kind, quality)),
                Tone::Secondary,
                |this, _, cx| {
                    if let Some(call) = &mut this.native_call {
                        call.quality_open = !call.quality_open;
                        call.stats_open = false;
                        cx.notify();
                    }
                },
                cx,
            ))
        })
        .child(styled_button(
            "native-call-toggle-stats",
            if call.stats_open {
                "Hide video stats"
            } else {
                "Video stats"
            },
            Tone::Secondary,
            |this, _, cx| {
                if let Some(call) = &mut this.native_call {
                    call.stats_open = !call.stats_open;
                    call.quality_open = false;
                    cx.notify();
                }
            },
            cx,
        ))
        .child(stop_button(call, cx));
    Some(
        div()
            .id("native-call")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .min_w_0()
            .min_h_0()
            .when(expanded, |panel| panel.flex_1().p_3())
            .when(!expanded, |panel| panel.px_3().py_2())
            .gap_1()
            .border_b_1()
            .border_color(color(BORDER))
            .bg(color(MAIN))
            .child(actions)
            .when(call.confirm_monitor.is_some() || call.monitor, |panel| {
                panel.child(div().text_sm().child(
                    "Anyone watching can see the Nuschl window while it’s on the shared monitor.",
                ))
            })
            .when_some(kind.filter(|_| call.quality_open), |panel, kind| {
                panel.child(quality_choices(kind, quality, cx))
            })
            .when(call.stats_open, |panel| {
                panel.child(video_stats(
                    stats.as_deref(),
                    call.own_video.is_some(),
                    quality,
                ))
            })
            .child(call.surface.clone())
            .into_any_element(),
    )
}

fn quality_label(kind: VideoKind, quality: Quality) -> String {
    match quality {
        Quality::Auto => "Auto".into(),
        Quality::Tier(_) => {
            let tier = kind.ladder()[kind.request(quality, 0)];
            format!("{}p{}", tier.height, tier.fps)
        }
    }
}

fn quality_choices(
    kind: VideoKind,
    selected: Quality,
    cx: &mut Context<Desktop>,
) -> impl IntoElement {
    layout::actions().children(
        std::iter::once(Quality::Auto)
            .chain((0..=kind.top() as u8).map(Quality::Tier))
            .enumerate()
            .map(|(index, quality)| {
                styled_button(
                    ("native-video-quality", index),
                    quality_label(kind, quality),
                    if quality == selected {
                        Tone::Primary
                    } else {
                        Tone::Secondary
                    },
                    move |this, _, cx| {
                        if let Some(call) = &mut this.native_call {
                            call.quality
                                .send_modify(|selection| selection.quality = quality);
                            call.quality_open = false;
                            call.stats_open = true;
                            cx.notify();
                        }
                    },
                    cx,
                )
            }),
    )
}

fn video_stats(stats: Option<&VideoStats>, owned: bool, quality: Quality) -> impl IntoElement {
    let body = div()
        .id("native-call-video-stats")
        .aria_label("Video stats")
        .flex()
        .flex_col()
        .gap_1()
        .text_sm()
        .text_color(color(TEXT_MUTED));
    let Some(stats) = stats else {
        return body.child("Waiting for video stats");
    };
    let below_selected = matches!(quality, Quality::Tier(_))
        && stats.sample.resolution().is_some_and(|(width, height)| {
            width.min(height) < stats.kind.ladder()[stats.kind.request(quality, 0)].height
        });
    body.child(
        div().flex().flex_wrap().gap_3().children(
            stats
                .sample
                .rows(owned)
                .map(|(label, value)| div().child(format!("{label} {value}"))),
        ),
    )
    .when_some(stats.limits.as_ref(), |body, limits| {
        body.child(limits.clone())
    })
    .when(below_selected, |body| {
        body.child(
            "The selected tier is a ceiling. Source and connection limits can reduce resolution.",
        )
    })
}

fn video_kind(source: Source) -> VideoKind {
    match source {
        Source::Camera => VideoKind::Camera,
        Source::Screen => VideoKind::Screen,
    }
}

fn stop_button(call: &Active, cx: &mut Context<Desktop>) -> impl IntoElement {
    styled_button(
        "native-call-stop",
        if call.owner {
            "End call"
        } else {
            "Leave video"
        },
        Tone::Danger,
        |this, _, cx| {
            this.stop_native();
            cx.notify();
        },
        cx,
    )
}

pub fn minimized(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if !call.minimized
        && this.selected_indices().is_some_and(|(index, channel)| {
            let server = &this.client.servers[index];
            this.native_call_in(&server.id, &server.channels[channel].target)
        })
    {
        return None;
    }
    Some(
        div()
            .id("native-call-minimized")
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(color(BORDER))
            .when(call.monitor, |bar| bar.child("Sharing entire monitor"))
            .child(styled_button(
                "native-call-restore",
                "Return to call",
                Tone::Secondary,
                |this, window, cx| this.restore_native_call(window, cx),
                cx,
            ))
            .child(stop_button(call, cx))
            .into_any_element(),
    )
}

pub async fn run(
    start: Start,
    mut settings: watch::Receiver<ClientSettings>,
    events: mpsc::Sender<ServiceEvent>,
) {
    let generation = start.generation;
    let error = drive(start, &mut settings, &events).await.err();
    let _ = events
        .send(ServiceEvent::NativeFinished { generation, error })
        .await;
}

async fn drive(
    start: Start,
    settings: &mut watch::Receiver<ClientSettings>,
    events: &mpsc::Sender<ServiceEvent>,
) -> Result<(), String> {
    let Start {
        crypto,
        generation,
        url,
        host,
        policy,
        source,
        updates,
        quality,
        mut stop,
    } = start;
    let revoke = async {
        loop {
            if settings.borrow_and_update().relay_only && !policy.relay_only {
                return;
            }
            if settings.changed().await.is_err() {
                return;
            }
        }
    };
    let cancel = async {
        tokio::select! {
            _ = &mut stop => {},
            () = revoke => {},
        }
    };
    tokio::pin!(cancel);
    let options = Connect {
        generation,
        host: &host,
        link: &url,
        codecs: &[Codec::Av1],
        policy,
    };
    let (mut media, grant, video_worker) = tokio::select! {
        biased;
        () = &mut cancel => return Ok(()),
        result = prepare(crypto, options, source, &updates) => result?,
    };
    let mut display = Display {
        updates,
        quality,
        stats: stats::Stats::new(policy.relay_only, Instant::now()),
    };
    let result = if let Some(grant) = grant {
        publish_video(
            &mut media,
            grant,
            video_worker,
            &mut display,
            generation,
            events,
            cancel,
        )
        .await
    } else {
        display.updates.status("Waiting for video");
        receive_video(&mut media, video_worker, &mut display, cancel).await
    };
    result.and(media.stop().await.map_err(text))
}

async fn publish_video(
    media: &mut Media,
    grant: portal::Grant,
    worker: PathBuf,
    display: &mut Display,
    generation: u64,
    events: &mpsc::Sender<ServiceEvent>,
    mut cancel: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<(), String> {
    if grant.requires_curtain() {
        let confirm = async {
            let (ready, confirmed) = oneshot::channel();
            events
                .send(ServiceEvent::ConfirmMonitorShare { generation, ready })
                .await
                .map_err(text)?;
            confirmed.await.map_err(text)
        };
        tokio::select! {
            biased;
            () = &mut cancel => return Ok(()),
            result = confirm => result?,
        }
    }
    let source = grant.source();
    let encoding = Encoding {
        kind: video_kind(source),
        remote: None,
        bitrate_estimate: None,
        software: false,
    };
    let initial = encoding.limits(display.quality.borrow_and_update().quality);
    let mut capture = grant
        .start_with_limits(worker.clone(), initial)
        .map_err(text)?;
    let updates = display.updates.clone();
    let mut preview =
        preview::Preview::start(worker, source, move |frame| updates.frame(frame, false))
            .map_err(|error| tracing::debug!(%error, "local video preview unavailable"))
            .ok();
    display.updates.status(match source {
        Source::Screen => "Your screen · waiting for a viewer",
        Source::Camera => "Your camera · waiting for a viewer",
    });
    let result = tokio::select! {
        biased;
        () = &mut cancel => Ok(()),
        result = send_video(media, &mut capture, &mut preview, encoding, initial, display) => result,
    };
    let closed = capture.stop().await.map_err(text);
    if let Some(preview) = preview
        && let Err(error) = preview.stop().await
    {
        tracing::debug!(%error, "could not stop the local preview");
    }
    result.and(closed)
}

async fn prepare(
    crypto: CryptoHandle,
    options: Connect<'_>,
    source: Option<Source>,
    updates: &Latest,
) -> Result<(Media, Option<portal::Grant>, PathBuf), String> {
    let (executable, video_worker) = WORKERS
        .as_ref()
        .ok_or("The native video workers are unavailable.")?
        .clone();
    let mut connection = Connection::connect(crypto, options).await.map_err(text)?;
    let permission = async {
        let Some(source) = source else {
            return Ok::<_, String>(None);
        };
        updates.status(match source {
            Source::Camera => "Waiting for camera permission",
            Source::Screen => "Choose a screen or window",
        });
        portal::request(source, None).await.map(Some).map_err(text)
    };
    tokio::pin!(permission);
    let grant = loop {
        tokio::select! {
            biased;
            grant = &mut permission => break grant?,
            tick = connection.poll_idle() => tick.map_err(text)?,
        }
    };
    let source = grant.as_ref().map(portal::Grant::source);
    updates.status("Joining video call");
    let publish = source.map(|source| match source {
        Source::Camera => Purpose::Camera,
        Source::Screen => Purpose::ScreenVideo,
    });
    let media = connection
        .into_media(MediaOptions {
            executable,
            sfu_port: SFU_PORT,
            publish,
        })
        .await
        .map_err(text)?;
    Ok((media, grant, video_worker))
}

struct Encoding {
    kind: VideoKind,
    remote: Option<CaptureLimits>,
    bitrate_estimate: Option<u32>,
    software: bool,
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

    fn limits(&self, quality: Quality) -> CaptureLimits {
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
            bitrate: local
                .bitrate
                .min(remote.bitrate)
                .min(self.bitrate_estimate.unwrap_or(u32::MAX)),
        }
    }

    fn limit_note(&self, quality: Quality) -> Option<String> {
        let local = self.preferred(quality);
        let effective = self.limits(quality);
        let mut notes = Vec::new();
        if let Some(remote) = self.remote
            && (remote.height < local.height
                || remote.fps < local.fps
                || remote.bitrate < local.bitrate)
        {
            notes.push(format!(
                "The call server allows up to {}p{} at {:.1} Mb/s.",
                remote.height,
                remote.fps,
                f64::from(remote.bitrate) / 1_000_000.,
            ));
        }
        if let Some(estimate) = self.bitrate_estimate
            && effective.bitrate == estimate
            && estimate < local.bitrate
        {
            notes.push(format!(
                "Estimated upload allows {:.1} Mb/s.",
                f64::from(estimate) / 1_000_000.,
            ));
        }
        if self.software && effective.fps > 15 {
            notes.push("Software encoding runs at up to 15 fps.".into());
        }
        (!notes.is_empty()).then(|| notes.join(" "))
    }
}

async fn send_video(
    media: &mut Media,
    capture: &mut Capture,
    preview: &mut Option<preview::Preview>,
    mut encoding: Encoding,
    mut applied: CaptureLimits,
    display: &mut Display,
) -> Result<(), String> {
    let mut ready = false;
    let mut sent = false;
    let mut timer = tokio::time::interval(stats::INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = media.next() => match call_event(event)? {
                Some(MediaEvent::SendReady) => { ready = true; capture.request_keyframe(); }
                Some(MediaEvent::KeyframeRequired) => capture.request_keyframe(),
                Some(MediaEvent::EncoderLimits { height, fps, bitrate }) => {
                    encoding.remote = Some(CaptureLimits {
                        height,
                        fps: fps.into(),
                        bitrate: video_rate(bitrate),
                    });
                }
                Some(MediaEvent::BandwidthEstimate { bitrate }) => {
                    encoding.bitrate_estimate = Some(video_rate(bitrate.saturating_mul(4) / 5));
                }
                Some(MediaEvent::Direct { active }) => display.stats.direct(active),
                Some(_) => {}
                None => return Ok(()),
            },
            frame = capture.next() => {
                let frame = frame.map_err(text)?;
                encoding.software = !frame.hardware;
                display.stats.frame(frame.width, frame.height, None);
                if let Some(preview) = preview {
                    preview.offer(&frame, display.updates.visible());
                }
                if ready {
                    let ticks = frame.capture_time_us.saturating_mul(9) / 100;
                    let bytes = frame.av1.len();
                    if media.send_frame(ticks, frame.keyframe, frame.av1).map_err(text)? {
                        display.stats.encoded(bytes);
                        if !sent {
                            sent = true;
                            display.updates.status(match encoding.kind {
                                VideoKind::Camera => "Your camera",
                                VideoKind::Screen => "Your screen",
                            });
                        }
                    } else {
                        capture.request_keyframe();
                    }
                }
            }
            _ = timer.tick() => {
                let quality = display.quality.borrow().quality;
                display.report(encoding.kind, encoding.limit_note(quality));
            }
            Ok(()) = display.quality.changed() => {}
        }
        let quality = display.quality.borrow_and_update().quality;
        let next = encoding.limits(quality);
        if next != applied {
            capture.set_limits(next).map_err(text)?;
            applied = next;
        }
    }
}

async fn receive_video(
    media: &mut Media,
    executable: PathBuf,
    display: &mut Display,
    cancel: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<(), String> {
    let mut decoder = None;
    let receive = async {
        let mut selected = None;
        let mut timer = tokio::time::interval(stats::INTERVAL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let event = tokio::select! {
                event = media.next() => call_event(event)?,
                Ok(()) = display.quality.changed() => {
                    let selection = *display.quality.borrow_and_update();
                    if let Some((publisher, purpose, _)) = selected {
                        let _ = media.set_quality(publisher, purpose, selection.quality, selection.pixels);
                    }
                    continue;
                }
                _ = timer.tick() => {
                    if let Some((_, _, kind)) = selected {
                        display.report(kind, None);
                    }
                    continue;
                }
            };
            let Some(event) = event else {
                break;
            };
            let (publisher, purpose, contiguous, key_frame, data) = match event {
                MediaEvent::Frame {
                    publisher,
                    purpose,
                    contiguous,
                    key_frame,
                    data,
                    ..
                } => (publisher, purpose, contiguous, key_frame, data),
                MediaEvent::SourceEnded { purpose } => {
                    if let Some((_, session)) = decoder.take_if(|(chosen, _)| *chosen == purpose) {
                        selected = None;
                        display.stats.clear(Instant::now());
                        display.updates.stats(None);
                        display.updates.clear("Waiting for video");
                        Decoder::stop(session).await.map_err(text)?;
                    }
                    continue;
                }
                MediaEvent::Direct { active } => {
                    display.stats.direct(active);
                    continue;
                }
                _ => continue,
            };
            let source = match purpose {
                Purpose::Camera => Source::Camera,
                Purpose::ScreenVideo => Source::Screen,
                _ => continue,
            };
            let decoder = match &mut decoder {
                Some((chosen, decoder)) if *chosen == purpose => decoder,
                Some(_) => continue,
                None => {
                    selected = Some((publisher, purpose, video_kind(source)));
                    let selection = *display.quality.borrow_and_update();
                    let _ =
                        media.set_quality(publisher, purpose, selection.quality, selection.pixels);
                    &mut decoder
                        .insert((
                            purpose,
                            Decoder::start(executable.clone(), source).map_err(text)?,
                        ))
                        .1
                }
            };
            if !contiguous {
                decoder.reset();
            }
            display.stats.encoded(data.len());
            let started = Instant::now();
            match decoder.decode(key_frame, data).await {
                Ok(Some(frame)) => {
                    display
                        .stats
                        .frame(frame.width, frame.height, Some(started.elapsed()));
                    display.updates.frame(frame, true);
                }
                Ok(None) => {}
                Err(DecodeError::Io(_) | DecodeError::Timeout | DecodeError::Busy) => {
                    display.updates.status("Waiting for a fresh video frame");
                }
                Err(error) => return Err(text(error)),
            }
        }
        Ok(())
    };
    let result = tokio::select! {
        biased;
        () = cancel => Ok(()),
        result = receive => result,
    };
    if let Some((_, decoder)) = decoder {
        result.and(decoder.stop().await.map_err(text))
    } else {
        result
    }
}

fn video_rate(bitrate: u64) -> u32 {
    bitrate.clamp(64_000, 50_000_000) as u32
}
fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn call_event(
    event: Option<Result<MediaEvent, nuschl_client::native::sfu::Error>>,
) -> Result<Option<MediaEvent>, String> {
    match event.transpose() {
        Err(nuschl_client::native::sfu::Error::Refused(reason)) if reason == "ended" => Ok(None),
        result => result.map_err(text),
    }
}
