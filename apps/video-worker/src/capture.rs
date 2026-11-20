// SPDX-License-Identifier: EUPL-1.2
//! Capture from one portal-granted PipeWire connection.

#[cfg(feature = "vaapi")]
mod vaapi;

use nuschl_ipc::video::{
    CaptureLimits, HEADER_BYTES, Header, Kind, Settings, Source, fit_capture_preview,
    read_capture_start, write_capture_preview,
};
use pipewire::{self as pw, properties::properties, spa};
use spa::{
    buffer::{ChunkFlags, DataType},
    param::{
        ParamType,
        format::{FormatProperties, MediaSubtype, MediaType},
        video::{VideoFormat, VideoInfoRaw},
    },
    pod::{Pod, Value, object, property, serialize::PodSerializer},
    utils::{Direction, Fraction, Rectangle, SpaTypes},
};
use std::{
    io::{self, Cursor, Read as _},
    os::{fd::AsFd as _, unix::net::UnixStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::sync_channel,
    },
    time::{Duration, Instant},
};

type Failure = Box<dyn std::error::Error>;

const PREVIEW_FPS: u16 = 30;

struct Picture {
    settings: Settings,
    captured_at: Duration,
    i420: Vec<u8>,
}

struct Control {
    limits: Mutex<CaptureLimits>,
    keyframe: AtomicBool,
    preview: AtomicBool,
}

impl Control {
    fn limits(&self) -> CaptureLimits {
        *self
            .limits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Default)]
struct Cadence {
    fps: u16,
    next: Duration,
}

impl Cadence {
    fn accept(&mut self, elapsed: Duration, fps: u16) -> bool {
        let interval = Duration::from_secs(1) / u32::from(fps);
        if fps != self.fps {
            self.fps = fps;
            self.next = elapsed;
        }
        if elapsed < self.next {
            return false;
        }
        self.next += interval;
        if self.next <= elapsed {
            self.next = elapsed + interval;
        }
        true
    }
}

