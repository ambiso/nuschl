// SPDX-License-Identifier: EUPL-1.2
//! Linux calls on the desktop. Capture and decoding use confined video workers.

mod preview;
mod stats;

use crate::{
    dialogs::{Tone, icon_button, styled_button},
    layout,
    menus::MenuTarget,
    services::ServiceEvent,
    state::Desktop,
    theme::{
        ACCENT, BACKDROP, BORDER, DANGER, DANGER_TEXT, MAIN, SIDEBAR, TEXT, TEXT_MUTED, color, tint,
    },
};
use futures_util::{Stream, StreamExt as _, stream};
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
    pin::Pin,
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
    source_requests: mpsc::Sender<Source>,
    source_pending: Option<Source>,
    updates: Arc<Latest>,
    stop: Option<oneshot::Sender<()>>,
    minimized: bool,
    expanded: bool,
    monitor: bool,
}

pub enum Finish {
    Left,
    RoomEnded,
}

pub enum SourceChange {
    Active { source: Source, monitor: bool },
    Cancelled,
    Failed(String),
}

pub struct Start {
    crypto: CryptoHandle,
    generation: u64,
    url: String,
    host: String,
    policy: PeerPolicy,
    source: Option<Source>,
    source_requests: mpsc::Receiver<Source>,
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
    quality: watch::Sender<Selection>,
    _updates: Task<()>,
}

impl Surface {
    fn new(
        updates: Arc<Latest>,
        status: &str,
        own_video: Option<VideoKind>,
        quality: watch::Sender<Selection>,
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
            quality,
            _updates: observe,
        }
    }
}

