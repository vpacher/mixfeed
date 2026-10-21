//! Thin runtime binding to the NDI SDK (`libndi`): source discovery, audio
//! receive and video receive.
//!
//! The library is not bundled. It is loaded at runtime from the user's NDI
//! installation (see [`library_candidates`]). Struct layouts follow the public
//! NDI SDK headers (MIT licensed, copyright Vizrt NDI AB; see `NOTICE`).

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use libloading::Library;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no NDI library found; tried: {0:?}")]
    LibraryNotFound(Vec<PathBuf>),
    #[error("NDI library {path} does not load: {reason}")]
    LoadFailed { path: PathBuf, reason: String },
    #[error("NDI library {path}: missing symbol {symbol}")]
    MissingSymbol { path: PathBuf, symbol: &'static str },
    #[error("NDIlib_initialize failed (CPU not supported?)")]
    InitializeFailed,
    #[error("NDI {0} failed")]
    CallFailed(&'static str),
    #[error("no NDI source matching {wanted:?} within {waited:?}; seen: {seen:?}")]
    SourceNotFound {
        wanted: String,
        waited: Duration,
        seen: Vec<String>,
    },
}

impl Error {
    /// Short text for a user (status line, menu, bar widget); the `Display`
    /// text has the details for the log.
    pub fn summary(&self) -> String {
        match self {
            Error::LibraryNotFound(_) => "NDI Runtime not installed".into(),
            Error::LoadFailed { .. } => "NDI Runtime does not load".into(),
            Error::MissingSymbol { .. } => "NDI Runtime too old (a function is missing)".into(),
            Error::InitializeFailed => "NDI Runtime cannot start on this computer".into(),
            other => other.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// FourCC of planar 32-bit float audio (`NDIlib_FourCC_audio_type_FLTP`).
pub const FOURCC_FLTP: u32 = u32::from_le_bytes(*b"FLTp");

/// FourCC of 4:2:2 video, U0 Y0 V0 Y1 (`NDIlib_FourCC_video_type_UYVY`).
pub const FOURCC_UYVY: u32 = u32::from_le_bytes(*b"UYVY");
/// FourCC of 8-bit BGRA video (`NDIlib_FourCC_video_type_BGRA`).
pub const FOURCC_BGRA: u32 = u32::from_le_bytes(*b"BGRA");
/// FourCC of 8-bit BGRX video (`NDIlib_FourCC_video_type_BGRX`).
pub const FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX");

const FRAME_TYPE_VIDEO: c_int = 1;
const FRAME_TYPE_AUDIO: c_int = 2;
const FRAME_TYPE_ERROR: c_int = 4;
const RECV_BANDWIDTH_AUDIO_ONLY: c_int = 10;
const RECV_BANDWIDTH_HIGHEST: c_int = 100;
const RECV_COLOR_FORMAT_FASTEST: c_int = 100;
/// UYVY without alpha, BGRA with alpha.
const RECV_COLOR_FORMAT_UYVY_BGRA: c_int = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct RawSource {
    p_ndi_name: *const c_char,
    p_url_address: *const c_char,
}

#[repr(C)]
struct RawFindCreate {
    show_local_sources: bool,
    p_groups: *const c_char,
    p_extra_ips: *const c_char,
}

#[repr(C)]
struct RawRecvCreateV3 {
    source_to_connect_to: RawSource,
    color_format: c_int,
    bandwidth: c_int,
    allow_video_fields: bool,
    p_ndi_recv_name: *const c_char,
}

#[repr(C)]
struct RawAudioFrameV3 {
    sample_rate: c_int,
    no_channels: c_int,
    no_samples: c_int,
    timecode: i64,
    four_cc: u32,
    p_data: *mut u8,
    channel_stride_in_bytes: c_int,
    p_metadata: *const c_char,
    timestamp: i64,
}

impl Default for RawAudioFrameV3 {
    fn default() -> Self {
        Self {
            sample_rate: 0,
            no_channels: 0,
            no_samples: 0,
            timecode: 0,
            four_cc: 0,
            p_data: ptr::null_mut(),
            channel_stride_in_bytes: 0,
            p_metadata: ptr::null(),
            timestamp: 0,
        }
    }
}

#[repr(C)]
struct RawVideoFrameV2 {
    xres: c_int,
    yres: c_int,
    four_cc: u32,
    frame_rate_n: c_int,
    frame_rate_d: c_int,
    picture_aspect_ratio: f32,
    frame_format_type: c_int,
    timecode: i64,
    p_data: *mut u8,
    /// Union with `data_size_in_bytes` (compressed formats only).
    line_stride_in_bytes: c_int,
    p_metadata: *const c_char,
    timestamp: i64,
}

impl Default for RawVideoFrameV2 {
    fn default() -> Self {
        Self {
            xres: 0,
            yres: 0,
            four_cc: 0,
            frame_rate_n: 0,
            frame_rate_d: 0,
            picture_aspect_ratio: 0.0,
            frame_format_type: 0,
            timecode: 0,
            p_data: ptr::null_mut(),
            line_stride_in_bytes: 0,
            p_metadata: ptr::null(),
            timestamp: 0,
        }
    }
}

type FnInitialize = unsafe extern "C" fn() -> bool;
type FnDestroy = unsafe extern "C" fn();
type FnVersion = unsafe extern "C" fn() -> *const c_char;
type FnFindCreate = unsafe extern "C" fn(*const RawFindCreate) -> *mut c_void;
type FnFindWait = unsafe extern "C" fn(*mut c_void, u32) -> bool;
type FnFindGet = unsafe extern "C" fn(*mut c_void, *mut u32) -> *const RawSource;
type FnFindDestroy = unsafe extern "C" fn(*mut c_void);
type FnRecvCreate = unsafe extern "C" fn(*const RawRecvCreateV3) -> *mut c_void;
type FnRecvCapture =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut RawAudioFrameV3, *mut c_void, u32) -> c_int;
type FnRecvFreeAudio = unsafe extern "C" fn(*mut c_void, *const RawAudioFrameV3);
type FnRecvFreeVideo = unsafe extern "C" fn(*mut c_void, *const RawVideoFrameV2);
type FnRecvDestroy = unsafe extern "C" fn(*mut c_void);
type FnRecvConnections = unsafe extern "C" fn(*mut c_void) -> c_int;

struct Api {
    _lib: Library,
    path: PathBuf,
    destroy: FnDestroy,
    version: FnVersion,
    find_create: FnFindCreate,
    find_wait: FnFindWait,
    find_get: FnFindGet,
    find_destroy: FnFindDestroy,
    recv_create: FnRecvCreate,
    recv_capture: FnRecvCapture,
    recv_free_audio: FnRecvFreeAudio,
    recv_free_video: FnRecvFreeVideo,
    recv_destroy: FnRecvDestroy,
    recv_connections: FnRecvConnections,
}

impl Drop for Api {
    fn drop(&mut self) {
        // SAFETY: NDIlib_initialize succeeded when this Api was built; all
        // finders and receivers hold an Arc<Api>, so none is alive here.
        unsafe { (self.destroy)() }
    }
}

/// Places where an installed NDI runtime may be. `MIXFEED_LIBNDI` wins.
pub fn library_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("MIXFEED_LIBNDI") {
        v.push(PathBuf::from(p));
    }
    if let Ok(dir) = std::env::var("NDI_RUNTIME_DIR_V6") {
        v.push(PathBuf::from(dir).join("libndi.dylib"));
    }
    for p in [
        "/usr/local/lib/libndi.dylib",
        // Root-owned copy of the runtime above, made by the agent: the sandbox
        // of the camera extension denies /usr/local (one deny log line per load).
        "/Library/Application Support/Mixfeed/lib/libndi.dylib",
        "/Library/NDI SDK for Apple/lib/macOS/libndi.dylib",
        "/Applications/NDIScanConverter.app/Contents/Frameworks/libndi.dylib",
        "/usr/local/lib/libndi.so.6",
        "/usr/lib/libndi.so.6",
    ] {
        v.push(PathBuf::from(p));
    }
    v
}

/// A loaded and initialised NDI runtime.
#[derive(Clone)]
pub struct Ndi {
    api: Arc<Api>,
}

macro_rules! sym {
    ($lib:expr, $path:expr, $name:literal) => {{
        // SAFETY: the symbol type matches the NDI SDK header declaration.
        let s = unsafe { $lib.get(concat!($name, "\0").as_bytes()) }.map_err(|_| {
            Error::MissingSymbol {
                path: $path.clone(),
                symbol: $name,
            }
        })?;
        *s
    }};
}

impl Ndi {
    /// The NDI runtime of this process, loaded on the first successful call.
    /// A failure is not kept: a later call tries again, so a runtime that is
    /// installed later is used without a restart. Callers wait between tries.
    pub fn shared() -> Result<&'static Ndi> {
        static SHARED: OnceLock<Ndi> = OnceLock::new();
        static LOADING: Mutex<()> = Mutex::new(());
        if let Some(n) = SHARED.get() {
            return Ok(n);
        }
        // One load at a time: a second Ndi would initialise the SDK twice.
        let _g = LOADING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = SHARED.get() {
            return Ok(n);
        }
        let n = Ndi::load()?;
        Ok(SHARED.get_or_init(|| n))
    }

    /// Load the first NDI library from [`library_candidates`] and initialise it.
    pub fn load() -> Result<Self> {
        let tried = library_candidates();
        // A library that is there but does not load (other architecture,
        // missing dependency) is reported instead of "not found".
        let mut failed = None;
        for path in &tried {
            if !path.exists() {
                continue;
            }
            // SAFETY: loading a shared library runs its initialisers; libndi is
            // a well-known vendor library.
            match unsafe { Library::new(path) } {
                Ok(lib) => return Self::from_library(lib, path.clone()),
                Err(e) => {
                    failed.get_or_insert(Error::LoadFailed { path: path.clone(), reason: e.to_string() });
                }
            }
        }
        Err(failed.unwrap_or(Error::LibraryNotFound(tried)))
    }

    fn from_library(lib: Library, path: PathBuf) -> Result<Self> {
        let initialize: FnInitialize = sym!(lib, path, "NDIlib_initialize");
        let api = Api {
            path: path.clone(),
            destroy: sym!(lib, path, "NDIlib_destroy"),
            version: sym!(lib, path, "NDIlib_version"),
            find_create: sym!(lib, path, "NDIlib_find_create_v2"),
            find_wait: sym!(lib, path, "NDIlib_find_wait_for_sources"),
            find_get: sym!(lib, path, "NDIlib_find_get_current_sources"),
            find_destroy: sym!(lib, path, "NDIlib_find_destroy"),
            recv_create: sym!(lib, path, "NDIlib_recv_create_v3"),
            recv_capture: sym!(lib, path, "NDIlib_recv_capture_v3"),
            recv_free_audio: sym!(lib, path, "NDIlib_recv_free_audio_v3"),
            recv_free_video: sym!(lib, path, "NDIlib_recv_free_video_v2"),
            recv_destroy: sym!(lib, path, "NDIlib_recv_destroy"),
            recv_connections: sym!(lib, path, "NDIlib_recv_get_no_connections"),
            _lib: lib,
        };
        // SAFETY: plain C call without arguments.
        if !unsafe { initialize() } {
            return Err(Error::InitializeFailed);
        }
        Ok(Self { api: Arc::new(api) })
    }

    pub fn library_path(&self) -> &std::path::Path {
        &self.api.path
    }

    pub fn version(&self) -> String {
        // SAFETY: returns a static NUL-terminated string.
        unsafe { CStr::from_ptr((self.api.version)()) }
            .to_string_lossy()
            .into_owned()
    }

    /// Create a finder that keeps browsing until it is dropped.
    pub fn finder(&self) -> Result<Finder> {
        let fc = RawFindCreate {
            show_local_sources: true,
            p_groups: ptr::null(),
            p_extra_ips: ptr::null(),
        };
        // SAFETY: fc is valid for the call; the SDK copies what it needs.
        let finder = unsafe { (self.api.find_create)(&fc) };
        if finder.is_null() {
            return Err(Error::CallFailed("find_create_v2"));
        }
        Ok(Finder {
            api: self.api.clone(),
            finder,
        })
    }

    /// Wait until the source for `name_part` appears (see [`pick_source`]),
    /// with a new finder. For repeated searches keep a [`Finder`] and use
    /// [`Finder::find`]: each new finder starts the network discovery again.
    pub fn find_source(&self, name_part: &str, timeout: Duration) -> Result<Source> {
        self.finder()?.find(name_part, timeout)
    }

    /// Connect an audio-only receiver (no frame sync) to `source`.
    pub fn audio_receiver(&self, source: &Source, recv_name: &str) -> Result<AudioReceiver> {
        let recv = self.recv_create(
            source,
            recv_name,
            RECV_COLOR_FORMAT_FASTEST,
            RECV_BANDWIDTH_AUDIO_ONLY,
        )?;
        Ok(AudioReceiver {
            api: self.api.clone(),
            recv,
        })
    }

    /// Connect a video receiver (full bandwidth, UYVY, no frame sync) to
    /// `source`. Audio and metadata are not requested.
    pub fn video_receiver(&self, source: &Source, recv_name: &str) -> Result<VideoReceiver> {
        let recv = self.recv_create(
            source,
            recv_name,
            RECV_COLOR_FORMAT_UYVY_BGRA,
            RECV_BANDWIDTH_HIGHEST,
        )?;
        Ok(VideoReceiver {
            api: self.api.clone(),
            recv,
        })
    }

    fn recv_create(
        &self,
        source: &Source,
        recv_name: &str,
        color_format: c_int,
        bandwidth: c_int,
    ) -> Result<*mut c_void> {
        let name = CString::new(source.name.as_str()).expect("source name has no NUL");
        let url = CString::new(source.url.as_str()).expect("url has no NUL");
        let rname = CString::new(recv_name).expect("receiver name has no NUL");
        let rc = RawRecvCreateV3 {
            source_to_connect_to: RawSource {
                p_ndi_name: name.as_ptr(),
                p_url_address: if source.url.is_empty() {
                    ptr::null()
                } else {
                    url.as_ptr()
                },
            },
            color_format,
            bandwidth,
            allow_video_fields: false,
            p_ndi_recv_name: rname.as_ptr(),
        };
        // SAFETY: rc and the CStrings it points to live for the call.
        let recv = unsafe { (self.api.recv_create)(&rc) };
        if recv.is_null() {
            return Err(Error::CallFailed("recv_create_v3"));
        }
        Ok(recv)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub url: String,
}

/// A running NDI source discovery.
pub struct Finder {
    api: Arc<Api>,
    finder: *mut c_void,
}

// SAFETY: a finder may be used from another thread, but not from two at once;
// Finder is not Sync, and its methods take &mut self.
unsafe impl Send for Finder {}

/// The source for `name_part`: the source with exactly this name, else the
/// first whose name contains it. NDI does not authenticate senders, so any
/// device can announce a name that contains the chosen one ("X (MIXER (OBS))"
/// contains "MIXER (OBS)"); an exact name chosen from the list must win.
pub fn pick_source<'a>(seen: &'a [Source], name_part: &str) -> Option<&'a Source> {
    seen.iter()
        .find(|s| s.name == name_part)
        .or_else(|| seen.iter().find(|s| s.name.contains(name_part)))
}

