// SPDX-License-Identifier: EUPL-1.2
//! Capture from one portal-granted PipeWire connection.

#[cfg(feature = "vaapi")]
mod vaapi;

use nuschl_ipc::video::{HEADER_BYTES, Header, Kind, Settings, Source};
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
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::sync_channel,
    },
    time::{Duration, Instant},
};

type Failure = Box<dyn std::error::Error>;

struct Picture {
    settings: Settings,
    captured_at: Duration,
    i420: Vec<u8>,
}

struct Control {
    bitrate: AtomicU32,
    keyframe: AtomicBool,
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
    let control = start_control()?;
    #[cfg(feature = "vaapi")]
    if std::env::var_os("NUSCHL_VIDEO_VAAPI").is_some()
        && let Ok(pipeline) = vaapi::prepare(target, control.bitrate.load(Ordering::Relaxed))
    {
        return vaapi::run(pipeline, source, &control);
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
                settings.bitrate = control.bitrate.load(Ordering::Relaxed);
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
        let mut encoded_at = None;
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
                // The software preview caps its rate to leave room for the UI and RTC.
                const FPS: u16 = 15;
                if encoded_at.is_some_and(|previous| {
                    elapsed.saturating_sub(previous)
                        < std::time::Duration::from_secs(1) / u32::from(FPS)
                }) {
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
                let settings = Settings {
                    source,
                    width: width & !1,
                    height: height & !1,
                    fps: FPS,
                    bitrate: 2_000_000,
                };
                let Some(i420) = convert(format.format(), settings, stride, pixels) else {
                    return;
                };
                encoded_at = Some(elapsed);
                let _ = pictures.try_send(Picture {
                    settings,
                    captured_at: elapsed,
                    i420,
                });
            })
            .register()?;
        let format = video_format()?;
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

fn start_control() -> io::Result<Arc<Control>> {
    // Standard output is a private full-duplex socket. Writes return video;
    // reads carry settings from the supervisor.
    let mut input = UnixStream::from(io::stdout().as_fd().try_clone_to_owned()?);
    let mut header = [0; HEADER_BYTES];
    input.read_exact(&mut header)?;
    let start = Header {
        bytes: 4,
        ..Header::empty(Kind::CaptureStart, 0)
    };
    if Header::from_bytes(header)? != start {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected capture start",
        ));
    }

    let mut target = [0; 4];
    input.read_exact(&mut target)?;
    let bitrate = u32::from_be_bytes(target);
    if !Settings::valid_bitrate(bitrate) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid capture bitrate",
        ));
    }
    let control = Arc::new(Control {
        bitrate: AtomicU32::new(bitrate),
        keyframe: AtomicBool::new(false),
    });
    let values = Arc::clone(&control);
    std::thread::Builder::new().spawn(move || {
        let update = Header {
            bytes: 4,
            ..Header::empty(Kind::SetBitrate, 0)
        };
        let mut header = [0; HEADER_BYTES];
        loop {
            if let Err(error) = input.read_exact(&mut header) {
                std::process::exit(i32::from(error.kind() != io::ErrorKind::UnexpectedEof));
            }
            let header = Header::from_bytes(header).ok();
            if header == Some(Header::empty(Kind::CaptureKeyframe, 0)) {
                values.keyframe.store(true, Ordering::Relaxed);
                continue;
            }
            if header != Some(update) || input.read_exact(&mut target).is_err() {
                std::process::exit(1);
            }
            let target = u32::from_be_bytes(target);
            if !Settings::valid_bitrate(target) {
                std::process::exit(1);
            }
            values.bitrate.store(target, Ordering::Relaxed);
        }
    })?;
    Ok(control)
}

fn video_format() -> io::Result<Vec<u8>> {
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
                width: 3840,
                height: 2160
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

fn convert(
    format: VideoFormat,
    settings: Settings,
    stride: usize,
    pixels: &[u8],
) -> Option<Vec<u8>> {
    let width = usize::from(settings.width);
    let height = usize::from(settings.height);
    let bytes_per_pixel = match format {
        VideoFormat::BGRx | VideoFormat::RGBx | VideoFormat::BGRA | VideoFormat::RGBA => 4,
        VideoFormat::RGB | VideoFormat::BGR => 3,
        VideoFormat::YUY2 => 2,
        _ => return None,
    };
    let row_bytes = width * bytes_per_pixel;
    if stride < row_bytes {
        return None;
    }
    let len = settings.frame_bytes().ok()?;
    let needed = stride
        .checked_mul(height.checked_sub(1)?)?
        .checked_add(row_bytes)?;
    let pixels = pixels.get(..needed)?;
    let mut planar = vec![0; len];
    let (y, uv) = planar.split_at_mut(width * height);
    let (u, v) = uv.split_at_mut(uv.len() / 2);
    for row in (0..height).step_by(2) {
        let row_start = row * stride;
        let top = &pixels[row_start..row_start + row_bytes];
        let next_start = row_start + stride;
        let bottom = &pixels[next_start..next_start + row_bytes];
        for column in (0..width).step_by(2) {
            let chroma = (row / 2) * (width / 2) + column / 2;
            let luma = row * width + column;
            let offset = column * bytes_per_pixel;
            if format == VideoFormat::YUY2 {
                let top = &top[offset..offset + 4];
                let bottom = &bottom[offset..offset + 4];
                y[luma..luma + 2].copy_from_slice(&[top[0], top[2]]);
                y[luma + width..luma + width + 2].copy_from_slice(&[bottom[0], bottom[2]]);
                u[chroma] = top[1].midpoint(bottom[1]);
                v[chroma] = top[3].midpoint(bottom[3]);
            } else {
                let red = usize::from(matches!(
                    format,
                    VideoFormat::BGRx | VideoFormat::BGRA | VideoFormat::BGR
                )) * 2;
                let quad = [
                    &top[offset..],
                    &top[offset + bytes_per_pixel..],
                    &bottom[offset..],
                    &bottom[offset + bytes_per_pixel..],
                ];
                let [first, second, third, fourth] = quad.map(|p| yuv(p[red], p[1], p[2 - red]));
                [y[luma], y[luma + 1], y[luma + width], y[luma + width + 1]] =
                    [first.0, second.0, third.0, fourth.0];
                let average = |a: u8, b: u8, c: u8, d: u8| {
                    ((u16::from(a) + u16::from(b) + u16::from(c) + u16::from(d) + 2) / 4) as u8
                };
                u[chroma] = average(first.1, second.1, third.1, fourth.1);
                v[chroma] = average(first.2, second.2, third.2, fourth.2);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let converted = convert(VideoFormat::BGRx, settings, stride, &pixels).unwrap();
        assert_eq!(converted[0], 63);
        assert_eq!(converted[16 * 16 + 16 * 16 / 4], 156);
        assert!(convert(VideoFormat::BGRx, settings, 12, &pixels).is_none());
        assert!(convert(VideoFormat::BGRx, settings, usize::MAX, &pixels).is_none());
        assert!(convert(VideoFormat::BGRx, settings, stride, &pixels[..1]).is_none());
    }
}
