// SPDX-License-Identifier: EUPL-1.2
//! Linux calls on the desktop. Capture and decoding use confined video workers.

mod preview;
mod publisher;
mod stats;
mod viewer;

use crate::{
    dialogs::{Tone, icon_button, styled_button},
    layout,
    menus::MenuTarget,
    services::ServiceEvent,
    state::Desktop,
    theme::{
        ACCENT, BACKDROP, BORDER, DANGER, DANGER_TEXT, ELEVATED, MAIN, SELECTED, SIDEBAR, TEXT,
        TEXT_MUTED, color,
    },
};
use futures_util::{FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui_kit::{prelude::*, *};
use nuschl_client::native::sfu::{
    Connect, Connection, Media, MediaEvent, MediaOptions, Purpose, Quality, VideoKind,
};
use nuschl_core::{model::TextTarget, network::PeerPolicy};
use nuschl_ipc::video::{DecodedFrame, Source, portal};
use nuschl_protocol::sfu::Codec;
use nuschl_session::{config::ClientSettings, identity::CryptoHandle};
use std::{
    cell::Cell,
    future::pending,
    path::{Path, PathBuf},
    rc::Rc,
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
    stats_open: bool,
    local_sources: [bool; 2],
    can_publish: bool,
    source_requests: mpsc::Sender<SourceRequest>,
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
    MediaEnded,
}

#[derive(Clone, Copy)]
struct SourceRequest {
    source: Source,
    enable: bool,
}

pub enum SourceChange {
    CanPublish(bool),
    MediaClosed,
    Active { source: Source, monitor: bool },
    Stopped(Source),
    Cancelled(Source),
    Failed(Source, String),
}

pub struct Start {
    owner: bool,
    crypto: CryptoHandle,
    generation: u64,
    url: String,
    host: String,
    policy: PeerPolicy,
    source: Option<Source>,
    source_requests: mpsc::Receiver<SourceRequest>,
    updates: Arc<Latest>,
    quality: watch::Receiver<Selection>,
    stop: oneshot::Receiver<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Selection {
    quality: Quality,
    pixels: u32,
    view: Option<Purpose>,
}

struct VideoStats {
    kind: VideoKind,
    sample: stats::Snapshot,
    encoder: Option<stats::Encoder>,
}

struct Display<'a> {
    source_changes: SourceChanges<'a>,
    updates: Arc<Latest>,
    quality: watch::Receiver<Selection>,
}

impl Display<'_> {
    fn report(&self, kind: VideoKind, sample: stats::Snapshot, encoder: Option<stats::Encoder>) {
        self.updates.stats(Some(Arc::new(VideoStats {
            kind,
            sample,
            encoder,
        })));
    }
}

struct Update {
    status: String,
    frame: Option<DecodedFrame>,
    stats: Option<Arc<VideoStats>>,
    local_view: Option<(u64, VideoKind)>,
    viewer_sources: viewer::Sources,
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

