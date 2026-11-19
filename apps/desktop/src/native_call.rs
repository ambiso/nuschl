// SPDX-License-Identifier: EUPL-1.2
//! Linux calls on the desktop. Capture and decoding use confined video workers.

use crate::{
    dialogs::{Tone, styled_button},
    layout,
    services::ServiceEvent,
    state::Desktop,
    theme::{BORDER, MAIN, TEXT, TEXT_MUTED, color},
};
use gpui_kit::{prelude::*, *};
use nuschl_client::native::sfu::{Connect, Connection, Media, MediaEvent, MediaOptions, Purpose};
use nuschl_core::{call::page_url, model::TextTarget, network::PeerPolicy};
use nuschl_ipc::video::{Capture, DecodedFrame, Decoder, Source, portal};
use nuschl_protocol::sfu::Codec;
use nuschl_session::{config::ClientSettings, identity::CryptoHandle};
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
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
    room: String,
    server: String,
    conversation: TextTarget,
    voice_generation: u64,
    browser: String,
    surface: Entity<Surface>,
    stop: Option<oneshot::Sender<()>>,
    after_stop: Option<AfterStop>,
    finished: bool,
    curtain: bool,
    curtain_ready: Option<oneshot::Sender<()>>,
}

enum AfterStop {
    Close,
    Browser,
}

pub struct Start {
    crypto: CryptoHandle,
    generation: u64,
    url: String,
    host: String,
    source: Option<Source>,
    updates: Arc<Latest>,
    stop: oneshot::Receiver<()>,
}

struct Update {
    status: String,
    frame: Option<DecodedFrame>,
    finished: bool,
}

struct Latest {
    update: Mutex<Update>,
    ready: Notify,
}

