// SPDX-License-Identifier: EUPL-1.2

use nuschl_ipc::video::Source;
use std::{
    error::Error,
    io,
    time::{Duration, Instant},
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    graphics_capture_picker::{Error as PickerError, GraphicsCapturePicker},
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

mod camera;

type Failure = Box<dyn Error + Send + Sync>;
type FrameSink = dyn FnMut(u32, u32, usize, &[u8]) -> io::Result<()> + Send;

pub fn run(
    source: Source,
    on_frame: impl FnMut(u32, u32, usize, &[u8]) -> io::Result<()> + Send + 'static,
) -> Result<(), Failure> {
    match source {
        Source::Camera => camera::run(on_frame),
        Source::Screen => screen(Box::new(on_frame)),
    }
}

fn screen(on_frame: Box<FrameSink>) -> Result<(), Failure> {
    let target = match GraphicsCapturePicker::pick_item() {
        Ok(Some(target)) => target,
        Ok(None) | Err(PickerError::Canceled) => {
            return Err(
                io::Error::new(io::ErrorKind::Interrupted, "Screen sharing canceled.").into(),
            );
        }
        Err(error) => return Err(error.into()),
    };
    let (width, height) = target.size()?;
    if !valid_screen_size(u32::try_from(width)?, u32::try_from(height)?) {
        return Err(io::Error::other("The selected screen size exceeds the capture limit.").into());
    }
    Screen::start(Settings::new(
        target,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        on_frame,
    ))?;
    Ok(())
}

// Bound the CPU mapping before D3D11 copies the selected surface.
fn valid_screen_size(width: u32, height: u32) -> bool {
    (16..=8192).contains(&width)
        && (16..=8192).contains(&height)
        && u64::from(width) * u64::from(height) <= 7680 * 4320
}

struct Screen {
    on_frame: Box<FrameSink>,
    last_frame: Option<Instant>,
}

impl GraphicsCaptureApiHandler for Screen {
    type Flags = Box<FrameSink>;
    type Error = io::Error;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            on_frame: context.flags,
            last_frame: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let now = Instant::now();
        if self
            .last_frame
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(33))
        {
            return Ok(());
        }
        let (width, height) = (frame.width(), frame.height());
        if !valid_screen_size(width, height) {
            return Err(io::Error::other(
                "The selected screen size exceeds the capture limit.",
            ));
        }
        let mut buffer = frame.buffer().map_err(io::Error::other)?;
        let stride = usize::try_from(buffer.row_pitch()).map_err(io::Error::other)?;
        let pixels = buffer.as_raw_buffer();
        self.last_frame = Some(now);
        (self.on_frame)(width, height, stride, pixels)
    }
}