pub fn run() -> Result<(), Failure> {
    let target = std::env::var("NUSCHL_VIDEO_NODE")
        .ok()
        .map(|value| value.parse::<u32>())
        .transpose()?;
    let (source, role) = match target {
        Some(_) => (Source::Screen, "Screen"),
        None => (Source::Camera, "Camera"),
    };
    let (control, preview) = start_control(source)?;
    #[cfg(feature = "vaapi")]
    if std::env::var_os("NUSCHL_VIDEO_VAAPI").is_some()
        && let Ok(pipeline) =
            vaapi::prepare(source, target, control.limits().bitrate, preview.is_some())
    {
        return vaapi::run(pipeline, source, control, preview);
    }
    let remote = io::stdin().as_fd().try_clone_to_owned()?;
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&loop_, None)?;
    let core = context.connect_fd_rc(remote, None)?;
    let stop_loop = loop_.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, _| {
            if let Some(loop_) = stop_loop.upgrade() {
                loop_.quit();
            }
        })
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        "Nuschl video capture",
        properties! {
            *pw::keys::NODE_NAME => "nuschl-video-capture",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => role,
        },
    )?;
    let (pictures, next_picture) = sync_channel::<Picture>(1);
    let (done, finished) = pw::channel::channel::<()>();
    let stop_loop = loop_.downgrade();
    let _attached = finished.attach(loop_.loop_(), move |_| {
        if let Some(loop_) = stop_loop.upgrade() {
            loop_.quit();
        }
    });
    let stop_loop = loop_.downgrade();
    let clock = Instant::now();
    std::thread::scope(|scope| {
        let preview = preview.and_then(|mut output| {
            output
                .set_write_timeout(Some(Duration::from_millis(200)))
                .ok()?;
            let (frames, mut queue) = tokio::sync::mpsc::channel::<(u16, u16, Vec<u8>)>(1);
            let control = control.clone();
            std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    while let Some((width, height, bgra)) = queue.blocking_recv() {
                        if control.preview.load(Ordering::Relaxed)
                            && write_capture_preview(&mut output, width, height, &bgra).is_err()
                        {
                            break;
                        }
                    }
                })
                .ok()?;
            Some(frames)
        });
        let encode_control = control.clone();
        let encoder = scope.spawn(move || {
            let mut stdout = io::stdout().lock();
            for picture in next_picture {
                let Picture {
                    mut settings,
                    captured_at,
                    i420,
                } = picture;
                if clock.elapsed().saturating_sub(captured_at) > Duration::from_millis(250) {
                    continue;
                }
                let capture_time_us = u64::try_from(captured_at.as_micros())
                    .unwrap_or(u64::MAX)
                    .max(1);
                let limits = encode_control.limits();
                if settings.width.min(settings.height) > limits.height || settings.fps > limits.fps
                {
                    continue;
                }
                settings.bitrate = limits.bitrate;
                let packet =
                    match super::linux::encode(&super::linux::config(settings), settings, &i420) {
                        Ok(packet) => packet,
                        Err(error) => {
                            let _ = done.send(());
                            return Err(error.to_string());
                        }
                    };
                if let Err(error) = write_frame(
                    &mut stdout,
                    Kind::CaptureFrame,
                    settings,
                    capture_time_us,
                    true,
                    &packet,
                ) {
                    let _ = done.send(());
                    return Err(error.to_string());
                }
            }
            Ok(())
        });
        let mut cadence = Cadence::default();
        let mut preview_cadence = Cadence::default();
        let listener = stream
            .add_local_listener_with_user_data(None)
            .state_changed(move |_, _, _, state| {
                if matches!(
                    state,
                    pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected
                ) && let Some(loop_) = stop_loop.upgrade()
                {
                    loop_.quit();
                }
            })
            .param_changed(|_, parsed: &mut Option<VideoInfoRaw>, id, parameter| {
                if id == ParamType::Format.as_raw() {
                    *parsed = parameter.and_then(|parameter| {
                        let mut format = VideoInfoRaw::default();
                        format.parse(parameter).ok()?;
                        Some(format)
                    });
                }
            })
            .process(move |stream, format| {
                let (Some(format), Some(mut buffer)) = (format, stream.dequeue_buffer()) else {
                    return;
                };
                let elapsed = clock.elapsed();
                // Leave processing time for the UI and RTC on the software path.
                let limits = control.limits();
                let fps = limits.fps.min(15);
                let encode = cadence.accept(elapsed, fps);
                let preview = preview
                    .as_ref()
                    .filter(|_| {
                        control.preview.load(Ordering::Relaxed)
                            && preview_cadence.accept(elapsed, PREVIEW_FPS)
                    })
                    .and_then(|frames| frames.try_reserve().ok());
                if !encode && preview.is_none() {
                    return;
                }
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                if data.as_raw().chunk.is_null()
                    || !matches!(data.type_(), DataType::MemPtr | DataType::MemFd)
                {
                    return;
                }
                let chunk = data.chunk();
                if chunk.flags().contains(ChunkFlags::CORRUPTED) {
                    return;
                }
                let offset = chunk.offset() as usize;
                let Some(end) = offset.checked_add(chunk.size() as usize) else {
                    return;
                };
                let Ok(stride) = usize::try_from(chunk.stride()) else {
                    return;
                };
                let Some(pixels) = data.data().and_then(|bytes| bytes.get(offset..end)) else {
                    return;
                };
                let (Ok(width), Ok(height)) = (
                    u16::try_from(format.size().width),
                    u16::try_from(format.size().height),
                ) else {
                    return;
                };
                let raw = (width & !1, height & !1);
                let Some(pixels) = MappedPixels::new(format.format(), raw, stride, pixels) else {
                    return;
                };
                if let Some(slot) = preview {
                    slot.send(pixels.preview());
                }
                if !encode {
                    return;
                }
                let Some((width, height)) = limits.dimensions(source, raw.0, raw.1) else {
                    return;
                };
                let settings = Settings {
                    source,
                    width,
                    height,
                    fps,
                    bitrate: limits.bitrate,
                };
                let Some(i420) = convert(&pixels, settings) else {
                    return;
                };
                let _ = pictures.try_send(Picture {
                    settings,
                    captured_at: elapsed,
                    i420,
                });
            })
            .register()?;
        let format = video_format(source)?;
        let format =
            Pod::from_bytes(&format).ok_or("could not construct the PipeWire video format")?;
        stream.connect(
            Direction::Input,
            target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [format],
        )?;
        loop_.run();
        drop(listener);
        encoder
            .join()
            .map_err(|_| "the video encoder thread stopped")??;
        Ok(())
    })
}

