//! The v4l2loopback camera "Mixfeed" (`/dev/video10`). The name is part of
//! the microphone's name, "Mixfeed Mic": Chrome then shows both as one
//! device (same group ID), a webcam with a built-in microphone.
//!
//! mixfeedd is the writer: it sets the output format (1920x1080 YUYV, Rec.709)
//! and writes one black frame, so that the device announces itself as a
//! camera (v4l2loopback `exclusive_caps=1`). The NDI video receive
//! (`mixfeed-cam`) runs only while an application streams from the device:
//! v4l2loopback sends `V4L2_EVENT_PRI_CLIENT_USAGE` when the first reader
//! starts and the last one stops. While an application streams and no NDI
//! frame comes, frames go out at 5 fps so the picture does not time out: the
//! last picture again while the sender is connected (a still picture is sent
//! once), else black (no source, searching, lost).
//!
//! The device offers YUYV, the usual webcam format: Chrome's V4L2 capture,
//! for one, does not accept UYVY. NDI sends UYVY; the byte order is swapped
//! per pixel pair on the CPU. Frames of another size or in BGRA are
//! converted there too (nearest-neighbour scaling, BT.709 limited range).

use std::ffi::{CString, c_char, c_void};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mixfeed_cam::MixfeedVideoFrame;
use mixfeed_ctl::CameraStatus;

pub const DEVICE: &str = "/dev/video10";
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FRAME_BYTES: usize = (WIDTH * HEIGHT * 2) as usize;
const FOURCC_UYVY: u32 = u32::from_le_bytes(*b"UYVY");
const FOURCC_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
const FOURCC_BGRA: u32 = u32::from_le_bytes(*b"BGRA");
const FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");
/// Black in YUYV, video range.
const BLACK: [u8; 4] = [16, 128, 16, 128];
/// Placeholder frames start after this gap and repeat at this interval.
const PLACEHOLDER_AFTER: Duration = Duration::from_millis(300);
const PLACEHOLDER_EVERY: Duration = Duration::from_millis(200);

// --- v4l2 ABI (linux/videodev2.h, 64-bit) ---------------------------------

const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_REC709: u32 = 3;
const V4L2_YCBCR_ENC_709: u32 = 2;
const V4L2_QUANTIZATION_LIM_RANGE: u32 = 2;
const V4L2_XFER_FUNC_709: u32 = 1;
/// v4l2loopback private event: `count` is 1 while an application streams.
const V4L2_EVENT_PRI_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08E0_0000 + 1;

#[repr(C)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: the union is 200 bytes and 8-aligned.
#[repr(C)]
struct Format {
    type_: u32,
    _pad: u32,
    fmt: [u8; 200],
}

#[repr(C)]
struct EventSubscription {
    type_: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
struct Event {
    type_: u32,
    _pad: u32,
    u: [u8; 64],
    pending: u32,
    sequence: u32,
    timestamp: [i64; 2],
    id: u32,
    reserved: [u32; 8],
}

const _: () = assert!(size_of::<PixFormat>() == 48);
const _: () = assert!(size_of::<Format>() == 208);
const _: () = assert!(size_of::<EventSubscription>() == 32);
const _: () = assert!(size_of::<Event>() == 136);

// _IOWR('V', 5, v4l2_format), _IOW('V', 90, v4l2_event_subscription), _IOR('V', 89, v4l2_event)
const VIDIOC_S_FMT: libc::c_ulong = 0xc0d0_5605;
const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = 0x4020_565a;
const VIDIOC_DQEVENT: libc::c_ulong = 0x8088_5659;

fn ioctl<T>(fd: i32, req: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
    // SAFETY: arg is a valid, correctly sized struct for req (sizes asserted above).
    let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

// --- state ------------------------------------------------------------------

struct Device {
    file: File,
    /// One device frame, for converted frames and placeholders.
    scratch: Vec<u8>,
    last_write: Instant,
    last_ndi: Option<Instant>,
    /// The NDI connection of the picture in scratch (mixfeed_cam::connection).
    picture_of: Option<u64>,
}

static DEV: Mutex<Option<Device>> = Mutex::new(None);
/// An application streams from the camera.
static LIVE: AtomicBool = AtomicBool::new(false);
static FRAMES: AtomicU64 = AtomicU64::new(0);
/// Width << 32 | height of the last NDI frame, and its FourCC.
static SOURCE_SIZE: AtomicU64 = AtomicU64::new(0);
static SOURCE_FOURCC: AtomicU64 = AtomicU64::new(0);
/// Frame rate numerator << 32 | denominator of the last NDI frame.
static SOURCE_RATE: AtomicU64 = AtomicU64::new(0);

/// Why the camera is not available, when [`start`] failed.
static UNAVAILABLE: Mutex<Option<String>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Set up the camera. False when it is not available: the daemon goes on
/// without it (the microphone does not depend on it), and [`status`] says why.
pub fn start() -> bool {
    match spawn() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("camera: {e}; no camera");
            *lock(&UNAVAILABLE) = Some(e);
            false
        }
    }
}

