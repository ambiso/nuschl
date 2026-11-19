// SPDX-License-Identifier: EUPL-1.2
//! Hardware AV1 from the authorized PipeWire stream. GStreamer imports
//! DMA-BUFs supported by the VA driver and uses VA surfaces through encode.

use super::{Cadence, Control, Failure};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use nuschl_ipc::video::{Kind, Settings, Source};
use std::{
    io,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

pub fn prepare(source: Source, node: Option<u32>, bitrate: u32) -> Result<gst::Pipeline, Failure> {
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
    let (width, height) = source.max_dimensions();
    let description = format!(
        concat!(
            "pipewiresrc fd=0 {target} ! ",
            "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream ! ",
            "vapostproc name=scale ! capsfilter name=limit ",
            "caps=\"video/x-raw(memory:VAMemory),format=NV12,width=[16,{width}],height=[16,{height}]\" ! ",
            "vaav1enc name=encoder bitrate={bitrate} rate-control=cbr hierarchical-level=1 ",
            "ref-frames=1 key-int-max=60 ! ",
            "av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! ",
            "appsink name=encoded max-buffers=1 sync=false",
        ),
        target = target,
        width = width,
        height = height,
        bitrate = bitrate / 1000,
    );
    let pipeline = gst::parse::launch(&description)?;
    pipeline
        .downcast()
        .map_err(|_| "invalid video pipeline".into())
}

pub fn run(pipeline: gst::Pipeline, source: Source, control: Arc<Control>) -> Result<(), Failure> {
    let sink = pipeline
        .by_name("encoded")
        .ok_or("missing video sink")?
        .downcast::<AppSink>()
        .map_err(|_| "invalid video sink")?;
    let encoder = pipeline.by_name("encoder").ok_or("missing video encoder")?;
    let mut scale = Scale::new(&pipeline)?;
    let clock = Instant::now();
    let cadence = Mutex::new(Cadence::default());
    let cadence_control = control.clone();
    // Drop raw frames before VA scaling and encoding. Rates can change
    // independently of the PipeWire source's negotiated framerate.
    let probe = scale
        .input
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let limits = cadence_control.limits();
            if cadence
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .accept(clock.elapsed(), limits.fps)
            {
                gst::PadProbeReturn::Ok
            } else {
                gst::PadProbeReturn::Drop
            }
        })
        .ok_or("could not install the capture cadence limit")?;
    pipeline.set_state(gst::State::Playing)?;
    let result = write_frames(&pipeline, &sink, &encoder, &mut scale, source, &control);
    scale.input.remove_probe(probe);
    let _ = pipeline.set_state(gst::State::Null);
    result
}

struct Scale {
    input: gst::Pad,
    filter: gst::Element,
    size: Option<(u16, u16)>,
}

impl Scale {
    fn new(pipeline: &gst::Pipeline) -> Result<Self, Failure> {
        let input = pipeline
            .by_name("scale")
            .and_then(|element| element.static_pad("sink"))
            .ok_or("missing video scaler input")?;
        let filter = pipeline
            .by_name("limit")
            .ok_or("missing video caps filter")?;
        Ok(Self {
            input,
            filter,
            size: None,
        })
    }

    fn update(
        &mut self,
        source: Source,
        limits: nuschl_ipc::video::CaptureLimits,
    ) -> Result<bool, Failure> {
        // Measure the unscaled PipeWire format upstream of the capsfilter.
        let Some(input) = self.input.current_caps() else {
            return Ok(false);
        };
        let size = dimensions(input.structure(0).ok_or("missing PipeWire video format")?)?;
        let Some((width, height)) = limits.dimensions(source, size.0, size.1) else {
            // Some PipeWire nodes produce dimensions below the VA encoder's
            // minimum. Retain the legal format chosen by the existing filter.
            return Ok(self.size.take().is_some());
        };
        if self.size == Some((width, height)) {
            return Ok(false);
        }
        let caps =
            format!("video/x-raw(memory:VAMemory),format=NV12,width={width},height={height}")
                .parse::<gst::Caps>()?;
        self.filter.set_property("caps", caps);
        self.size = Some((width, height));
        Ok(true)
    }
}