impl Latest {
    fn status(&self, status: impl Into<String>) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update.status = status.into();
        drop(update);
        self.ready.notify_one();
    }

    fn finished(&self, status: String) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update.status = status;
        update.frame = None;
        update.finished = true;
        drop(update);
        self.ready.notify_one();
    }

    fn frame(&self, frame: DecodedFrame) {
        let mut update = self
            .update
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if update.status != "Watching video" {
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
    _updates: Task<()>,
}

impl Surface {
    fn new(updates: Arc<Latest>, status: &str, cx: &mut Context<Self>) -> Self {
        let observe = cx.spawn(async move |this, cx| {
            loop {
                updates.ready.notified().await;
                let (status, frame, finished) = {
                    let mut update = updates
                        .update
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    (update.status.clone(), update.frame.take(), update.finished)
                };
                if this
                    .update(cx, |this, cx| {
                        this.status = status;
                        if finished {
                            this.image = None;
                        } else if let Some(frame) = frame.and_then(render_frame) {
                            this.image = Some(frame);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                if finished {
                    break;
                }
            }
        });
        Self {
            status: status.into(),
            image: None,
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
            .gap_1()
            .w_full()
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
                        .w_full()
                        .h(px(240.))
                        .bg(rgb(0x08090c))
                        .child(img(image).size_full().object_fit(ObjectFit::Contain)),
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
            && !self.settings.relay_only
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
        let owner = source.is_some();
        let room = link.url();
        let browser = if owner {
            let Some(browser) = link.creator_url() else {
                return;
            };
            browser
        } else {
            room.clone()
        };
        let status = match source {
            Some(Source::Camera) => "Waiting for camera permission",
            Some(Source::Screen) => "Choose a screen or window",
            None => "Joining video call",
        };
        let updates = Arc::new(Latest {
            update: Mutex::new(Update {
                status: status.into(),
                frame: None,
                finished: false,
            }),
            ready: Notify::new(),
        });
        let (stop, stopped) = oneshot::channel();
        self.native_call_generation = generation;
        self.native_call = Some(Active {
            owner,
            room,
            server,
            conversation,
            voice_generation,
            browser: browser.clone(),
            surface: cx.new(|cx| Surface::new(updates.clone(), status, cx)),
            stop: Some(stop),
            after_stop: None,
            finished: false,
            curtain: false,
            curtain_ready: None,
        });
        self.services.native_call(Start {
            crypto: self.driver.call_crypto(),
            generation,
            url: browser,
            host: link.host().to_owned(),
            source,
            updates,
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
                self.announce_call(link, "Call");
            }
            Err(error) => self.client.servers[index].set_notice(error),
        }
    }

    fn stop_native(&mut self, after: AfterStop) {
        if let Some(call) = &mut self.native_call {
            if let Some(stop) = call.stop.take() {
                let _ = stop.send(());
            }
            call.after_stop = Some(after);
        }
    }

    pub fn stop_native_owner(&mut self) {
        if let Some(call) = &self.native_call
            && call.owner
        {
            if call.finished {
                self.native_call = None;
            } else {
                self.stop_native(AfterStop::Close);
            }
        }
    }

    pub fn sync_native_call(&mut self) {
        let Some(call) = &self.native_call else {
            return;
        };
        let current = self.selected_indices().is_some_and(|(index, channel)| {
            let server = &self.client.servers[index];
            server.status == nuschl_client::server::Status::Connected
                && server.id == call.server
                && server.generation == call.voice_generation
                && server.channels[channel].target == call.conversation
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
        if self.settings.relay_only || !current {
            if call.finished {
                self.native_call = None;
            } else {
                self.stop_native(AfterStop::Close);
            }
        }
    }

    fn leave_native_call(&mut self, after: AfterStop, cx: &mut Context<Self>) {
        self.stop_native(after);
        if self.native_call.as_ref().is_some_and(|call| call.finished) {
            self.native_finished(self.native_call_generation, cx);
        }
        cx.notify();
    }

    pub fn native_curtain(&mut self, generation: u64, ready: oneshot::Sender<()>) {
        if generation == self.native_call_generation
            && let Some(call) = &mut self.native_call
            && call.stop.is_some()
        {
            call.curtain = true;
            call.curtain_ready = Some(ready);
        }
    }

    pub fn native_finished(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.native_call_generation {
            return;
        }
        let Some(call) = &mut self.native_call else {
            return;
        };
        call.finished = true;
        call.stop.take();
        call.curtain = false;
        call.curtain_ready = None;
        match call.after_stop.take() {
            Some(AfterStop::Browser) => {
                cx.open_url(&page_url(&call.browser, self.settings.relay_only));
                self.native_call = None;
            }
            Some(AfterStop::Close) => {
                let ends_our_call = call.owner
                    && self
                        .client
                        .own_call()
                        .is_some_and(|own| own.link == call.room);
                self.native_call = None;
                if ends_our_call {
                    self.end_call();
                }
            }
            None => {}
        }
    }
}

pub fn curtain(
    this: &mut Desktop,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    let call = this.native_call.as_mut()?;
    if !call.curtain {
        return None;
    }
    if let Some(ready) = call.curtain_ready.take() {
        window.on_next_frame(move |_, _| {
            let _ = ready.send(());
        });
    }
    let cover = div()
        .id("screen-sharing-curtain")
        .size_full()
        .flex()
        .flex_col()
        .bg(color(MAIN))
        .text_color(color(TEXT))
        .map(|cover| crate::chrome::round(cover, this.window_corners))
        .child(crate::chrome::title_bar(
            "Screen sharing".into(),
            window,
            cx,
        ))
        .child(
            div()
                .px_3()
                .py_2()
                .child("Conversations are hidden during screen sharing."),
        )
        .when_some(panel(this, cx), |cover, panel| cover.child(panel));
    Some(crate::chrome::frame(window, cover.into_any_element()))
}

pub fn panel(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let call = this.native_call.as_ref()?;
    let actions = layout::actions()
        .child(styled_button(
            "native-call-browser",
            "Open in browser",
            Tone::Secondary,
            |this, _, cx| {
                this.leave_native_call(AfterStop::Browser, cx);
            },
            cx,
        ))
        .child(styled_button(
            "native-call-stop",
            if call.owner {
                "Stop sharing"
            } else {
                "Leave video"
            },
            Tone::Danger,
            |this, _, cx| {
                this.leave_native_call(AfterStop::Close, cx);
            },
            cx,
        ));
    Some(
        div()
            .id("native-call")
            .flex()
            .flex_col()
            .min_w_0()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(color(BORDER))
            .bg(color(MAIN))
            .child(actions)
            .child(call.surface.clone())
            .into_any_element(),
    )
}

pub async fn run(
    start: Start,
    mut settings: watch::Receiver<ClientSettings>,
    events: mpsc::Sender<ServiceEvent>,
) {
    let generation = start.generation;
    let updates = start.updates.clone();
    let status = match drive(start, &mut settings, &events).await {
        Ok(()) => "Video call ended".into(),
        Err(error) => format!("Video unavailable. {error}"),
    };
    updates.finished(status);
    let _ = events
        .send(ServiceEvent::NativeFinished { generation })
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
        source,
        updates,
        mut stop,
    } = start;
    let revoke = async {
        loop {
            if settings.borrow_and_update().relay_only || settings.changed().await.is_err() {
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
    let (mut media, grant, video_worker) = tokio::select! {
        biased;
        () = &mut cancel => return Ok(()),
        result = prepare(crypto, generation, &host, &url, source, &updates) => result?,
    };
    let result = if let Some(grant) = grant {
        publish_video(
            &mut media,
            grant,
            video_worker,
            &updates,
            generation,
            events,
            cancel,
        )
        .await
    } else {
        updates.status("Waiting for video");
        receive_video(&mut media, video_worker, &updates, cancel).await
    };
    result.and(media.stop().await.map_err(text))
}

async fn publish_video(
    media: &mut Media,
    grant: portal::Grant,
    worker: PathBuf,
    updates: &Latest,
    generation: u64,
    events: &mpsc::Sender<ServiceEvent>,
    mut cancel: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<(), String> {
    if grant.requires_curtain() {
        let hide_conversations = async {
            let (ready, painted) = oneshot::channel();
            events
                .send(ServiceEvent::NativeCurtain { generation, ready })
                .await
                .map_err(text)?;
            tokio::time::timeout(Duration::from_secs(5), painted)
                .await
                .map_err(text)?
                .map_err(text)
        };
        tokio::select! {
            biased;
            () = &mut cancel => return Ok(()),
            result = hide_conversations => result?,
        }
    }
    let rate = match grant.source() {
        Source::Screen => 4_000_000,
        Source::Camera => 2_000_000,
    };
    let mut capture = grant.start(worker, rate).map_err(text)?;
    updates.status("Waiting for a viewer");
    let result = tokio::select! {
        biased;
        () = &mut cancel => Ok(()),
        result = send_video(media, &mut capture, rate, updates) => result,
    };
    result.and(capture.stop().await.map_err(text))
}

async fn prepare(
    crypto: CryptoHandle,
    generation: u64,
    host: &str,
    url: &str,
    source: Option<Source>,
    updates: &Latest,
) -> Result<(Media, Option<portal::Grant>, PathBuf), String> {
    let (executable, video_worker) = WORKERS
        .as_ref()
        .ok_or("The native video workers are unavailable.")?
        .clone();
    let grant = match source {
        Some(source) => Some(portal::request(source, None).await.map_err(text)?),
        None => None,
    };
    updates.status("Joining video call");
    let connection = Connection::connect(
        crypto,
        Connect {
            generation,
            host,
            link: url,
            codecs: &[Codec::Av1],
            policy: PeerPolicy::default(),
        },
    )
    .await
    .map_err(text)?;
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

async fn send_video(
    media: &mut Media,
    capture: &mut Capture,
    mut cap: u32,
    updates: &Latest,
) -> Result<(), String> {
    let mut ready = false;
    let mut sent = false;
    let mut capacity = u32::MAX;
    let mut rate = cap;
    loop {
        tokio::select! {
            event = media.next() => match event.transpose().map_err(text)? {
                Some(MediaEvent::SendReady) => { ready = true; capture.request_keyframe(); }
                Some(MediaEvent::KeyframeRequired) => capture.request_keyframe(),
                Some(MediaEvent::EncoderLimits { bitrate, .. }) => { cap = video_rate(bitrate); }
                Some(MediaEvent::BandwidthEstimate { bitrate }) => { capacity = video_rate(bitrate.saturating_mul(4) / 5); }
                Some(_) => {}
                None => return Ok(()),
            },
            frame = capture.next() => {
                let frame = frame.map_err(text)?;
                if ready {
                    let ticks = frame.capture_time_us.saturating_mul(9) / 100;
                    if media.send_frame(ticks, frame.keyframe, frame.av1).map_err(text)? {
                        if !sent { sent = true; updates.status("Sharing video"); }
                    } else {
                        capture.request_keyframe();
                    }
                }
            }
        }
        let next = cap.min(capacity);
        if next != rate {
            capture.set_bitrate(next).map_err(text)?;
            rate = next;
        }
    }
}

async fn receive_video(
    media: &mut Media,
    executable: PathBuf,
    updates: &Latest,
    cancel: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<(), String> {
    let mut decoder = None;
    let receive = async {
        while let Some(event) = media.next().await {
            let MediaEvent::Frame {
                purpose,
                contiguous,
                key_frame,
                data,
                ..
            } = event.map_err(text)?
            else {
                continue;
            };
            let source = match purpose {
                Purpose::Camera => Source::Camera,
                Purpose::ScreenVideo => Source::Screen,
                _ => continue,
            };
            // One decoded source per native viewer. The browser can show every track.
            let decoder = match &mut decoder {
                Some((chosen, decoder)) if *chosen == source => decoder,
                Some(_) => continue,
                None => {
                    &mut decoder
                        .insert((
                            source,
                            Decoder::start(executable.clone(), source).map_err(text)?,
                        ))
                        .1
                }
            };
            if !contiguous {
                decoder.reset();
            }
            if let Some(frame) = decoder.decode(key_frame, data).await.map_err(text)? {
                updates.frame(frame);
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
