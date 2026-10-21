//! mixfeedd: Mixfeed receiver daemon for Linux (systemd user service).
//!
//! Offers a PipeWire source "Mixfeed Mic" fed by the shared receive engine
//! (`mixfeed-audio`): the NDI receive runs while a client records, plus the
//! engine's linger. With v4l2loopback loaded, also the camera "Mixfeed
//! Camera" (`camera`), with NDI video only while an application streams. Clients change the source and the reference level through
//! the control socket ([`mixfeed_ctl::socket`]); `mfctl` is the command-line
//! client. Settings are kept in `$XDG_CONFIG_HOME/mixfeed/settings.json`.

#[cfg(target_os = "linux")]
mod camera;
#[cfg(target_os = "linux")]
mod control;
#[cfg(target_os = "linux")]
mod monitor;
#[cfg(target_os = "linux")]
mod source;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use mixfeed_audio::{Change, Hooks};

    mixfeed_audio::init(Hooks {
        log: |m| eprintln!("{m}"),
        changed: |_: Change| {},
    });
    let settings = control::load_settings();
    mixfeed_audio::set_source(&settings.source);
    mixfeed_audio::set_level(settings.level);
    mixfeed_audio::set_keep_warm(settings.keep_warm);
    mixfeed_audio::set_muted(settings.muted);
    if camera::start() {
        camera::set_source(&settings.source);
    }
    control::spawn()?;
    // The camera has its first frame: WirePlumber (ordered after this unit)
    // now sees a capture device and makes a camera node for it.
    notify_ready();
    // Connect to PipeWire again when the connection breaks (PipeWire restart),
    // in this process: the camera keeps its writer, so WirePlumber, which
    // restarts with PipeWire, still sees a capture device.
    loop {
        let why = match source::run() {
            Err(e) => e.to_string(),
            Ok(()) => "PipeWire main loop ended".to_owned(),
        };
        eprintln!("{why}; connecting again in 2 s");
        monitor::stop();
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Tell systemd (Type=notify) that the daemon is ready.
#[cfg(target_os = "linux")]
fn notify_ready() {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let sock = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(s) => s,
        Err(e) => return eprintln!("notify: {e}"),
    };
    let sent = if let Some(name) = path.to_str().and_then(|p| p.strip_prefix('@')) {
        // Abstract socket namespace.
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name).and_then(|a| sock.send_to_addr(b"READY=1", &a))
    } else {
        sock.send_to(b"READY=1", &path)
    };
    if let Err(e) = sent {
        eprintln!("notify: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("mixfeedd runs on Linux only");
    std::process::exit(1);
}
