// SPDX-License-Identifier: EUPL-1.2
//! AVFoundation and ScreenCaptureKit calls for one user-selected source.

#![allow(non_snake_case)] // The delegate implementations use objc2's selector names.

use crate::layout::{Plane, fit_screen};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, Message, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset640x480,
    AVCaptureSessionPreset1280x720, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferIsPlanar, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    kCVReturnSuccess,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSDictionary, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration,
    SCContentSharingPickerMode, SCContentSharingPickerObserver, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};
use std::{
    io,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(20);
const MAX_FRAME_AGE: Duration = Duration::from_millis(250);

/// Capture on the worker's main thread. Callbacks receive one packed BGRA
/// frame at a time. A callback error stops the capture.
pub fn run(
    screen: bool,
    mut on_frame: impl FnMut(u32, u32, usize, &[u8]) -> io::Result<()> + Send,
) -> io::Result<()> {
    autoreleasepool(|_| {
        let output = FrameOutput::new();
        if screen {
            capture_screen(&output, &mut on_frame)
        } else {
            capture_camera(&output, &mut on_frame)
        }
    })
}

type Callback<'a> = dyn FnMut(u32, u32, usize, &[u8]) -> io::Result<()> + 'a;

struct Frame {
    width: u32,
    height: u32,
    stride: usize,
    captured: Instant,
    pixels: Vec<u8>,
}

#[derive(Default)]
struct Frames {
    current: Mutex<Option<Frame>>,
    stopped: AtomicBool,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NuschlMacCaptureFrameOutput"]
    #[ivars = Frames]
    struct FrameOutput;

    // SAFETY: The class inherits NSObject and uses its NSObjectProtocol methods.
    unsafe impl NSObjectProtocol for FrameOutput {}

    // SAFETY: The exact delegate signatures are registered. The supplied camera
    // sample is borrowed only for the callback on the selected serial queue.
    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn captureOutput_didOutputSampleBuffer_fromConnection(
            &self,
            _: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _: &AVCaptureConnection,
        ) {
            self.submit(sample);
        }
    }

    // SAFETY: The sample is borrowed only while ScreenCaptureKit's serial
    // dispatch queue holds it. Audio and microphone callbacks are disregarded.
    unsafe impl SCStreamOutput for FrameOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_didOutputSampleBuffer_ofType(
            &self,
            _: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.submit(sample);
            }
        }
    }

    // SAFETY: A stop notification touches only the atomic shared signal.
    unsafe impl SCStreamDelegate for FrameOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_didStopWithError(&self, _: &SCStream, _: &NSError) {
            self.ivars().stopped.store(true, Ordering::Release);
        }
    }
);

impl FrameOutput {
    fn new() -> Retained<Self> {
        let allocated = Self::alloc().set_ivars(Frames::default());
        // SAFETY: The allocated object inherits NSObject's initializer.
        unsafe { msg_send![super(allocated), init] }
    }

    fn submit(&self, sample: &CMSampleBuffer) {
        let Ok(mut next) = self.ivars().current.try_lock() else {
            return;
        };
        if next.is_none() {
            *next = copy_frame(sample);
        }
    }

    fn deliver(&self, on_frame: &mut Callback<'_>) -> io::Result<bool> {
        let frame = self
            .ivars()
            .current
            .lock()
            .map_err(|_| io::Error::other("macOS video output failed"))?
            .take();
        if let Some(frame) = frame.filter(|frame| frame.captured.elapsed() <= MAX_FRAME_AGE) {
            on_frame(frame.width, frame.height, frame.stride, &frame.pixels)?;
            return Ok(true);
        }
        Ok(false)
    }
}

struct ReadLock<'a>(&'a CVPixelBuffer);