fn write_frame(
    output: &mut impl io::Write,
    kind: Kind,
    settings: Settings,
    timestamp_us: u64,
    keyframe: bool,
    av1: &[u8],
) -> io::Result<()> {
    if av1.is_empty() || av1.len() > settings.packet_limit(keyframe) {
        return Ok(());
    }
    let header = Header {
        kind,
        key: keyframe,
        id: timestamp_us,
        bytes: av1.len() as u32,
    };
    output.write_all(&header.to_bytes())?;
    output.write_all(&settings.to_bytes())?;
    output.write_all(av1)?;
    output.flush()
}

fn start_control(source: Source) -> io::Result<(Arc<Control>, Option<UnixStream>)> {
    // Standard output is a private full-duplex socket. Writes return video;
    // reads carry settings from the supervisor.
    let mut input = UnixStream::from(io::stdout().as_fd().try_clone_to_owned()?);
    let (limits, preview) = read_capture_start(&mut input, source)?;
    let set_limits = Header {
        bytes: CaptureLimits::BYTES as u32,
        ..Header::empty(Kind::SetCaptureLimits, 0)
    };
    let set_preview = Header {
        bytes: 1,
        ..Header::empty(Kind::SetCapturePreview, 0)
    };
    let control = Arc::new(Control {
        limits: Mutex::new(limits),
        keyframe: AtomicBool::new(false),
        preview: AtomicBool::new(false),
    });
    let values = Arc::clone(&control);
    std::thread::Builder::new().spawn(move || {
        let mut header = [0; HEADER_BYTES];
        let mut target = [0; CaptureLimits::BYTES];
        loop {
            if let Err(error) = input.read_exact(&mut header) {
                std::process::exit(i32::from(error.kind() != io::ErrorKind::UnexpectedEof));
            }
            let header = Header::from_bytes(header).ok();
            if header == Some(Header::empty(Kind::CaptureKeyframe, 0)) {
                values.keyframe.store(true, Ordering::Relaxed);
                continue;
            }
            if header == Some(set_preview) {
                let mut visible = [0];
                if input.read_exact(&mut visible).is_err() || visible[0] > 1 {
                    std::process::exit(1);
                }
                values.preview.store(visible[0] != 0, Ordering::Relaxed);
                continue;
            }
            if header != Some(set_limits) || input.read_exact(&mut target).is_err() {
                std::process::exit(1);
            }
            let Ok(next) = CaptureLimits::from_bytes(source, target) else {
                std::process::exit(1);
            };
            *values
                .limits
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = next;
        }
    })?;
    Ok((control, preview))
}

fn video_format(source: Source) -> io::Result<Vec<u8>> {
    let (width, height) = source.max_dimensions();
    let format = object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::RGBx,
            VideoFormat::BGRA,
            VideoFormat::RGBA,
            VideoFormat::RGB,
            VideoFormat::BGR,
            VideoFormat::YUY2
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 320,
                height: 240
            },
            Rectangle {
                width: 16,
                height: 16
            },
            Rectangle {
                width: width.into(),
                height: height.into()
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 120, denom: 1 }
        ),
    );
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(format))
        .map(|(buffer, _)| buffer.into_inner())
        .map_err(io::Error::other)
}

struct MappedPixels<'a> {
    width: usize,
    height: usize,
    stride: usize,
    pixel_bytes: usize,
    red: usize,
    data: &'a [u8],
}