impl Finder {
    /// Wait until the source for `name_part` appears (see [`pick_source`]).
    pub fn find(&mut self, name_part: &str, timeout: Duration) -> Result<Source> {
        let start = Instant::now();
        loop {
            let seen = self.sources();
            if let Some(s) = pick_source(&seen, name_part) {
                return Ok(s.clone());
            }
            if start.elapsed() >= timeout {
                return Err(Error::SourceNotFound {
                    wanted: name_part.into(),
                    waited: timeout,
                    seen: seen.into_iter().map(|s| s.name).collect(),
                });
            }
            self.wait(1000);
        }
    }

    /// Wait up to `timeout_ms` for a change of the source list. True if it changed.
    pub fn wait(&mut self, timeout_ms: u32) -> bool {
        // SAFETY: finder is a live finder instance.
        unsafe { (self.api.find_wait)(self.finder, timeout_ms) }
    }

    /// The sources seen so far.
    pub fn sources(&mut self) -> Vec<Source> {
        let mut n = 0u32;
        // SAFETY: the returned array is valid until the next call on finder.
        let arr = unsafe { (self.api.find_get)(self.finder, &mut n) };
        let text = |p: *const c_char| {
            if p.is_null() {
                String::new()
            } else {
                // SAFETY: strings are NUL-terminated and owned by the finder.
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        };
        (0..n as usize)
            .map(|i| {
                // SAFETY: i < n.
                let s = unsafe { *arr.add(i) };
                Source {
                    name: text(s.p_ndi_name),
                    url: text(s.p_url_address),
                }
            })
            .collect()
    }
}

impl Drop for Finder {
    fn drop(&mut self) {
        // SAFETY: finder is live and dropped exactly once.
        unsafe { (self.api.find_destroy)(self.finder) }
    }
}

/// One received audio frame, deinterleaved into owned planes.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sample_rate: u32,
    /// NDI timestamp in 100 ns units (sender clock), or i64::MAX if not set.
    pub timestamp: i64,
    /// NDI timecode in 100 ns units.
    pub timecode: i64,
    pub four_cc: u32,
    /// One Vec per channel, `samples` long.
    pub planes: Vec<Vec<f32>>,
}