impl Render for Surface {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        crate::images::collect(window);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap_1()
            .min_h_0()
            .w_full()
            .when(self.image.is_none(), |stage| {
                stage.items_center().justify_center()
            })
            .child(
                div()
                    .id("native-call-status")
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(gpui_kit::text!(self.status.clone())),
            )
            .when_some(self.image.clone(), |view, image| {
                let image_size = image.size(0);
                let quality = self.quality.clone();
                view.child(
                    div()
                        .id("native-call-picture")
                        .role(Role::Image)
                        .aria_label(match self.own_video {
                            Some(VideoKind::Camera) => "Your camera",
                            Some(VideoKind::Screen) => "Your screen",
                            None => "Incoming video",
                        })
                        .relative()
                        .w_full()
                        .min_h_0()
                        .flex_1()
                        .overflow_hidden()
                        .rounded(px(8.))
                        .bg(color(BACKDROP))
                        .child(
                            img(image)
                                .absolute()
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                        .when(self.own_video.is_none(), |picture| {
                            picture.child(
                                canvas(
                                    |_, _, _| (),
                                    move |bounds, (), window, _| {
                                        let fitted =
                                            ObjectFit::Contain.get_bounds(bounds, image_size);
                                        let shorter_side =
                                            fitted.size.width.min(fitted.size.height);
                                        let pixels = (f32::from(shorter_side)
                                            * window.scale_factor())
                                            as u32;
                                        quality.send_if_modified(|selection| {
                                            std::mem::replace(&mut selection.pixels, pixels)
                                                != pixels
                                        });
                                    },
                                )
                                .absolute()
                                .size_full(),
                            )
                        }),
                )
            })
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
        let status = source.map_or("Joining video call", portal_prompt);
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
        let (request_source, source_requests) = mpsc::channel(1);
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
            surface: cx
                .new(|cx| Surface::new(updates.clone(), status, own_video, selection.clone(), cx)),
            quality: selection,
            quality_open: false,
            stats_open: false,
            own_video: None,
            source_requests: request_source,
            source_pending: None,
            updates: updates.clone(),
            stop: Some(stop),
            minimized: false,
            expanded: false,
            monitor: false,
        });
        self.services.native_call(Start {
            crypto: self.driver.call_crypto(),
            generation,
            url,
            host: link.host().to_owned(),
            policy,
            source,
            source_requests,
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

    fn choose_native_source(&mut self, source: Source, cx: &mut Context<Self>) {
        if let Some(call) = &mut self.native_call
            && call.stop.is_some()
            && call.source_pending.is_none()
            && call
                .own_video
                .is_some_and(|kind| kind != video_kind(source))
            && call.source_requests.try_send(source).is_ok()
        {
            call.source_pending = Some(source);
            cx.notify();
        }
    }

    pub fn native_source_changed(
        &mut self,
        generation: u64,
        change: SourceChange,
        cx: &mut Context<Self>,
    ) {
        let Some(call) = self
            .native_call
            .as_mut()
            .filter(|call| generation == self.native_call_generation && call.stop.is_some())
        else {
            return;
        };
        call.source_pending = None;
        match change {
            SourceChange::Active { source, monitor } => {
                let kind = video_kind(source);
                call.monitor = monitor;
                call.own_video = Some(kind);
                call.surface.update(cx, |surface, cx| {
                    surface.own_video = Some(kind);
                    cx.notify();
                });
            }
            SourceChange::Cancelled => {}
            SourceChange::Failed(error) => {
                if let Some(server) = self.client.servers.iter_mut().find(|server| {
                    server.id == call.server && server.generation == call.voice_generation
                }) {
                    server.set_notice(format!("Could not switch video source. {error}"));
                }
            }
        }
        cx.notify();
    }

    pub fn native_finished(&mut self, generation: u64, result: Result<Finish, String>) {
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
            match result {
                Err(error) => server.set_notice(format!("Video unavailable. {error}")),
                Ok(Finish::RoomEnded) if !call.owner => server
                    .calls
                    .retain(|card| card.link != call.room || card.target != call.conversation),
                Ok(_) => {}
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
        .relative()
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
        .when_some(panel(this, cx), |cover, panel| cover.child(panel))
        .when_some(crate::menus::menu(this, cx), |cover, menu| {
            cover.child(menu)
        });
    Some(crate::chrome::frame(window, cover.into_any_element()))
}

pub fn panel(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if call.minimized {
        return None;
    }
    let expanded = call.expanded;
    let generation = this.native_call_generation;
    let surface = call.surface.read(cx);
    let has_picture = surface.image.is_some();
    let stats = surface.stats.clone();
    let kind = call
        .own_video
        .or_else(|| stats.as_ref().map(|stats| stats.kind));
    let quality = call.quality.borrow().quality;
    let actions = layout::actions()
        .id("native-call-toolbar")
        .role(Role::Toolbar)
        .aria_label("Video call controls")
        .items_center()
        .justify_center()
        .w_auto()
        .max_w_full()
        .mx_auto()
        .rounded(px(8.))
        .bg(color(SIDEBAR))
        .px_2()
        .py_1()
        .children(source_controls(call, cx))
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
        .child(icon_button(
            "native-call-expand",
            if expanded {
                "fullscreen-exit"
            } else {
                "fullscreen"
            },
            if expanded {
                "Exit full screen"
            } else {
                "Full screen"
            },
            TEXT,
            None,
            move |this, window, cx| this.set_call_expanded(!expanded, window, cx),
            cx,
        ))
        .child(icon_button(
            "native-call-minimize",
            "window-minimize",
            "Minimize call",
            TEXT,
            None,
            |this, window, cx| this.minimize_native_call(window, cx),
            cx,
        ))
        .child(icon_button(
            "native-call-options",
            "more",
            "Video options",
            TEXT,
            None,
            move |this, window, cx| this.open_video_menu(generation, window.mouse_position(), cx),
            cx,
        ))
        .child(stop_button(call, cx));
    Some(
        div()
            .id("native-call")
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .when(expanded, |panel| panel.flex_1().p_3())
            .when(!expanded, |panel| {
                panel
                    .max_h(relative(0.5))
                    .px_3()
                    .py_2()
                    .when(has_picture, |panel| panel.h(relative(0.5)))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_video_menu(generation, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .gap_1()
            .border_b_1()
            .border_color(color(BORDER))
            .bg(color(MAIN))
            .when_some(call.source_pending, |panel, source| {
                panel.child(layout::text(
                    "native-call-source-pending",
                    portal_prompt(source),
                ))
            })
            .when(call.monitor, |panel| {
                panel.child(layout::text(
                    "native-call-monitor-warning",
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
            .child(actions)
            .into_any_element(),
    )
}

impl Desktop {
    fn open_video_menu(
        &mut self,
        generation: u64,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(call) = self
            .native_call
            .as_ref()
            .filter(|call| self.native_call_generation == generation && !call.minimized)
        else {
            return;
        };
        if let Some(index) = self
            .client
            .servers
            .iter()
            .position(|server| server.id == call.server)
        {
            self.make_menu(index, MenuTarget::VideoStats(generation), position);
            cx.notify();
        }
    }

    pub(crate) fn video_stats_command(&self, generation: u64) -> Option<&'static str> {
        let call = self.native_call.as_ref()?;
        (self.native_call_generation == generation && !call.minimized).then_some(
            if call.stats_open {
                "Hide video stats"
            } else {
                "Video stats"
            },
        )
    }

    pub(crate) fn toggle_video_stats(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.native_call_generation == generation
            && let Some(call) = &mut self.native_call
        {
            call.stats_open = !call.stats_open;
            call.quality_open = false;
            cx.notify();
        }
    }
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
        .role(Role::Group)
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

fn source_controls(call: &Active, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    let Some(active) = call.own_video else {
        return Vec::new();
    };
    let choose = call.source_pending.is_none() && call.stop.is_some();
    [
        (Source::Camera, "video", "Use camera", "Camera is on"),
        (Source::Screen, "screen", "Share screen", "Sharing screen"),
    ]
    .into_iter()
    .map(|(source, icon, action, status)| {
        let enabled = active == video_kind(source);
        if choose && !enabled {
            return icon_button(
                ("native-call-source", icon),
                icon,
                action,
                TEXT,
                None,
                move |this, _, cx| this.choose_native_source(source, cx),
                cx,
            );
        }
        div()
            .id(("native-call-source", icon))
            .role(Role::Image)
            .aria_label(if enabled { status } else { action })
            .size(px(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .when(enabled, |icon| icon.bg(tint(ACCENT, 0x35)))
            .child(crate::assets::icon(
                icon,
                18.,
                color(if enabled { ACCENT } else { TEXT_MUTED }),
            ))
            .into_any_element()
    })
    .collect()
}

fn portal_prompt(source: Source) -> &'static str {
    match source {
        Source::Camera => "Waiting for camera permission",
        Source::Screen => "Choose a screen or window",
    }
}

fn purpose(source: Source) -> Purpose {
    match source {
        Source::Camera => Purpose::Camera,
        Source::Screen => Purpose::ScreenVideo,
    }
}

fn stop_button(call: &Active, cx: &mut Context<Desktop>) -> impl IntoElement {
    icon_button(
        "native-call-stop",
        "hang-up",
        if call.owner {
            "End call"
        } else {
            "Leave video"
        },
        DANGER_TEXT,
        Some(DANGER),
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
    let result = drive(start, &mut settings, &events).await;
    let _ = events
        .send(ServiceEvent::NativeFinished { generation, result })
        .await;
}

async fn drive(
    start: Start,
    settings: &mut watch::Receiver<ClientSettings>,
    events: &mpsc::Sender<ServiceEvent>,
) -> Result<Finish, String> {
    let Start {
        crypto,
        generation,
        url,
        host,
        policy,
        source,
        source_requests,
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
        () = &mut cancel => return Ok(Finish::Left),
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
            SourceChanges { generation, events },
            source_requests,
            cancel,
        )
        .await
        .map(|()| Finish::Left)
    } else {
        display.updates.status("Waiting for video");
        receive_video(&mut media, video_worker, &mut display, cancel).await
    };
    let closed = media.stop().await.map_err(text);
    result.and_then(|finish| closed.map(|()| finish))
}

#[derive(Clone, Copy)]
struct SourceChanges<'a> {
    generation: u64,
    events: &'a mpsc::Sender<ServiceEvent>,
}

impl SourceChanges<'_> {
    async fn report(self, change: SourceChange) -> Result<(), String> {
        self.events
            .send(ServiceEvent::NativeSourceChanged {
                generation: self.generation,
                change,
            })
            .await
            .map_err(text)
    }
}

async fn publish_video(
    media: &mut Media,
    mut grant: portal::Grant,
    worker: PathBuf,
    display: &mut Display,
    changes: SourceChanges<'_>,
    mut source_requests: mpsc::Receiver<Source>,
    mut cancel: Pin<&mut impl Future<Output = ()>>,
) -> Result<(), String> {
    loop {
        let source = grant.source();
        let monitor = grant.can_include_app();
        let encoding = Encoding {
            kind: video_kind(source),
            remote: None,
            bitrate_estimate: None,
            software: false,
        };
        let initial = encoding.ceiling(display.quality.borrow_and_update().quality);
        let starting = async {
            changes
                .report(SourceChange::Active { source, monitor })
                .await?;
            grant
                .start_with_limits(worker.clone(), initial)
                .map_err(text)
        };
        let mut capture = tokio::select! {
            biased;
            () = &mut cancel => return Ok(()),
            capture = starting => capture?,
        };
        let updates = display.updates.clone();
        let mut preview = preview::Preview::start(worker.clone(), source, move |frame| {
            updates.frame(frame, false)
        })
        .map_err(|error| tracing::debug!(%error, "local video preview unavailable"))
        .ok();
        let status = match source {
            Source::Screen => "Your screen · waiting for a viewer",
            Source::Camera => "Your camera · waiting for a viewer",
        };
        display.updates.clear(status);
        display.updates.stats(None);
        display.stats.clear(Instant::now());

        let result = {
            let choices = stream::unfold(&mut source_requests, |requests| async move {
                loop {
                    let source = requests.recv().await?;
                    let change = match portal::request(source, None).await {
                        Ok(grant) => return Some((Ok(grant), requests)),
                        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
                            SourceChange::Cancelled
                        }
                        Err(error) => SourceChange::Failed(text(error)),
                    };
                    if let Err(error) = changes.report(change).await {
                        return Some((Err(error), requests));
                    }
                }
            });
            tokio::pin!(choices);
            tokio::select! {
                biased;
                () = &mut cancel => Ok(None),
                result = send_video(
                    media, &mut capture, &mut preview, encoding, initial, display, choices.as_mut(),
                ) => result,
            }
        };
        let closed = capture.stop().await.map_err(text);
        if let Some(preview) = preview
            && let Err(error) = preview.stop().await
        {
            tracing::debug!(%error, "could not stop the local preview");
        }
        closed?;
        let Some(next) = result? else {
            return Ok(());
        };
        media.switch_source(purpose(next.source())).map_err(text)?;
        grant = next;
    }
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
        updates.status(portal_prompt(source));
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
    let publish = source.map(purpose);
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

    fn limit_note(&self, quality: Quality, effective: CaptureLimits) -> Option<String> {
        let local = self.preferred(quality);
        let ceiling = self.ceiling(quality);
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
            && estimate < ceiling.bitrate
        {
            notes.push(format!(
                "Estimated video budget is {:.2} Mb/s. Target video is {}p at up to {} fps.",
                f64::from(estimate) / 1_000_000.,
                effective.height,
                effective.fps,
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
    mut choices: Pin<&mut impl Stream<Item = Result<portal::Grant, String>>>,
) -> Result<Option<portal::Grant>, String> {
    let mut ready = false;
    let mut sent = false;
    let mut timer = tokio::time::interval(stats::INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let mut retune = false;
        let mut report = false;
        tokio::select! {
            choice = choices.next() => return choice.transpose(),
            event = media.next() => match call_event(event)? {
                Some(MediaEvent::SendReady) => { ready = true; capture.request_keyframe(); }
                Some(MediaEvent::KeyframeRequired) => capture.request_keyframe(),
                Some(MediaEvent::EncoderLimits { height, fps, bitrate }) => {
                    encoding.remote = Some(CaptureLimits {
                        height,
                        fps: fps.into(),
                        bitrate: video_rate(bitrate),
                    });
                    retune = true;
                }
                Some(MediaEvent::BandwidthEstimate { bitrate }) => {
                    encoding.bitrate_estimate = Some((bitrate.saturating_mul(4) / 5).min(50_000_000) as u32);
                }
                Some(MediaEvent::Direct { active }) => {
                    display.stats.direct(active);
                    encoding.bitrate_estimate = None;
                    retune = true;
                }
                Some(_) => {}
                None => return Ok(None),
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
            _ = timer.tick() => { retune = true; report = true; }
            Ok(()) = display.quality.changed() => { retune = true; }
        }
        if retune {
            let quality = display.quality.borrow_and_update().quality;
            let next = encoding.limits(quality, applied);
            if next != applied {
                capture.set_limits(next).map_err(text)?;
                applied = next;
            }
            if report {
                display.report(encoding.kind, encoding.limit_note(quality, applied));
            }
        }
    }
}

async fn receive_video(
    media: &mut Media,
    executable: PathBuf,
    display: &mut Display,
    cancel: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<Finish, String> {
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
                if !key_frame {
                    media.request_keyframe(purpose).map_err(text)?;
                }
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
                    media.request_keyframe(purpose).map_err(text)?;
                }
                Err(error) => return Err(text(error)),
            }
        }
        Ok(Finish::RoomEnded)
    };
    let result = tokio::select! {
        biased;
        () = cancel => Ok(Finish::Left),
        result = receive => result,
    };
    if let Some((_, decoder)) = decoder {
        let closed = decoder.stop().await.map_err(text);
        result.and_then(|finish| closed.map(|()| finish))
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
            software: false,
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