impl<'a> MappedPixels<'a> {
    fn new(format: VideoFormat, raw: (u16, u16), stride: usize, data: &'a [u8]) -> Option<Self> {
        let (width, height) = (usize::from(raw.0), usize::from(raw.1));
        let (pixel_bytes, red) = match format {
            VideoFormat::BGRx | VideoFormat::BGRA => (4, 2),
            VideoFormat::RGBx | VideoFormat::RGBA => (4, 0),
            VideoFormat::BGR => (3, 2),
            VideoFormat::RGB => (3, 0),
            VideoFormat::YUY2 => (2, 0),
            _ => return None,
        };
        let row_bytes = width * pixel_bytes;
        if width == 0 || !width.is_multiple_of(2) || stride < row_bytes {
            return None;
        }
        let needed = stride
            .checked_mul(height.checked_sub(1)?)?
            .checked_add(row_bytes)?;
        Some(Self {
            width,
            height,
            stride,
            pixel_bytes,
            red,
            data: data.get(..needed)?,
        })
    }

    fn yuy2(&self) -> bool {
        self.pixel_bytes == 2
    }

    fn row(&self, row: usize, output_height: usize) -> &[u8] {
        let start = (row * self.height / output_height) * self.stride;
        &self.data[start..start + self.width * self.pixel_bytes]
    }

    fn rgb(&self, row: &[u8], x: usize) -> [u8; 3] {
        let offset = x * self.pixel_bytes;
        [
            row[offset + self.red],
            row[offset + 1],
            row[offset + 2 - self.red],
        ]
    }

    fn yuv(&self, row: &[u8], x: usize) -> (u8, u8, u8) {
        if self.yuy2() {
            let offset = x * 2;
            let pair = offset & !3;
            (row[offset], row[pair + 1], row[pair + 3])
        } else {
            let [r, g, b] = self.rgb(row, x);
            yuv(r, g, b)
        }
    }

    fn preview(&self) -> (u16, u16, Vec<u8>) {
        let (width, height) = fit_capture_preview(self.width as u16, self.height as u16);
        let (w, h) = (usize::from(width), usize::from(height));
        let columns: Vec<_> = (0..w).map(|x| x * self.width / w).collect();
        let mut pixels = Vec::with_capacity(w * h * 4);
        for output_row in 0..h {
            let row = self.row(output_row, h);
            for &column in &columns {
                let [r, g, b] = if self.yuy2() {
                    let (y, u, v) = self.yuv(row, column);
                    rgb_from_yuv(y, u, v)
                } else {
                    self.rgb(row, column)
                };
                pixels.extend_from_slice(&[b, g, r, 255]);
            }
        }
        (width, height, pixels)
    }
}

fn convert(raw: &MappedPixels<'_>, settings: Settings) -> Option<Vec<u8>> {
    let (width, height) = (usize::from(settings.width), usize::from(settings.height));
    if raw.width < width || raw.height < height {
        return None;
    }
    let len = settings.frame_bytes().ok()?;
    let mut planar = vec![0; len];
    let (y, uv) = planar.split_at_mut(width * height);
    let (u, v) = uv.split_at_mut(uv.len() / 2);
    let is_yuy2 = raw.yuy2();
    let columns: Vec<_> = (0..width).map(|x| x * raw.width / width).collect();
    for row in (0..height).step_by(2) {
        let top = raw.row(row, height);
        let bottom = raw.row(row + 1, height);
        for (pair, &[left, right]) in columns.as_chunks::<2>().0.iter().enumerate() {
            let chroma = (row / 2) * (width / 2) + pair;
            let luma = row * width + pair * 2;
            let quad = [
                raw.yuv(top, left),
                raw.yuv(top, right),
                raw.yuv(bottom, left),
                raw.yuv(bottom, right),
            ];
            let [first, second, third, fourth] = quad;
            [y[luma], y[luma + 1], y[luma + width], y[luma + width + 1]] =
                [first.0, second.0, third.0, fourth.0];
            let average = |a: u8, b: u8, c: u8, d: u8| {
                ((u16::from(a)
                    + u16::from(b)
                    + u16::from(c)
                    + u16::from(d)
                    + if is_yuy2 { 0 } else { 2 })
                    / 4) as u8
            };
            u[chroma] = average(first.1, second.1, third.1, fourth.1);
            v[chroma] = average(first.2, second.2, third.2, fourth.2);
        }
    }
    Some(planar)
}