impl AudioFrame {
    pub fn samples(&self) -> usize {
        self.planes.first().map_or(0, Vec::len)
    }
}

pub enum Capture {
    Audio(AudioFrame),
    Nothing,
}

pub struct AudioReceiver {
    api: Arc<Api>,
    recv: *mut c_void,
}

// SAFETY: an NDI receiver may be used from another thread, but not from two at once;
// AudioReceiver is not Sync, and capture takes &mut self.
unsafe impl Send for AudioReceiver {}

impl AudioReceiver {
    /// Wait up to `timeout_ms` for the next frame. Video and metadata are not requested.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<Capture> {
        let mut f = RawAudioFrameV3::default();
        // SAFETY: recv is live; f is a valid out-parameter; null video/metadata pointers are allowed.
        let t = unsafe {
            (self.api.recv_capture)(
                self.recv,
                ptr::null_mut(),
                &mut f,
                ptr::null_mut(),
                timeout_ms,
            )
        };
        match t {
            FRAME_TYPE_AUDIO => {
                let nch = if f.p_data.is_null() { 0 } else { f.no_channels.max(0) as usize };
                let ns = f.no_samples.max(0) as usize;
                let stride = f.channel_stride_in_bytes.max(0) as usize;
                let mut planes = Vec::with_capacity(nch);
                for c in 0..nch {
                    // SAFETY: planar float data, each plane `stride` bytes apart, `ns` samples valid,
                    // data valid until recv_free_audio_v3 below.
                    let plane = unsafe {
                        std::slice::from_raw_parts(f.p_data.add(c * stride) as *const f32, ns)
                    };
                    planes.push(plane.to_vec());
                }
                let frame = AudioFrame {
                    sample_rate: f.sample_rate.max(0) as u32,
                    timestamp: f.timestamp,
                    timecode: f.timecode,
                    four_cc: f.four_cc,
                    planes,
                };
                // SAFETY: frees the frame captured above, exactly once.
                unsafe { (self.api.recv_free_audio)(self.recv, &f) };
                Ok(Capture::Audio(frame))
            }
            FRAME_TYPE_ERROR => Err(Error::CallFailed("recv_capture_v3 (connection lost)")),
            _ => Ok(Capture::Nothing),
        }
    }
}