impl Drop for ReadLock<'_> {
    fn drop(&mut self) {
        // SAFETY: This guard exists only after the matching read lock succeeded.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

fn copy_frame(sample: &CMSampleBuffer) -> Option<Frame> {
    let captured = Instant::now();
    // SAFETY: The sample is held by the capture framework for the callback.
    let buffer = unsafe { sample.image_buffer() }?;
    if CVPixelBufferIsPlanar(&buffer)
        || CVPixelBufferGetPixelFormatType(&buffer) != kCVPixelFormatType_32BGRA
    {
        return None;
    }
    // SAFETY: The retained pixel buffer stays alive through the read lock.
    let locked = unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) };
    if locked != kCVReturnSuccess {
        return None;
    }
    let _lock = ReadLock(&buffer);
    let width = CVPixelBufferGetWidth(&buffer);
    let height = CVPixelBufferGetHeight(&buffer);
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    let plane = Plane::new(width, height, stride, CVPixelBufferGetDataSize(&buffer))?;
    let bytes = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
    if bytes.is_null() {
        return None;
    }
    // SAFETY: CoreVideo's read lock holds this packed plane stable. Its non-null
    // base address spans the validated number of bytes within the reported size.
    let bytes = unsafe { std::slice::from_raw_parts(bytes.cast_const(), plane.span) };
    let mut pixels = Vec::with_capacity(plane.row_bytes * height);
    for row in bytes.chunks(stride) {
        pixels.extend_from_slice(&row[..plane.row_bytes]);
    }
    Some(Frame {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        stride: plane.row_bytes,
        captured,
        pixels,
    })
}

fn capture_camera(output: &FrameOutput, on_frame: &mut Callback<'_>) -> io::Result<()> {
    // SAFETY: AVFoundation exports this constant media type for video devices.
    let video =
        unsafe { AVMediaTypeVideo }.ok_or_else(|| io::Error::other("No video media type"))?;
    camera_consent(video)?;
    // SAFETY: The media type is the framework's video constant.
    let device = unsafe { AVCaptureDevice::defaultDeviceWithMediaType(video) }
        .ok_or_else(|| io::Error::other("No default camera"))?;
    // SAFETY: The retained default video device and resulting input are held in this scope.
    let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
        .map_err(|_| io::Error::other("The camera could not be opened"))?;
    // SAFETY: The capture session is created and operated on this thread.
    let session = unsafe { AVCaptureSession::new() };
    // SAFETY: This is a new session and the input is a genuine AVFoundation input.
    if !unsafe { session.canAddInput(&input) } {
        return Err(io::Error::other("The camera input is unavailable"));
    }
    // SAFETY: AVFoundation accepted the input for this session.
    unsafe { session.addInput(&input) };
    // SAFETY: These are the framework's bounded-resolution preset constants.
    let hd = unsafe { AVCaptureSessionPreset1280x720 };
    // SAFETY: The VGA preset is the fallback for cameras without a 720p preset.
    let vga = unsafe { AVCaptureSessionPreset640x480 };
    let preset = [hd, vga]
        .into_iter()
        // SAFETY: The presets and configured input belong to AVFoundation.
        .find(|preset| unsafe { session.canSetSessionPreset(preset) })
        .ok_or_else(|| io::Error::other("The camera has no supported capture preset"))?;
    // SAFETY: The selected preset passed the session's compatibility check.
    unsafe { session.setSessionPreset(preset) };
    // SAFETY: The output is a fresh AVFoundation video output.
    let video_out = unsafe { AVCaptureVideoDataOutput::new() };
    let key = NSString::from_str("PixelFormatType");
    let format = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
    let value: &AnyObject = &format;
    let settings = NSDictionary::from_slices(&[&*key], &[value]);
    // SAFETY: The dictionary uses the CVPixelBuffer format key and the BGRA fourcc.
    unsafe { video_out.setVideoSettings(Some(&settings)) };
    // SAFETY: Late samples are discarded to keep the native queue bounded.
    unsafe { video_out.setAlwaysDiscardsLateVideoFrames(true) };
    let queue = DispatchQueue::new("eu.nuschl.camera", None);
    // SAFETY: The typed delegate and its serial queue live until the session is stopped.
    unsafe {
        video_out
            .setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(output)), Some(&queue))
    };
    // SAFETY: The output was created by the same framework as the session.
    if !unsafe { session.canAddOutput(&video_out) } {
        return Err(io::Error::other("The camera output is unavailable"));
    }
    // SAFETY: AVFoundation accepted the output for this session.
    unsafe { session.addOutput(&video_out) };
    // SAFETY: The session is configured and remains on this thread.
    unsafe { session.startRunning() };
    let result = (|| {
        // SAFETY: The session is owned by this thread throughout capture.
        while unsafe { session.isRunning() } {
            if !output.deliver(on_frame)? {
                thread::sleep(POLL);
            }
        }
        Err(io::Error::other("The camera stopped"))
    })();
    // SAFETY: The session and delegate remain retained until capture is stopped.
    unsafe { session.stopRunning() };
    result
}