/// Open the device, set the format, start the event thread.
fn spawn() -> Result<(), String> {
    if !std::path::Path::new(DEVICE).exists() {
        return Err(format!("v4l2loopback not loaded ({DEVICE} not found)"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEVICE)
        .map_err(|e| format!("cannot open {DEVICE}: {e}"))?;
    let fd = file.as_raw_fd();
    set_format(fd).map_err(|e| format!("cannot set the format of {DEVICE}: {e}"))?;
    let mut sub = EventSubscription {
        type_: V4L2_EVENT_PRI_CLIENT_USAGE,
        id: 0,
        flags: 0,
        reserved: [0; 5],
    };
    ioctl(fd, VIDIOC_SUBSCRIBE_EVENT, &mut sub).map_err(|e| format!("{DEVICE} sends no client events: {e}"))?;

    let mut dev = Device {
        file,
        scratch: vec![0; FRAME_BYTES],
        last_write: Instant::now(),
        last_ndi: None,
        picture_of: None,
    };
    // The device announces CAPTURE only after the writer has sent a frame.
    write_black(&mut dev).map_err(|e| format!("cannot write to {DEVICE}: {e}"))?;
    *lock(&DEV) = Some(dev);

    mixfeed_cam::mixfeed_cam_init(on_log, on_frame, std::ptr::null_mut());
    // The loop has no end; after a panic it starts again (else the camera
    // would no longer follow the applications that open it).
    let events = move || loop {
        if let Err(p) = std::panic::catch_unwind(|| event_loop(fd)) {
            let msg = p.downcast_ref::<&str>().map(|s| (*s).to_owned()).or_else(|| p.downcast_ref::<String>().cloned());
            eprintln!("camera: event loop panic: {}", msg.as_deref().unwrap_or("unknown"));
            std::thread::sleep(Duration::from_secs(1));
        }
    };
    if let Err(e) = std::thread::Builder::new().name("mixfeedd-camera".into()).spawn(events) {
        *lock(&DEV) = None;
        return Err(format!("camera thread: {e}"));
    }
    eprintln!("camera: {DEVICE} ready ({WIDTH}x{HEIGHT} YUYV)");
    Ok(())
}

fn set_format(fd: i32) -> std::io::Result<()> {
    let pix = PixFormat {
        width: WIDTH,
        height: HEIGHT,
        pixelformat: FOURCC_YUYV,
        field: V4L2_FIELD_NONE,
        bytesperline: WIDTH * 2,
        sizeimage: FRAME_BYTES as u32,
        colorspace: V4L2_COLORSPACE_REC709,
        priv_: 0,
        flags: 0,
        ycbcr_enc: V4L2_YCBCR_ENC_709,
        quantization: V4L2_QUANTIZATION_LIM_RANGE,
        xfer_func: V4L2_XFER_FUNC_709,
    };
    let mut f = Format {
        type_: V4L2_BUF_TYPE_VIDEO_OUTPUT,
        _pad: 0,
        fmt: [0; 200],
    };
    // SAFETY: PixFormat (48 bytes) fits in the 200-byte union; plain data.
    unsafe { std::ptr::write_unaligned(f.fmt.as_mut_ptr().cast::<PixFormat>(), pix) };
    ioctl(fd, VIDIOC_S_FMT, &mut f)
}

/// Give the camera the same NDI source as the microphone.
pub fn set_source(name: &str) {
    if let Ok(c) = CString::new(name) {
        // SAFETY: c is a valid NUL-terminated string for the call.
        unsafe { mixfeed_cam::mixfeed_cam_set_source(c.as_ptr()) };
    }
}

pub fn status() -> Option<CameraStatus> {
    if let Some(e) = lock(&UNAVAILABLE).clone() {
        return Some(CameraStatus { device: DEVICE.into(), error: Some(e), ..Default::default() });
    }
    lock(&DEV).as_ref()?;
    let mut buf = [0 as c_char; 512];
    // SAFETY: buf has 512 writable bytes.
    unsafe { mixfeed_cam::mixfeed_cam_status(buf.as_mut_ptr(), buf.len()) };
    // SAFETY: mixfeed_cam_status writes a NUL-terminated string.
    let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    let size = SOURCE_SIZE.load(Ordering::Relaxed);
    let cc = (SOURCE_FOURCC.load(Ordering::Relaxed) as u32).to_le_bytes();
    let rate = SOURCE_RATE.load(Ordering::Relaxed);
    Some(CameraStatus {
        device: DEVICE.into(),
        live: LIVE.load(Ordering::Relaxed),
        text,
        frames: FRAMES.load(Ordering::Relaxed),
        source_width: (size >> 32) as u32,
        source_height: size as u32,
        source_fourcc: if cc == [0; 4] { String::new() } else { String::from_utf8_lossy(&cc).into_owned() },
        source_rate_n: (rate >> 32) as u32,
        source_rate_d: rate as u32,
        error: None,
    })
}

// --- events and placeholders -------------------------------------------------

fn event_loop(fd: i32) {
    loop {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut p, 1, PLACEHOLDER_EVERY.as_millis() as i32) };
        if n > 0 && p.revents & libc::POLLPRI != 0 {
            drain_events(fd);
        }
        if LIVE.load(Ordering::Relaxed) {
            placeholder_if_idle();
        }
    }
}