impl AudioReceiver {
    /// Number of live connections to the sender; 0 when it has gone.
    pub fn connections(&self) -> u32 {
        // SAFETY: recv is live.
        unsafe { (self.api.recv_connections)(self.recv) }.max(0) as u32
    }
}

impl Drop for AudioReceiver {
    fn drop(&mut self) {
        // SAFETY: recv is live and dropped exactly once.
        unsafe { (self.api.recv_destroy)(self.recv) }
    }
}

pub struct VideoReceiver {
    api: Arc<Api>,
    recv: *mut c_void,
}

// SAFETY: as for AudioReceiver.
unsafe impl Send for VideoReceiver {}

/// Bytes per pixel of the formats a receiver asks for (UYVY, BGRA, BGRX).
pub fn video_bytes_per_pixel(four_cc: u32) -> Option<usize> {
    match four_cc {
        FOURCC_UYVY => Some(2),
        FOURCC_BGRA | FOURCC_BGRX => Some(4),
        _ => None,
    }
}

/// True for a known format with a full row in each line (`stride >= width *
/// bytes per pixel`, stride fits u32). Consumers read `width` pixels per line
/// (in Rust, or in VideoToolbox from a raw pointer), so a frame that fails
/// this must not reach them.
pub fn video_frame_well_formed(four_cc: u32, width: u32, height: u32, stride: usize) -> bool {
    let Some(bpp) = video_bytes_per_pixel(four_cc) else { return false };
    width > 0 && height > 0 && u32::try_from(stride).is_ok() && stride >= width as usize * bpp
}