fn camera_consent(video: &objc2_av_foundation::AVMediaType) -> io::Result<()> {
    // SAFETY: `video` is the framework's retained video media type constant.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(video) };
    if status == AVAuthorizationStatus::Authorized {
        return Ok(());
    }
    if status == AVAuthorizationStatus::NotDetermined {
        let (tx, rx) = sync_channel(1);
        let callback = RcBlock::new(move |allowed: Bool| {
            let _ = tx.try_send(allowed.as_bool());
        });
        // SAFETY: The native completion block owns a thread-safe sender.
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &callback) };
        if matches!(rx.recv_timeout(CONSENT_TIMEOUT), Ok(true)) {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Camera access was declined",
    ))
}

// Own the picker result until the capture thread constructs the stream.
struct Picked(Retained<SCContentFilter>);

// SAFETY: The picker supplies the filter to create a stream. The delegate
// retains it and hands its retained reference to the main thread once. Only
// that thread reads the filter and no code mutates it.
unsafe impl Send for Picked {}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "NuschlMacCapturePickerObserver"]
    #[ivars = SyncSender<io::Result<Picked>>]
    struct PickerObserver;

    // SAFETY: The class inherits NSObject and uses its NSObjectProtocol methods.
    unsafe impl NSObjectProtocol for PickerObserver {}

    // SAFETY: These methods implement exactly the system picker's observer
    // signatures. The bounded sender can be called from any framework thread.
    unsafe impl SCContentSharingPickerObserver for PickerObserver {
        #[unsafe(method(contentSharingPicker:didCancelForStream:))]
        fn contentSharingPicker_didCancelForStream(
            &self,
            _: &SCContentSharingPicker,
            _: Option<&SCStream>,
        ) {
            let _ = self.ivars().try_send(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Screen sharing was cancelled",
            )));
        }

        #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
        fn contentSharingPicker_didUpdateWithFilter_forStream(
            &self,
            _: &SCContentSharingPicker,
            filter: &SCContentFilter,
            stream: Option<&SCStream>,
        ) {
            // This picker selects one source for a new stream.
            if stream.is_none() {
                let _ = self.ivars().try_send(Ok(Picked(filter.retain())));
            }
        }

        #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
        fn contentSharingPickerStartDidFailWithError(&self, _: &NSError) {
            let _ = self.ivars().try_send(Err(io::Error::other("The screen picker failed")));
        }
    }
);

struct Picker {
    picker: Retained<SCContentSharingPicker>,
    observer: Retained<PickerObserver>,
}

impl Picker {
    fn open() -> (Self, Receiver<io::Result<Picked>>) {
        let (tx, rx) = sync_channel(1);
        let observer = PickerObserver::alloc().set_ivars(tx);
        // SAFETY: The allocated object inherits NSObject's initializer.
        let observer: Retained<PickerObserver> = unsafe { msg_send![super(observer), init] };
        // SAFETY: `open` runs on the AppKit main thread.
        let picker = unsafe { SCContentSharingPicker::sharedPicker() };
        // SAFETY: The configuration is for one display or one window.
        let configuration = unsafe { SCContentSharingPickerConfiguration::new() };
        // SAFETY: Both flags are specified in the framework's picker modes.
        unsafe {
            configuration.setAllowedPickerModes(
                SCContentSharingPickerMode::SingleDisplay
                    | SCContentSharingPickerMode::SingleWindow,
            )
        };
        // SAFETY: A new share always begins with a new explicit selection.
        unsafe { configuration.setAllowsChangingSelectedContent(false) };
        // SAFETY: The object has the exact type constructed by ScreenCaptureKit.
        unsafe { picker.setDefaultConfiguration(&configuration) };
        // SAFETY: The observer remains retained until removed in Drop.
        unsafe { picker.addObserver(ProtocolObject::from_ref(&*observer)) };
        // SAFETY: Activation and presentation run on the AppKit main thread.
        unsafe { picker.setActive(true) };
        // SAFETY: The main-thread picker is active and has a retained observer.
        unsafe { picker.present() };
        (Self { picker, observer }, rx)
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        // SAFETY: This is the same retained observer registered by `open`.
        unsafe {
            self.picker
                .removeObserver(ProtocolObject::from_ref(&*self.observer))
        };
        // SAFETY: The picker is owned by this main-thread scope.
        unsafe { self.picker.setActive(false) };
    }
}