fn drain_events(fd: i32) {
    loop {
        let mut ev = Event {
            type_: 0,
            _pad: 0,
            u: [0; 64],
            pending: 0,
            sequence: 0,
            timestamp: [0; 2],
            id: 0,
            reserved: [0; 8],
        };
        if ioctl(fd, VIDIOC_DQEVENT, &mut ev).is_err() {
            return;
        }
        if ev.type_ == V4L2_EVENT_PRI_CLIENT_USAGE {
            let count = u32::from_ne_bytes([ev.u[0], ev.u[1], ev.u[2], ev.u[3]]);
            let live = count > 0;
            if LIVE.swap(live, Ordering::SeqCst) != live {
                eprintln!("camera: {}", if live { "application streams" } else { "no application streams" });
                if live {
                    // The size of a former session's source would show until the first frame.
                    SOURCE_SIZE.store(0, Ordering::Relaxed);
                    SOURCE_FOURCC.store(0, Ordering::Relaxed);
                    SOURCE_RATE.store(0, Ordering::Relaxed);
                    mixfeed_cam::mixfeed_cam_start();
                } else {
                    mixfeed_cam::mixfeed_cam_stop();
                }
            }
        }
        if ev.pending == 0 {
            return;
        }
    }
}

fn placeholder_if_idle() {
    let mut g = lock(&DEV);
    let Some(dev) = g.as_mut() else { return };
    let ndi_recent = dev.last_ndi.is_some_and(|t| t.elapsed() < PLACEHOLDER_AFTER);
    if ndi_recent || dev.last_write.elapsed() < PLACEHOLDER_EVERY {
        return;
    }
    // scratch holds the last picture until the first black frame.
    let r = if dev.picture_of.is_some() && dev.picture_of == mixfeed_cam::connection() {
        write_scratch(dev)
    } else {
        dev.picture_of = None;
        write_black(dev)
    };
    if let Err(e) = r {
        eprintln!("camera: placeholder: {e}");
    }
}

fn write_black(dev: &mut Device) -> std::io::Result<()> {
    for px in dev.scratch.as_chunks_mut::<4>().0 {
        *px = BLACK;
    }
    write_scratch(dev)
}