/// One received video frame. The pixels stay in NDI's buffer and are given
/// back when the frame is dropped, so a consumer can copy them once, straight
/// into its own buffer.
pub struct VideoFrame<'a> {
    rx: &'a VideoReceiver,
    raw: RawVideoFrameV2,
}

impl VideoFrame<'_> {
    pub fn width(&self) -> u32 {
        self.raw.xres.max(0) as u32
    }
    pub fn height(&self) -> u32 {
        self.raw.yres.max(0) as u32
    }
    /// [`FOURCC_UYVY`], or BGRA when the source sends alpha.
    pub fn four_cc(&self) -> u32 {
        self.raw.four_cc
    }
    /// Bytes from one line to the next.
    pub fn stride(&self) -> usize {
        self.raw.line_stride_in_bytes.max(0) as usize
    }
    /// Frame rate as numerator and denominator.
    pub fn frame_rate(&self) -> (u32, u32) {
        (
            self.raw.frame_rate_n.max(0) as u32,
            self.raw.frame_rate_d.max(0) as u32,
        )
    }
    /// NDI timecode in 100 ns units.
    pub fn timecode(&self) -> i64 {
        self.raw.timecode
    }
    /// NDI timestamp in 100 ns units (sender clock), or i64::MAX if not set.
    pub fn timestamp(&self) -> i64 {
        self.raw.timestamp
    }
    /// True for progressive frames (fields are not requested).
    pub fn progressive(&self) -> bool {
        self.raw.frame_format_type == 1
    }
    /// True when the frame has pixels in a known format and each line holds
    /// a full row (see [`video_frame_well_formed`]).
    pub fn well_formed(&self) -> bool {
        !self.raw.p_data.is_null()
            && video_frame_well_formed(self.raw.four_cc, self.width(), self.height(), self.stride())
    }

    /// The pixels: `height` lines of `stride` bytes.
    pub fn data(&self) -> &[u8] {
        if self.raw.p_data.is_null() {
            return &[];
        }
        // SAFETY: NDI gives `yres` lines of `line_stride_in_bytes` bytes, valid
        // until recv_free_video_v2 in drop.
        unsafe { std::slice::from_raw_parts(self.raw.p_data, self.stride() * self.height() as usize) }
    }
}