    fn local_view(&self, view: Option<(u64, VideoKind, bool)>) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update.local_view = view.map(|(id, kind, _)| (id, kind));
        update.status = view
            .map_or("Waiting for video", |(_, kind, ready)| {
                publisher::label(kind, ready)
            })
            .into();
        update.frame = None;
        update.stats = None;
        update.clear = true;
        drop(update);
        self.ready.notify_one();
    }

    fn without_viewers(&self) {
        self.update
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .viewer_sources = viewer::Sources::default();
        self.ready.notify_one();
    }

    fn local_frame(&self, id: u64, frame: DecodedFrame) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if update.local_view.is_some_and(|(current, _)| current == id) && update.visible {
            update.frame = Some(frame);
            drop(update);
            self.ready.notify_one();
        }
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
    viewer_sources: viewer::Sources,
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
                let (status, frame, clear, stats, local_view, viewer_sources) = {
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
                        update.local_view,
                        update.viewer_sources,
                    )
                };
                if this
                    .update(cx, |this, cx| {
                        this.status = status;
                        this.stats = stats;
                        this.own_video = local_view.map(|(_, kind)| kind);
                        this.viewer_sources = viewer_sources;
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
            viewer_sources: viewer::Sources::default(),
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
        let status = "Joining video call";
        let updates = Arc::new(Latest {
            update: Mutex::new(Update {
                status: status.into(),
                frame: None,
                stats: None,
                local_view: None,
                viewer_sources: viewer::Sources::default(),
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
            view: None,
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
            stats_open: false,
            local_sources: [false; 2],
            can_publish: false,
            source_requests: request_source,
            source_pending: source,
            updates: updates.clone(),
            stop: Some(stop),
            minimized: false,
            expanded: false,
            monitor: false,
        });
        self.services.native_call(Start {
            owner,
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
        source: Option<Source>,
        cx: &mut Context<Self>,
    ) {
        if self.native_call.is_some() || !self.native_available(self.call_host()) {
            return;
        }
        match self.create_call(index, target.clone()) {
            Ok(link) => {
                let server = self.client.servers[index].id.clone();
                self.open_native_call(server, target, &link, source, cx);
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
        let Some(call) = self.native_call.as_mut().filter(|call| call.stop.is_some()) else {
            return;
        };
        let enable = !call.local_sources[publisher::index(source)];
        if enable && (!call.can_publish || call.source_pending.is_some()) {
            return;
        }
        if call
            .source_requests
            .try_send(SourceRequest { source, enable })
            .is_ok()
            && enable
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
        let source = match change {
            SourceChange::CanPublish(allowed) => {
                call.can_publish = allowed;
                cx.notify();
                return;
            }
            SourceChange::MediaClosed => {
                call.can_publish = false;
                call.local_sources = [false; 2];
                call.source_pending = None;
                call.monitor = false;
                cx.notify();
                return;
            }
            SourceChange::Active { source, monitor } => {
                call.local_sources[publisher::index(source)] = true;
                if source == Source::Screen {
                    call.monitor = monitor;
                }
                source
            }
            SourceChange::Stopped(source) => {
                call.local_sources[publisher::index(source)] = false;
                if source == Source::Screen {
                    call.monitor = false;
                }
                source
            }
            SourceChange::Cancelled(source) => source,
            SourceChange::Failed(source, error) => {
                if let Some(server) = self.client.servers.iter_mut().find(|server| {
                    server.id == call.server && server.generation == call.voice_generation
                }) {
                    server.set_notice(format!("Video source unavailable. {error}"));
                }
                source
            }
        };
        if call.source_pending == Some(source) {
            call.source_pending = None;
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
        .when_some(panel(this, window, cx), |cover, panel| cover.child(panel))
        .when_some(crate::menus::menu(this, cx), |cover, menu| {
            cover.child(menu)
        });
    Some(crate::chrome::frame(window, cover.into_any_element()))
}

pub fn panel(this: &Desktop, window: &mut Window, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if call.minimized {
        return None;
    }
    let expanded = call.expanded;
    let generation = this.native_call_generation;
    let surface = call.surface.read(cx);
    let has_picture = surface.image.is_some();
    let stats = surface.stats.clone();
    let owned = surface.own_video.is_some();
    let selection = *call.quality.borrow();
    let kind = surface
        .own_video
        .or_else(|| surface.viewer_sources.kind(selection.view))
        .or_else(|| stats.as_ref().map(|stats| stats.kind));
    let (camera, screen) = surface.viewer_sources.available();
    let picker = viewer::controls(
        camera || call.local_sources[publisher::index(Source::Camera)],
        screen || call.local_sources[publisher::index(Source::Screen)],
        selection.view,
        generation,
        cx,
    );
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
        .children(picker)
        .when_some(kind, |actions, kind| {
            actions.child(quality_selector(
                kind,
                selection.quality,
                stats.as_deref(),
                generation,
                this.menu
                    .as_ref()
                    .is_some_and(|menu| menu.target == MenuTarget::VideoQuality(generation, kind)),
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
            move |this, window, cx| {
                this.open_video_menu(
                    generation,
                    MenuTarget::VideoStats(generation),
                    window.mouse_position(),
                    cx,
                )
            },
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
                    this.open_video_menu(
                        generation,
                        MenuTarget::VideoStats(generation),
                        event.position,
                        cx,
                    );
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
            .when(call.stats_open && !has_picture, |panel| {
                panel.child(video_stats(stats.as_deref(), owned))
            })
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(call.surface.clone())
                    .when(call.stats_open && has_picture, |stage| {
                        let scroll_height = window.viewport_size().height;
                        stage.child(
                            layout::scroll_region(
                                "native-call-metrics-scroll",
                                "Video metrics",
                                scroll_height,
                                window,
                                cx,
                            )
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(352.))
                            .max_w_full()
                            .max_h_full()
                            .pl_2()
                            .py_2()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(color(BORDER))
                            .bg(color(MAIN))
                            .child(video_stats(stats.as_deref(), owned)),
                        )
                    }),
            )
            .child(actions)
            .into_any_element(),
    )
}

impl Desktop {
    fn open_video_menu(
        &mut self,
        generation: u64,
        target: MenuTarget,
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
            self.make_menu(index, target, position);
            cx.notify();
        }
    }

    pub(crate) fn video_quality_commands(
        &self,
        generation: u64,
        kind: VideoKind,
    ) -> impl Iterator<Item = (Quality, String, bool)> {
        let current = self
            .native_call
            .as_ref()
            .filter(|call| self.native_call_generation == generation && !call.minimized)
            .map(|call| match call.quality.borrow().quality {
                Quality::Auto => Quality::Auto,
                Quality::Tier(tier) => Quality::Tier(tier.min(kind.top() as u8)),
            });
        std::iter::once(Quality::Auto)
            .chain((0..=kind.top() as u8).map(Quality::Tier))
            .filter_map(move |quality| {
                current.map(|current| (quality, quality_label(kind, quality), current == quality))
            })
    }

    pub(crate) fn set_video_quality(
        &mut self,
        generation: u64,
        quality: Quality,
        cx: &mut Context<Self>,
    ) {
        if self.native_call_generation == generation
            && let Some(call) = self.native_call.as_ref().filter(|call| !call.minimized)
        {
            call.quality
                .send_modify(|selection| selection.quality = quality);
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
            cx.notify();
        }
    }
}

fn quality_selector(
    kind: VideoKind,
    selected: Quality,
    stats: Option<&VideoStats>,
    generation: u64,
    open: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let label: SharedString =
        format!("Quality · {}", quality_readout(kind, selected, stats)).into();
    let anchor = Rc::new(Cell::new(point(px(0.), px(0.))));
    let position = anchor.clone();
    let activate = Rc::new(
        move |this: &mut Desktop, _: &mut Window, cx: &mut Context<Desktop>| {
            this.open_video_menu(
                generation,
                MenuTarget::VideoQuality(generation, kind),
                position.get(),
                cx,
            );
        },
    );
    let click = activate.clone();
    let key = activate.clone();
    let control = div()
        .id("native-call-quality")
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_expanded(open)
        .tab_index(0)
        .w(px(224.))
        .max_w_full()
        .min_w_0()
        .flex_shrink_0()
        .px_3()
        .py(px(6.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .gap_2()
        .bg(color(if open { SELECTED } else { ELEVATED }))
        .hover(|style| style.bg(color(SELECTED)))
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color(TEXT))
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space" | "down") {
                key(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(text!(label).with_id("native-call-quality-label")),
        )
        .child(crate::assets::icon("chevron-down", 14., color(TEXT_MUTED)))
        .child(
            canvas(
                move |bounds, _, _| anchor.set(bounds.bottom_left() + point(px(0.), px(4.))),
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0(),
        );
    layout::focusable_button(
        "native-call-quality",
        control,
        crate::dialogs::accessible_click(activate, cx),
    )
}

fn quality_readout(kind: VideoKind, quality: Quality, stats: Option<&VideoStats>) -> String {
    if quality == Quality::Auto
        && let Some(stats) = stats.filter(|stats| stats.kind == kind)
    {
        if let Some(encoder) = stats.encoder {
            return format!("Auto ({}p{})", encoder.target.height, encoder.target.fps);
        }
        if let Some(height) = stats.sample.height() {
            return format!("Auto ({height}p)");
        }
    }
    quality_label(kind, quality)
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

fn video_stats(stats: Option<&VideoStats>, owned: bool) -> impl IntoElement {
    div()
        .id("native-call-video-stats")
        .role(Role::Group)
        .aria_label("Video stats")
        .flex()
        .flex_wrap()
        .gap_x_2()
        .gap_y_1()
        .min_w_0()
        .children(
            stats::rows(
                stats.map(|stats| stats.sample),
                stats.and_then(|stats| stats.encoder),
                owned,
            )
            .map(|(label, value)| {
                div()
                    .w(px(156.))
                    .max_w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(color(TEXT_MUTED))
                            .child(Text::new(label.into(), label.into())),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .child(Text::new((label, 1_usize).into(), value.into())),
                    )
            }),
        )
}

fn video_kind(source: Source) -> VideoKind {
    match source {
        Source::Camera => VideoKind::Camera,
        Source::Screen => VideoKind::Screen,
    }
}

fn source_controls(call: &Active, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    if !call.can_publish && !call.local_sources.into_iter().any(|active| active) {
        return Vec::new();
    }
    [
        (
            Source::Camera,
            "native-call-camera",
            "video",
            "Start camera",
            "Stop camera",
        ),
        (
            Source::Screen,
            "native-call-screen",
            "screen",
            "Share screen",
            "Stop sharing screen",
        ),
    ]
    .into_iter()
    .filter_map(|(source, id, icon, start, stop)| {
        let active = call.local_sources[publisher::index(source)];
        if !active && (!call.can_publish || call.source_pending.is_some()) {
            return None;
        }
        Some(icon_button(
            id,
            icon,
            if active { stop } else { start },
            if active { ACCENT } else { TEXT },
            active.then_some(ACCENT),
            move |this, _, cx| this.choose_native_source(source, cx),
            cx,
        ))
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

fn return_actions(call: &Active, cx: &mut Context<Desktop>) -> Div {
    layout::actions()
        .items_center()
        .when(call.monitor, |bar| bar.child("Sharing entire monitor"))
        .child(styled_button(
            "native-call-restore",
            "Return to call",
            Tone::Secondary,
            |this, window, cx| this.restore_native_call(window, cx),
            cx,
        ))
        .child(stop_button(call, cx))
}

pub fn minimized(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    if this.selected_indices().is_some_and(|(index, channel)| {
        let server = &this.client.servers[index];
        this.native_call_in(&server.id, &server.channels[channel].target)
    }) {
        return None;
    }
    Some(
        return_actions(call, cx)
            .id("native-call-minimized")
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(color(BORDER))
            .into_any_element(),
    )
}

pub fn call_card(this: &Desktop, index: usize, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    let link = crate::call::join_link(&call.room)?;
    let who = if call.owner {
        "You"
    } else {
        this.client
            .calls_in(index, &call.conversation)
            .into_iter()
            .find(|invite| invite.link == call.room)
            .and_then(|invite| this.client.servers[index].users.get(&invite.sender))
            .map_or("", |user| user.name.as_str())
    };
    let actions = if call.minimized {
        return_actions(call, cx)
    } else {
        layout::actions()
    };
    Some(crate::view::video_call_card(who, &link, actions, cx))
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
        owner,
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
    let (mut media, video_worker) = tokio::select! {
        biased;
        () = &mut cancel => return Ok(Finish::Left),
        result = prepare(crypto, options) => result?,
    };
    let mut display = Display {
        source_changes: SourceChanges { generation, events },
        updates,
        quality,
    };
    let mut publisher = publisher::Publisher::new(policy.relay_only);
    let mut viewer = viewer::Viewer::new(video_worker.clone(), policy.relay_only);
    let result = tokio::select! {
        biased;
        () = &mut cancel => Ok(Finish::Left),
        result = meeting(
            &mut media, &mut display, &mut publisher, &mut viewer,
            (video_worker.as_path(), source), source_requests,
        ) => result,
    };
    let (closed, (), decoded) = tokio::join!(media.stop(), publisher.stop(), viewer.stop());
    let result = result.and_then(|finish| closed.map_err(text).and(decoded).map(|()| finish));
    if owner && matches!(result, Ok(Finish::MediaEnded) | Err(_)) {
        if let Err(error) = result {
            tracing::warn!(%error, "native video stopped while the meeting remains open");
        }
        display.updates.local_view(None);
        display.updates.without_viewers();
        display
            .updates
            .status("Meeting open · browser guests can use the chat link");
        display
            .source_changes
            .report(SourceChange::MediaClosed)
            .await?;
        cancel.await;
        return Ok(Finish::Left);
    }
    result
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

type Picker = BoxFuture<'static, (Source, Result<portal::Grant, ashpd::Error>)>;

fn pick_source(source: Source) -> Picker {
    async move { (source, portal::request(source, None).await) }.boxed()
}

async fn next_grant(picker: &mut Option<Picker>) -> (Source, Result<portal::Grant, ashpd::Error>) {
    match picker {
        Some(picker) => picker.await,
        None => pending().await,
    }
}

async fn meeting(
    media: &mut Media,
    display: &mut Display<'_>,
    publisher: &mut publisher::Publisher,
    viewer: &mut viewer::Viewer,
    (worker, initial): (&Path, Option<Source>),
    mut source_requests: mpsc::Receiver<SourceRequest>,
) -> Result<Finish, String> {
    let changes = display.source_changes;
    let mut selection = *display.quality.borrow_and_update();
    let mut picker = initial.map(pick_source);
    let mut allowed = None;
    let mut timer = tokio::time::interval(stats::INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    display.updates.status("Waiting for video");
    loop {
        let can_publish = media.can_publish();
        if allowed != Some(can_publish) {
            changes
                .report(SourceChange::CanPublish(can_publish))
                .await?;
            allowed = Some(can_publish);
        }
        let [camera, screen] = &mut publisher.slots;
        let (source, frame) = tokio::select! {
            event = media.next() => {
                let event = match event {
                    Some(Ok(event)) => event,
                    Some(Err(nuschl_client::native::sfu::Error::Refused(reason))) if reason == "ended" => {
                        return Ok(Finish::RoomEnded);
                    }
                    Some(Err(error)) => return Err(text(error)),
                    None => return Ok(Finish::MediaEnded),
                };
                let retune = matches!(event, MediaEvent::SourceLimits { .. } | MediaEvent::Direct { .. });
                publisher.event(&event, &display.updates);
                if retune {
                    publisher.retune(display, selection, false);
                }
                viewer.handle_event(media, display, event).await?;
                continue;
            }
            request = source_requests.recv() => {
                let Some(SourceRequest { source, enable }) = request else { return Ok(Finish::Left); };
                if enable {
                    if picker.is_none() && !publisher.active(source) && can_publish {
                        picker = Some(pick_source(source));
                    } else {
                        changes.report(SourceChange::Cancelled(source)).await?;
                    }
                } else if publisher.active(source) {
                    publisher.retire(source);
                    media.set_source(purpose(source), false).map_err(text)?;
                    changes.report(SourceChange::Stopped(source)).await?;
                    publisher.focus(selection, &display.updates);
                    publisher.retune(display, selection, false);
                }
                continue;
            }
            (source, result) = next_grant(&mut picker) => {
                picker = None;
                let change = match result {
                    Ok(grant) => {
                        let monitor = grant.can_include_app();
                        match media.set_source(purpose(source), true) {
                            Ok(()) => match publisher.start(grant, worker, selection, display.updates.clone()) {
                                Ok(()) => SourceChange::Active { source, monitor },
                                Err(error) => {
                                    media.set_source(purpose(source), false).map_err(text)?;
                                    SourceChange::Failed(source, error)
                                }
                            },
                            Err(error) => SourceChange::Failed(source, text(error)),
                        }
                    }
                    Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => SourceChange::Cancelled(source),
                    Err(error) => SourceChange::Failed(source, text(error)),
                };
                changes.report(change).await?;
                publisher.focus(selection, &display.updates);
                publisher.retune(display, selection, false);
                continue;
            }
            frame = publisher::LocalVideo::next(camera) => (Source::Camera, frame),
            frame = publisher::LocalVideo::next(screen) => (Source::Screen, frame),
            _ = publisher.retiring.next(), if !publisher.retiring.is_empty() => continue,
            Ok(()) = display.quality.changed() => {
                selection = *display.quality.borrow_and_update();
                if publisher.focus(selection, &display.updates) {
                    publisher.retune(display, selection, false);
                } else {
                    viewer.selection_changed(media, display, selection).await?;
                }
                continue;
            }
            _ = timer.tick() => {
                if publisher.focus(selection, &display.updates) {
                    publisher.retune(display, selection, true);
                } else {
                    viewer.report(display);
                }
                continue;
            }
        };
        match frame {
            Ok(frame) => publisher.frame(source, frame, media, display.updates.visible())?,
            Err(error) => {
                publisher.retire(source);
                media.set_source(purpose(source), false).map_err(text)?;
                changes.report(SourceChange::Stopped(source)).await?;
                changes
                    .report(SourceChange::Failed(source, text(error)))
                    .await?;
                publisher.focus(selection, &display.updates);
                publisher.retune(display, selection, false);
            }
        }
    }
}

async fn prepare(crypto: CryptoHandle, options: Connect<'_>) -> Result<(Media, PathBuf), String> {
    let (executable, video_worker) = WORKERS
        .as_ref()
        .ok_or("The native video workers are unavailable.")?
        .clone();
    let connection = Connection::connect(crypto, options).await.map_err(text)?;
    let media = connection
        .into_media(MediaOptions {
            executable,
            sfu_port: SFU_PORT,
            publish: None,
        })
        .await
        .map_err(text)?;
    Ok((media, video_worker))
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::{Instant, Quality, VideoKind, VideoStats, quality_readout, stats};
    use nuschl_ipc::video::CaptureLimits;

    #[test]
    fn auto_quality_shows_the_current_encoder_target_or_received_resolution() {
        let now = Instant::now();
        let kind = VideoKind::Screen;
        let mut measurements = stats::Stats::new(false, now);
        measurements.frame(2580, 1080, None);
        let mut video = VideoStats {
            kind,
            sample: measurements.sample(now + stats::INTERVAL),
            encoder: None,
        };
        assert_eq!(quality_readout(kind, Quality::Auto, None), "Auto");
        assert_eq!(
            quality_readout(kind, Quality::Auto, Some(&video)),
            "Auto (1080p)"
        );
        video.encoder = Some(stats::Encoder {
            target: CaptureLimits {
                height: 1440,
                fps: 60,
                bitrate: 8_000_000,
            },
            budget_bps: None,
            ceiling: None,
            hardware: None,
        });
        assert_eq!(
            quality_readout(kind, Quality::Auto, Some(&video)),
            "Auto (1440p60)"
        );
    }
}