fn write_scratch(dev: &mut Device) -> std::io::Result<()> {
    dev.file.write_all(&dev.scratch)?;
    dev.last_write = Instant::now();
    FRAMES.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

// --- mixfeed-cam callbacks -----------------------------------------------------

extern "C" fn on_log(msg: *const c_char) {
    if !msg.is_null() {
        // SAFETY: mixfeed-cam passes a valid NUL-terminated string.
        eprintln!("camera: {}", unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy());
    }
}

/// Called on the NDI receive thread for each frame; the pixels are valid
/// only during the call. A panic must not leave this `extern "C"` function:
/// that aborts the daemon, microphone included.
extern "C" fn on_frame(_ctx: *mut c_void, f: *const MixfeedVideoFrame) {
    // SAFETY: mixfeed-cam passes a valid frame for the duration of the call.
    let f = unsafe { &*f };
    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_frame(f))) {
        let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned());
        eprintln!("camera: frame dropped after a panic: {}", msg.unwrap_or_default());
    }
}

fn write_frame(f: &MixfeedVideoFrame) {
    // mixfeed-cam passes only well-formed frames (known format, full lines).
    if f.data.is_null() || f.width == 0 || f.height == 0 {
        return;
    }
    SOURCE_SIZE.store(u64::from(f.width) << 32 | u64::from(f.height), Ordering::Relaxed);
    SOURCE_FOURCC.store(u64::from(f.fourcc), Ordering::Relaxed);
    SOURCE_RATE.store(u64::from(f.rate_n) << 32 | u64::from(f.rate_d), Ordering::Relaxed);
    // SAFETY: NDI gives `height` lines of `stride` bytes.
    let src = unsafe { std::slice::from_raw_parts(f.data, f.stride as usize * f.height as usize) };

    let mut g = lock(&DEV);
    let Some(dev) = g.as_mut() else { return };
    match f.fourcc {
        FOURCC_UYVY => uyvy_to_yuyv(src, f, &mut dev.scratch),
        FOURCC_BGRA | FOURCC_BGRX => bgra_to_yuyv(src, f, &mut dev.scratch),
        _ => return,
    }
    let ok = dev.file.write_all(&dev.scratch);
    match ok {
        Ok(()) => {
            dev.last_write = Instant::now();
            dev.last_ndi = Some(dev.last_write);
            dev.picture_of = mixfeed_cam::connection();
            FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => eprintln!("camera: write: {e}"),
    }
}

/// UYVY to YUYV: swap the bytes of each macropixel (two pixels sharing U and
/// V), with nearest-neighbour scaling when the size differs.
fn uyvy_to_yuyv(src: &[u8], f: &MixfeedVideoFrame, dst: &mut [u8]) {
    let (sw2, sh) = ((f.width / 2) as usize, f.height as usize);
    let (dw2, dh) = ((WIDTH / 2) as usize, HEIGHT as usize);
    let same = sw2 == dw2 && sh == dh;
    for y in 0..dh {
        let sy = if same { y } else { y * sh / dh };
        let srow = &src[sy * f.stride as usize..];
        let drow = &mut dst[y * dw2 * 4..(y + 1) * dw2 * 4];
        for (x, d) in drow.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let sx = if same { x } else { x * sw2 / dw2 } * 4;
            let [u, y0, v, y1] = [srow[sx], srow[sx + 1], srow[sx + 2], srow[sx + 3]];
            *d = [y0, u, y1, v];
        }
    }
}

/// BGRA to YUYV (BT.709, limited range) with nearest-neighbour scaling.
fn bgra_to_yuyv(src: &[u8], f: &MixfeedVideoFrame, dst: &mut [u8]) {
    let (sw, sh) = (f.width as usize, f.height as usize);
    let (dw, dh) = (WIDTH as usize, HEIGHT as usize);
    let px = |row: &[u8], x: usize| {
        let p = &row[(x * sw / dw) * 4..];
        (f32::from(p[2]), f32::from(p[1]), f32::from(p[0]))
    };
    let luma = |(r, g, b): (f32, f32, f32)| 16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b;
    for y in 0..dh {
        let srow = &src[(y * sh / dh) * f.stride as usize..];
        let drow = &mut dst[y * dw * 2..(y + 1) * dw * 2];
        for x in (0..dw).step_by(2) {
            let (a, b) = (px(srow, x), px(srow, x + 1));
            let (r, g, bl) = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0, (a.2 + b.2) / 2.0);
            let u = 128.0 - 0.1006 * r - 0.3386 * g + 0.4392 * bl;
            let v = 128.0 + 0.4392 * r - 0.3989 * g - 0.0403 * bl;
            drow[x * 2..x * 2 + 4].copy_from_slice(&[luma(a) as u8, u as u8, luma(b) as u8, v as u8]);
        }
    }
}