fn yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
    let u = ((-26 * r - 87 * g + 113 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (y as u8, u as u8, v as u8)
}

fn rgb_from_yuv(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = 298 * (i32::from(y) - 16);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    [y + 459 * v, y - 55 * u - 136 * v, y + 541 * u]
        .map(|component| ((component + 128) >> 8).clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_3440_by_1440_at_the_1440p_tier() {
        let source = Source::Screen;
        let raw = (3440, 1440);
        let limits = CaptureLimits {
            height: 1440,
            fps: 60,
            bitrate: 8_000_000,
        };
        let (width, height) = limits.dimensions(source, raw.0, raw.1).unwrap();
        assert_eq!((width, height), raw);
        let settings = Settings {
            source,
            width,
            height,
            fps: 15,
            bitrate: limits.bitrate,
        };
        let stride = usize::from(raw.0) * 4;
        let mut pixels = vec![0; stride * usize::from(raw.1)];
        let last = pixels.len() - 2;
        pixels[last] = 255;
        let mapped = MappedPixels::new(VideoFormat::BGRx, raw, stride, &pixels).unwrap();
        let frame = convert(&mapped, settings).unwrap();
        assert_eq!(frame.len(), settings.frame_bytes().unwrap());
        assert_eq!(frame[0], 16);
        assert_eq!(frame[usize::from(width) * usize::from(height) - 1], 63);
    }

    #[test]
    fn cadence_reduces_100_fps_to_60_fps() {
        let mut cadence = Cadence::default();
        let sent = (0..100)
            .filter(|index| cadence.accept(Duration::from_millis(index * 10), 60))
            .count();
        assert_eq!(sent, 60);
    }

    #[test]
    fn validates_stride_and_converts_bgrx_as_bt709() {
        let settings = Settings {
            source: Source::Screen,
            width: 16,
            height: 16,
            fps: 15,
            bitrate: 2_000_000,
        };
        let stride = usize::from(settings.width) * 4 + 4;
        let mut pixels = vec![0; stride * 16];
        pixels[2] = 255;
        let mapped = MappedPixels::new(VideoFormat::BGRx, (16, 16), stride, &pixels).unwrap();
        let converted = convert(&mapped, settings).unwrap();
        assert_eq!(converted[0], 63);
        assert_eq!(converted[16 * 16 + 16 * 16 / 4], 156);
        assert!(MappedPixels::new(VideoFormat::BGRx, (16, 16), 12, &pixels).is_none());
        assert!(MappedPixels::new(VideoFormat::BGRx, (16, 16), usize::MAX, &pixels).is_none());
        assert!(MappedPixels::new(VideoFormat::BGRx, (16, 16), stride, &pixels[..1]).is_none());
    }

    #[test]
    fn packs_preview_pixels_as_opaque_bgra_across_formats_and_row_padding() {
        let rgb = [
            3, 5, 7, 0, 11, 13, 17, 0, 255, 255, 255, 255, 19, 23, 29, 0, 31, 37, 41, 0,
        ];
        let mapped = MappedPixels::new(VideoFormat::RGBx, (2, 2), 12, &rgb).unwrap();
        assert_eq!(
            mapped.preview(),
            (
                2,
                2,
                vec![
                    7, 5, 3, 255, 17, 13, 11, 255, 29, 23, 19, 255, 41, 37, 31, 255
                ]
            )
        );

        let yuy2 = [16, 128, 235, 128];
        let mapped = MappedPixels::new(VideoFormat::YUY2, (2, 1), 4, &yuy2).unwrap();
        assert_eq!(
            mapped.preview(),
            (2, 1, vec![0, 0, 0, 255, 255, 255, 255, 255])
        );
    }
}