fn capture_screen(output: &FrameOutput, on_frame: &mut Callback<'_>) -> io::Result<()> {
    let main = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("The screen picker needs the main thread"))?;
    let app = NSApplication::sharedApplication(main);
    // A standalone helper becomes an AppKit application while its picker is
    // visible. The supervisor reaps this process when the share stops.
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Regular) {
        return Err(io::Error::other("The screen picker could not open"));
    }
    app.finishLaunching();
    let (_picker, selection) = Picker::open();
    let Picked(filter) = wait_for_picker(&app, &selection, CONSENT_TIMEOUT)??;
    // SAFETY: The immutable selected filter is retained while creating its stream.
    let size = unsafe { filter.contentRect() }.size;
    // SAFETY: `pointPixelScale` reads the backing scale of this selected filter.
    let scale = unsafe { filter.pointPixelScale() };
    let (width, height) = fit_screen(size.width, size.height, f64::from(scale))
        .ok_or_else(|| io::Error::other("The selected content has no usable dimensions"))?;
    // SAFETY: The configuration belongs to this thread until consumed by SCStream.
    let configuration = unsafe { SCStreamConfiguration::new() };
    // SAFETY: The width came from the bounded screen dimensions above.
    unsafe { configuration.setWidth(width) };
    // SAFETY: The height came from the bounded screen dimensions above.
    unsafe { configuration.setHeight(height) };
    // SAFETY: BGRA is a supported ScreenCaptureKit packed pixel format.
    unsafe { configuration.setPixelFormat(kCVPixelFormatType_32BGRA) };
    // SAFETY: Three is within the documented ScreenCaptureKit queue depth.
    unsafe { configuration.setQueueDepth(3) };
    // SAFETY: CoreMedia constructs a positive, valid frame interval.
    let interval = unsafe { CMTime::new(1, 30) };
    // SAFETY: This interval is a valid, positive CMTime.
    unsafe { configuration.setMinimumFrameInterval(interval) };
    // SAFETY: This video-only stream includes the pointer over the selected source.
    unsafe { configuration.setShowsCursor(true) };
    // SAFETY: Audio is outside this worker's capture role.
    unsafe { configuration.setCapturesAudio(false) };
    // SAFETY: The filter was granted by the system picker. Configuration and
    // delegate stay alive until the stream is stopped.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            Some(ProtocolObject::from_ref(output)),
        )
    };
    let queue = DispatchQueue::new("eu.nuschl.screenshare", None);
    // SAFETY: The serial queue delivers only screen samples to the retained delegate.
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|_| io::Error::other("The screen capture output could not start"))?;
    let (tx, rx) = sync_channel(1);
    let completion = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.try_send(error.is_null());
    });
    // SAFETY: ScreenCaptureKit copies the completion block. The block only tests
    // whether a callback argument is null and sends its boolean on a bounded channel.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&completion)) };
    let result = (|| {
        if !wait_for_picker(&app, &rx, Duration::from_secs(10))? {
            return Err(io::Error::other(
                "The selected screen or window is unavailable",
            ));
        }
        while !output.ivars().stopped.load(Ordering::Acquire) {
            output.deliver(on_frame)?;
            pump(&app);
        }
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Screen sharing stopped",
        ))
    })();
    // SAFETY: The stream and delegate remain retained while stop is requested.
    unsafe { stream.stopCaptureWithCompletionHandler(None) };
    result
}

fn wait_for_picker<T>(
    app: &NSApplication,
    result: &Receiver<T>,
    budget: Duration,
) -> io::Result<T> {
    let start = Instant::now();
    while start.elapsed() < budget {
        match result.try_recv() {
            Ok(value) => return Ok(value),
            Err(TryRecvError::Empty) => pump(app),
            Err(TryRecvError::Disconnected) => break,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Screen capture setup timed out",
    ))
}

fn pump(app: &NSApplication) {
    autoreleasepool(|_| {
        let deadline = NSDate::dateWithTimeIntervalSinceNow(POLL.as_secs_f64());
        // SAFETY: Foundation defines this static run-loop mode for AppKit events.
        let mode = unsafe { NSDefaultRunLoopMode };
        if let Some(event) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&deadline),
            mode,
            true,
        ) {
            app.sendEvent(&event);
        }
    });
}
