//! Headphone monitor: PipeWire's `pw-loopback` from the "Mixfeed Mic" node
//! to an output. PipeWire matches clocks and rates between the two. The
//! monitor's capture node is named `mixfeed_monitor_in`, so the on-air
//! detection in `source` does not count it. The monitor is not saved; it is
//! off after a restart.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

/// Prefix of the monitor's node names (see `source::is_monitor_node`).
pub const NODE_PREFIX: &str = "mixfeed_monitor";

struct Running {
    child: Child,
    target: String,
}

static MONITOR: Mutex<Option<Running>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Start the monitor to `target` (an output node name; `None` = default
/// output), replacing a running one. Stop it with `on = false`.
pub fn set(on: bool, target: Option<String>) -> Result<(), String> {
    let mut g = lock(&MONITOR);
    if let Some(mut r) = g.take() {
        let _ = r.child.kill();
        let _ = r.child.wait();
        eprintln!("monitor off");
    }
    if !on {
        return Ok(());
    }
    let mut cmd = Command::new("pw-loopback");
    cmd.args(["-n", NODE_PREFIX, "-C", "mixfeed_mic", "-l", "20"])
        .args(["-i", &format!("node.name={NODE_PREFIX}_in node.description=\"Mixfeed monitor\" node.dont-reconnect=true")])
        .args(["-o", &format!("node.name={NODE_PREFIX}_out node.description=\"Mixfeed monitor\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if let Some(t) = &target {
        cmd.args(["-P", t]);
    }
    let child = cmd.spawn().map_err(|e| format!("pw-loopback: {e}"))?;
    let target = target.unwrap_or_else(|| "default".into());
    eprintln!("monitor on to {target}");
    *g = Some(Running { child, target });
    Ok(())
}

/// The output the monitor plays to; `None` while off (or when pw-loopback ended).
pub fn target() -> Option<String> {
    let mut g = lock(&MONITOR);
    let ended = g.as_mut().is_some_and(|r| !matches!(r.child.try_wait(), Ok(None)));
    if ended {
        eprintln!("monitor: pw-loopback ended");
        *g = None;
    }
    g.as_ref().map(|r| r.target.clone())
}

/// Stop the monitor (at daemon exit).
pub fn stop() {
    let _ = set(false, None);
}
