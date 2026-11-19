// SPDX-License-Identifier: EUPL-1.2
//! Hardware AV1 from the authorized PipeWire stream. GStreamer imports
//! DMA-BUFs supported by the VA driver and uses VA surfaces through encode.

use super::Failure;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use nuschl_ipc::video::{Header, Kind, Settings, Source};
use std::{
    io::{self, Write as _},
    time::Instant,
};

const BITRATE: u32 = 20_000_000;

pub fn prepare(node: Option<u32>) -> Result<gst::Pipeline, Failure> {
    let plugins = option_env!("NUSCHL_VIDEO_GST_PLUGINS")
        .ok_or("the VA-API GStreamer plugin list is missing")?;
    gst::init()?;
    for path in plugins.split(':') {
        gst::Plugin::load_file(path)?;
    }
    // Element registration probes the render node before the source starts.
    if gst::ElementFactory::find("vaav1enc").is_none() {
        return Err("the selected render node cannot encode AV1".into());
    }

    let target = node.map_or_else(String::new, |node| format!("path={node}"));
    let description = format!(
        concat!(
            "pipewiresrc fd=0 {target} ! ",
            "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! ",
            "vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width=[16,3840],height=[16,2160] ! ",
            "vaav1enc bitrate={bitrate} rate-control=cbr hierarchical-level=1 key-int-max=1 ! ",
            "av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! ",
            "appsink name=encoded max-buffers=1 leaky-type=downstream sync=false",
        ),
        target = target,
        bitrate = BITRATE / 1000,
    );
    let pipeline = gst::parse::launch(&description)?;
    pipeline
        .downcast()
        .map_err(|_| "invalid video pipeline".into())
}

pub fn run(pipeline: gst::Pipeline, source: Source) -> Result<(), Failure> {
    let sink = pipeline
        .by_name("encoded")
        .ok_or("missing video sink")?
        .downcast::<AppSink>()
        .map_err(|_| "invalid video sink")?;
    let bus = pipeline.bus().ok_or("missing pipeline bus")?;
    pipeline.set_state(gst::State::Playing)?;
    let result = write_frames(&bus, &sink, source);
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn write_frames(bus: &gst::Bus, sink: &AppSink, source: Source) -> Result<(), Failure> {
    let clock = Instant::now();
    let mut stdout = io::stdout().lock();
    loop {
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(300)) else {
            if let Some(message) =
                bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
            {
                return match message.view() {
                    gst::MessageView::Error(error) => Err(error.error().into()),
                    _ => Ok(()),
                };
            }
            continue;
        };
        let format = sample
            .caps()
            .and_then(|caps| caps.structure(0))
            .ok_or("missing AV1 format")?;
        let fps = format
            .get::<gst::Fraction>("framerate")
            .ok()
            .map_or(30, |rate| {
                (rate.numer() / rate.denom().max(1)).clamp(1, 120)
            });
        let settings = Settings {
            source,
            width: u16::try_from(format.get::<i32>("width")?)?,
            height: u16::try_from(format.get::<i32>("height")?)?,
            fps: u16::try_from(fps)?,
            bitrate: BITRATE,
        };
        settings.frame_bytes()?;

        let buffer = sample.buffer().ok_or("missing AV1 packet")?;
        if buffer.size() == 0
            || buffer.size() > settings.packet_limit(true)
            || buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
        {
            continue;
        }
        let bytes = buffer.map_readable()?;
        let timestamp = u64::try_from(clock.elapsed().as_micros())?.max(1);
        let header = Header {
            kind: Kind::HardwareFrame,
            key: true,
            id: timestamp,
            bytes: u32::try_from(bytes.len())?,
        };
        stdout.write_all(&header.to_bytes())?;
        stdout.write_all(&settings.to_bytes())?;
        stdout.write_all(bytes.as_slice())?;
        stdout.flush()?;
    }
}