impl Drop for VideoFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: frees the frame captured into raw, exactly once.
        unsafe { (self.rx.api.recv_free_video)(self.rx.recv, &self.raw) }
    }
}

pub enum VideoCapture<'a> {
    Video(VideoFrame<'a>),
    Nothing,
}

impl VideoReceiver {
    /// Wait up to `timeout_ms` for the next frame.
    pub fn capture(&mut self, timeout_ms: u32) -> Result<VideoCapture<'_>> {
        let mut raw = RawVideoFrameV2::default();
        // SAFETY: recv is live; raw is a valid out-parameter; null audio/metadata pointers are allowed.
        let t = unsafe {
            (self.api.recv_capture)(
                self.recv,
                (&mut raw as *mut RawVideoFrameV2).cast(),
                ptr::null_mut(),
                ptr::null_mut(),
                timeout_ms,
            )
        };
        match t {
            FRAME_TYPE_VIDEO => Ok(VideoCapture::Video(VideoFrame { rx: self, raw })),
            FRAME_TYPE_ERROR => Err(Error::CallFailed("recv_capture_v3 (connection lost)")),
            _ => Ok(VideoCapture::Nothing),
        }
    }

    /// Number of live connections to the sender; 0 when it has gone.
    pub fn connections(&self) -> u32 {
        // SAFETY: recv is live.
        unsafe { (self.api.recv_connections)(self.recv) }.max(0) as u32
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        // SAFETY: recv is live and dropped exactly once; frames borrow the
        // receiver, so none is alive here.
        unsafe { (self.api.recv_destroy)(self.recv) }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn video_frame_checks_format_and_line_length() {
        use super::{FOURCC_BGRA, FOURCC_FLTP, FOURCC_UYVY, video_frame_well_formed as ok};
        assert!(ok(FOURCC_UYVY, 1920, 1080, 3840));
        assert!(ok(FOURCC_UYVY, 1920, 1080, 4096)); // padded lines
        assert!(!ok(FOURCC_UYVY, 1920, 1080, 3838)); // line too short
        assert!(ok(FOURCC_BGRA, 1280, 720, 5120));
        assert!(!ok(FOURCC_BGRA, 1280, 720, 2560)); // UYVY stride for BGRA
        assert!(!ok(FOURCC_FLTP, 1920, 1080, 3840)); // not a video format
        assert!(!ok(FOURCC_UYVY, 0, 1080, 3840));
        assert!(!ok(FOURCC_UYVY, 1920, 1080, 1 << 33)); // stride beyond u32
    }

    #[test]
    fn exact_name_wins_over_a_longer_match() {
        let src = |n: &str| super::Source { name: n.into(), url: String::new() };
        let seen = [src("X (MIXER (OBS))"), src("MIXER (OBS)"), src("MIXER (OBS 2)")];
        assert_eq!(super::pick_source(&seen, "MIXER (OBS)").unwrap().name, "MIXER (OBS)");
        // A name part still finds the first source that contains it.
        assert_eq!(super::pick_source(&seen, "OBS 2").unwrap().name, "MIXER (OBS 2)");
        assert_eq!(super::pick_source(&seen, "MIXER").unwrap().name, "X (MIXER (OBS))");
        assert!(super::pick_source(&seen, "VMIX").is_none());
    }

    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn audio_frame_v3_layout_matches_sdk_header_on_64_bit() {
        assert_eq!(offset_of!(RawAudioFrameV3, timecode), 16);
        assert_eq!(offset_of!(RawAudioFrameV3, four_cc), 24);
        assert_eq!(offset_of!(RawAudioFrameV3, p_data), 32);
        assert_eq!(offset_of!(RawAudioFrameV3, channel_stride_in_bytes), 40);
        assert_eq!(offset_of!(RawAudioFrameV3, p_metadata), 48);
        assert_eq!(offset_of!(RawAudioFrameV3, timestamp), 56);
        assert_eq!(size_of::<RawAudioFrameV3>(), 64);
    }

    #[test]
    fn fourcc_fltp() {
        assert_eq!(FOURCC_FLTP, 0x7054_4c46);
    }

    #[test]
    fn video_frame_v2_layout_matches_sdk_header_on_64_bit() {
        assert_eq!(offset_of!(RawVideoFrameV2, four_cc), 8);
        assert_eq!(offset_of!(RawVideoFrameV2, picture_aspect_ratio), 20);
        assert_eq!(offset_of!(RawVideoFrameV2, frame_format_type), 24);
        assert_eq!(offset_of!(RawVideoFrameV2, timecode), 32);
        assert_eq!(offset_of!(RawVideoFrameV2, p_data), 40);
        assert_eq!(offset_of!(RawVideoFrameV2, line_stride_in_bytes), 48);
        assert_eq!(offset_of!(RawVideoFrameV2, p_metadata), 56);
        assert_eq!(offset_of!(RawVideoFrameV2, timestamp), 64);
        assert_eq!(size_of::<RawVideoFrameV2>(), 72);
    }

    #[test]
    fn fourcc_uyvy() {
        assert_eq!(FOURCC_UYVY, 0x5956_5955);
    }
}