fn dimensions(format: &gst::StructureRef) -> Result<(u16, u16), Failure> {
    Ok((
        u16::try_from(format.get::<i32>("width")?)?,
        u16::try_from(format.get::<i32>("height")?)?,
    ))
}

fn write_frames(
    pipeline: &gst::Pipeline,
    sink: &AppSink,
    encoder: &gst::Element,
    scale: &mut Scale,
    source: Source,
    control: &Control,
) -> Result<(), Failure> {
    let bus = pipeline.bus().ok_or("missing pipeline bus")?;
    let clock = Instant::now();
    let mut stdout = io::stdout().lock();
    let mut bitrate = encoder.property::<u32>("bitrate");
    let mut previous = 0;
    let mut waiting_for_key = true;
    let mut requested_at: Option<Instant> = None;
    loop {
        let limits = control.limits();
        if scale.update(source, limits)? {
            waiting_for_key = true;
            requested_at = None;
        }
        let wanted = limits.bitrate / 1000;
        if wanted != bitrate {
            encoder.set_property("bitrate", wanted);
            bitrate = encoder.property::<u32>("bitrate");
        }
        if requested_at.is_none_or(|time| time.elapsed() >= Duration::from_millis(150))
            && (control.keyframe.swap(false, Ordering::Relaxed) || waiting_for_key)
        {
            force_keyframe(encoder)?;
            requested_at = Some(Instant::now());
        }
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
        let (width, height) = dimensions(format)?;
        if scale.size.is_some_and(|wanted| wanted != (width, height))
            || width.min(height) > limits.height
        {
            continue;
        }
        let settings = Settings {
            source,
            width,
            height,
            fps: u16::try_from(fps)?.min(limits.fps),
            bitrate: bitrate * 1000,
        };
        settings.frame_bytes()?;

        let buffer = sample.buffer().ok_or("missing AV1 packet")?;
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let captured = sample.segment().and_then(|segment| {
            segment
                .downcast_ref::<gst::ClockTime>()?
                .to_running_time(buffer.pts())
        });
        let stale = captured
            .zip(pipeline.current_running_time())
            .is_some_and(|(then, now)| {
                now.saturating_sub(then) > gst::ClockTime::from_mseconds(250)
            });
        if buffer.size() == 0
            || buffer.size() > settings.packet_limit(keyframe)
            || buffer.flags().contains(gst::BufferFlags::CORRUPTED)
            || stale
        {
            waiting_for_key = true;
            continue;
        }
        if waiting_for_key && !keyframe {
            continue;
        }
        waiting_for_key = false;
        let bytes = buffer.map_readable()?;
        let timestamp = captured
            .map_or_else(
                || u64::try_from(clock.elapsed().as_micros()),
                |time| Ok(time.useconds()),
            )?
            .max(previous + 1);
        previous = timestamp;
        super::write_frame(
            &mut stdout,
            Kind::HardwareFrame,
            settings,
            timestamp,
            keyframe,
            bytes.as_slice(),
        )?;
    }
}

fn force_keyframe(encoder: &gst::Element) -> Result<(), Failure> {
    let source = encoder
        .static_pad("src")
        .ok_or("missing video encoder output")?;
    // The GstVideoEncoder upstream key-unit event takes a running time in ns.
    let event = gst::event::CustomUpstream::new(
        gst::Structure::builder("GstForceKeyUnit")
            .field("running-time", u64::MAX)
            .field("all-headers", true)
            .field("count", 0u32)
            .build(),
    );
    // The first request can race the pipeline's transition to Playing.
    source.send_event(event);
    Ok(())
}
