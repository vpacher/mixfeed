//! mixfeed-ctl: settings and status of a Mixfeed receiver, the same on every
//! platform. macOS carries them in custom audio device properties, Linux in
//! the JSON messages of the `mixfeedd` control socket.

use serde::{Deserialize, Serialize};

/// Reference level: how NDI float samples map to digital full scale.
///
/// The NDI convention: float 1.0 = +4 dBu = -20 dBFS (SMPTE) = -14 dBFS
/// (EBU R68), so digital full scale is 10.0. NDI Test Patterns follows it
/// (its 0 dBu tone arrives as 0.63). OBS with DistroAV sends full scale as
/// 1.0 instead. Senders do not say which they use, so the user chooses; the
/// default is the NDI convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// NDI float 1.0 = 0 dBFS, no change (x1); OBS with DistroAV sends so.
    Raw,
    /// NDI convention, 1.0 = +4 dBu = -20 dBFS (x0.1).
    #[default]
    Smpte,
    /// NDI convention with EBU R68 alignment, 1.0 = +4 dBu = -14 dBFS (x0.2).
    Ebu,
}

impl Level {
    /// In menu order: the default first.
    pub const ALL: [Level; 3] = [Level::Smpte, Level::Ebu, Level::Raw];

    pub fn name(self) -> &'static str {
        match self {
            Level::Raw => "raw",
            Level::Smpte => "smpte",
            Level::Ebu => "ebu",
        }
    }

    pub fn from_name(name: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.name() == name)
    }

    /// Gain applied to the received samples.
    pub fn gain(self) -> f32 {
        match self {
            Level::Raw => 1.0,
            // 10^(-20/20) and 10^(-14/20).
            Level::Smpte => 0.1,
            Level::Ebu => 0.1995,
        }
    }
}

/// What the receiver is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u32)]
pub enum State {
    #[default]
    Idle = 0,
    NoSource = 1,
    Searching = 2,
    Connected = 3,
    Lost = 4,
    Error = 5,
}

/// The lowest peak level in a status: silence.
pub const PEAK_FLOOR_DB: f64 = -100.0;

/// A snapshot of the receiver. Unknown numbers are `None`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub state: State,
    /// Status line, e.g. "connected STUDIO (OBS) 192.0.2.10:5961".
    pub text: String,
    /// Full name of the connected source; empty when not connected.
    pub source: String,
    /// A client takes audio (the monitor included).
    pub io: bool,
    /// An application other than the monitor takes audio and the microphone
    /// is not muted: the microphone is on air (tally).
    #[serde(default)]
    pub on_air: bool,
    /// The microphone sends silence (see [`Settings::muted`]).
    #[serde(default)]
    pub muted: bool,
    /// Output the monitor plays to ("default" for the default output); not
    /// set while the monitor is off.
    #[serde(default)]
    pub monitor: Option<String>,
    pub sender_ppm: Option<f64>,
    pub ratio_ppm: Option<f64>,
    pub fill_ms: Option<f64>,
    /// Smoothed fill and the fill the clock bridge holds.
    pub fill_avg_ms: Option<f64>,
    pub target_ms: Option<f64>,
    pub underruns: u64,
    pub resyncs: u64,
    pub overflows: u64,
    /// Peak of the audio given to the clients (after the reference level) in
    /// the last 1-2 s, in dBFS, at least [`PEAK_FLOOR_DB`]; `None` while no
    /// audio goes to a client.
    #[serde(default)]
    pub peak_db: Option<f64>,
    /// The virtual camera, where the receiver has one (Linux `mixfeedd`).
    #[serde(default)]
    pub camera: Option<CameraStatus>,
}

/// A snapshot of the virtual camera.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CameraStatus {
    /// Device path, e.g. "/dev/video10".
    pub device: String,
    /// An application streams from the camera (tally).
    pub live: bool,
    /// Status line of the video receiver.
    pub text: String,
    /// Frames written to the device since start (NDI and placeholder).
    pub frames: u64,
    /// Width, height and FourCC of the last NDI frame; 0 before the first.
    pub source_width: u32,
    pub source_height: u32,
    pub source_fourcc: String,
    /// Frame rate of the last NDI frame as numerator and denominator; 0 before
    /// the first.
    #[serde(default)]
    pub source_rate_n: u32,
    #[serde(default)]
    pub source_rate_d: u32,
    /// Why the camera is not available (device missing, setup failed); the
    /// other fields are empty then.
    #[serde(default)]
    pub error: Option<String>,
}

/// Settings that a client can change and the receiver keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Name part of the NDI source; empty = none.
    pub source: String,
    pub level: Level,
    /// Keep the microphone receiver connected without a client.
    #[serde(default)]
    pub keep_warm: bool,
    /// The microphone sends silence and stays connected; it is not on air.
    #[serde(default)]
    pub muted: bool,
}

/// Messages of a control socket (Linux `mixfeedd`): one JSON request per line,
/// one JSON response per line.
pub mod socket {
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    use super::{Level, Settings, Status};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "cmd", rename_all = "kebab-case")]
    pub enum Request {
        Status,
        /// Source names; keeps the receiver browsing for a while.
        Sources,
        Get,
        /// Change the given settings; returns the new settings.
        Set {
            source: Option<String>,
            level: Option<Level>,
            #[serde(default)]
            keep_warm: Option<bool>,
            #[serde(default)]
            muted: Option<bool>,
        },
        /// Play the microphone to an output (`target`: output name, `None`
        /// for the default), or stop that. Returns the status.
        Monitor {
            on: bool,
            #[serde(default)]
            target: Option<String>,
        },
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Response {
        Status(Box<Status>),
        Sources(Vec<String>),
        Settings(Settings),
        Error(String),
    }

    /// `$XDG_RUNTIME_DIR/mixfeed/control.sock`.
    pub fn path() -> Option<PathBuf> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        Some(PathBuf::from(dir).join("mixfeed").join("control.sock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_round_trip() {
        for l in Level::ALL {
            assert_eq!(Level::from_name(l.name()), Some(l));
        }
        assert_eq!(Level::from_name("loud"), None);
    }

    #[test]
    fn requests_are_tagged_json() {
        use socket::Request;
        let r: Request = serde_json::from_str(r#"{"cmd":"set","source":"STUDIO","level":"smpte"}"#).unwrap();
        assert_eq!(
            r,
            Request::Set {
                source: Some("STUDIO".into()),
                level: Some(Level::Smpte),
                keep_warm: None,
                muted: None,
            }
        );
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
    }
}
