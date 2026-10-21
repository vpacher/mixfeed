#!/bin/sh
# Build and install mixfeedd and mfctl (~/.local/bin), the systemd user unit and,
# on Omarchy, the bar widget (~/.config/omarchy/plugins/vpacher.mixfeed).
# With "remove": stop and uninstall (settings in ~/.config/mixfeed stay).
#
# Checks first: a missing requirement stops the install with a hint, a missing
# optional part (camera, monitor) gives a warning.
set -eu
cd "$(dirname "$0")/.."
unit="$HOME/.config/systemd/user/mixfeedd.service"
plugin_id=vpacher.mixfeed
plugin="$HOME/.config/omarchy/plugins/$plugin_id"
camera_dev=/dev/video10
# Points mixfeedd at FFmpeg libraries outside the library path (NDI HX decoding).
ffmpeg_dropin="$unit.d/ndi-ffmpeg.conf"
# Chrome gives a camera the group of the one microphone whose name contains
# the camera's name (GuessVideoGroupID): "Mixfeed" in "Mixfeed Mic" makes
# them one device, a webcam with a built-in microphone.
camera_name="Mixfeed"

fail() {
    echo "error: $*" >&2
    exit 1
}
warn() {
    echo "warning: $*" >&2
}
have() {
    command -v "$1" >/dev/null 2>&1
}
# True when root owns the path and no other user can change it: only such a
# folder goes into mixfeedd's library path.
root_only() {
    [ "$(stat -c %u "$1")" = 0 ] && [ -z "$(find "$1" -maxdepth 0 -perm /022)" ]
}
# The omarchy CLI talks to the running shell over IPC.
have_omarchy_shell() {
    [ -d "$HOME/.config/omarchy" ] && have omarchy && pgrep -x quickshell >/dev/null 2>&1
}

if [ "${1:-}" = remove ]; then
    systemctl --user disable --now mixfeedd.service 2>/dev/null || true
    rm -f "$unit" "$ffmpeg_dropin" "$HOME/.local/bin/mixfeedd" "$HOME/.local/bin/mfctl"
    rmdir "$unit.d" 2>/dev/null || true
    if have_omarchy_shell; then
        omarchy plugin disable "$plugin_id" >/dev/null 2>&1 || true
    fi
    rm -rf "$plugin"
    systemctl --user daemon-reload
    echo "removed"
    exit 0
fi

# ---------- Requirements ----------
systemctl --user show-environment >/dev/null 2>&1 ||
    fail "no systemd user session (mixfeedd runs as a systemd user service)"
have cargo || fail "cargo not found: install Rust (Arch: pacman -S rustup && rustup default stable)"
{ have pkg-config && pkg-config --exists libpipewire-0.3; } ||
    fail "PipeWire development files not found (Arch: pacman -S pipewire pkgconf)"
# The PipeWire bindings are generated with bindgen, which needs libclang.
ldconfig -p 2>/dev/null | grep -q 'libclang\.so' ||
    fail "libclang not found, needed to build the PipeWire bindings (Arch: pacman -S clang)"

# ---------- Optional parts ----------
systemctl --user is-active --quiet pipewire.service ||
    warn "PipeWire is not running: the microphone needs it"
have pw-loopback || warn "pw-loopback not found: the headphone monitor is not available"
if [ ! -e "$camera_dev" ]; then
    if modinfo v4l2loopback >/dev/null 2>&1; then
        warn "$camera_dev not found: the camera is off. Load v4l2loopback with:"
    else
        warn "v4l2loopback is not installed: the camera is off. Install v4l2loopback-dkms (and the kernel headers), then:"
    fi
    cat >&2 <<EOF
  echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
  echo 'options v4l2loopback video_nr=10 card_label="$camera_name" exclusive_caps=1 max_buffers=2' | sudo tee /etc/modprobe.d/v4l2loopback.conf
  sudo modprobe v4l2loopback
EOF
elif [ "$(cat /sys/class/video4linux/video10/name 2>/dev/null)" != "$camera_name" ]; then
    warn "$camera_dev is not \"$camera_name\" (another v4l2loopback setup?): apps show its other name"
fi
if systemctl is-active --quiet ufw.service 2>/dev/null; then
    # NDI receivers listen on 5960 (reliable UDP) and on 6960 and up (TCP and
    # UDP, one port per connection; senders such as BirdDog converters connect
    # back to these). Without the second range the camera stays "no video".
    echo "note: ufw is active. Microphone audio works without a rule; camera video needs inbound NDI ports from the sender, e.g.:" >&2
    echo "  sudo ufw allow proto udp from <sender network> to any port 5960:5999 comment 'NDI receive (RUDP)'" >&2
    echo "  sudo ufw allow proto tcp from <sender network> to any port 6960:7959 comment 'NDI receive (TCP)'" >&2
    echo "  sudo ufw allow proto udp from <sender network> to any port 6960:7959 comment 'NDI receive (UDP)'" >&2
fi

# ---------- Build ----------
cargo build --release -p mixfeedd
# The NDI runtime is loaded at run time: check it with the same search as mixfeedd.
ndi=$(target/release/mfctl ndi) || exit 1
echo "NDI runtime: $ndi"

# NDI HX sources (H.264/HEVC: phone apps, HX cameras) are decoded by the NDI
# runtime with FFmpeg, which it loads by soname and does not include (6.3.2:
# libavcodec.so.61 + libavutil.so.59 = FFmpeg 7). Without them the camera shows
# "Video decoder not found" and HX audio is silent; full-bandwidth NDI works.
# The runtime names the sonames it loads, so read them from the library.
ndi_lib=${ndi##*(}
ndi_lib=${ndi_lib%)}
ffmpeg_libs=$(grep -aoE 'libav(codec|util)\.so\.[0-9]+' "$ndi_lib" 2>/dev/null | sort -u || true)
ffmpeg_dir=
ffmpeg_missing=
for so in $ffmpeg_libs; do
    ldconfig -p 2>/dev/null | grep -qE "^[[:space:]]+$so " || ffmpeg_missing="$ffmpeg_missing $so"
done
if [ -n "$ffmpeg_missing" ]; then
    # Older FFmpeg versions are packaged next to the current one, in their
    # own folder (Arch AUR ffmpeg7.1: /usr/lib/ffmpeg7.1).
    for d in /usr/lib/ffmpeg* /usr/local/lib/ffmpeg* /opt/ffmpeg*/lib; do
        [ -d "$d" ] || continue
        found=yes
        for so in $ffmpeg_missing; do
            [ -e "$d/$so" ] || found=
        done
        if [ -n "$found" ] && ! root_only "$d"; then
            warn "$d has FFmpeg libraries, but root does not own it alone: not used"
            found=
        fi
        if [ -n "$found" ]; then
            ffmpeg_dir=$d
            break
        fi
    done
fi
if [ -n "$ffmpeg_dir" ]; then
    echo "NDI HX: FFmpeg libraries from $ffmpeg_dir"
elif [ -n "$ffmpeg_missing" ]; then
    hint="install FFmpeg libraries that have them next to the current FFmpeg, then run this script again"
    case "$ffmpeg_missing" in
    *libavcodec.so.61*) hint="install FFmpeg 7 next to the current FFmpeg (Arch: AUR package ffmpeg7.1), then run this script again" ;;
    esac
    warn "NDI HX sources (phone apps, HX cameras) do not decode: the NDI runtime needs${ffmpeg_missing}. To fix, $hint"
fi

# ---------- Install ----------
install -Dm755 -t "$HOME/.local/bin" target/release/mixfeedd target/release/mfctl
install -Dm644 linux/mixfeedd/mixfeedd.service "$unit"
if [ -n "$ffmpeg_dir" ]; then
    mkdir -p "$unit.d"
    printf '[Service]\nEnvironment=LD_LIBRARY_PATH=%s\n' "$ffmpeg_dir" >"$ffmpeg_dropin"
else
    rm -f "$ffmpeg_dropin"
    rmdir "$unit.d" 2>/dev/null || true
fi
systemctl --user daemon-reload
systemctl --user enable mixfeedd.service
systemctl --user restart mixfeedd.service
systemctl --user is-active --quiet mixfeedd.service ||
    fail "mixfeedd did not start, see: journalctl --user -u mixfeedd"
"$HOME/.local/bin/mfctl" status >/dev/null || fail "mfctl cannot reach mixfeedd"
echo "installed; mixfeedd $(systemctl --user is-active mixfeedd.service)"

# WirePlumber makes camera nodes when it starts. On a first install it runs
# already, and it saw the loopback device before mixfeedd wrote a frame (no
# capture format then), so there is no PipeWire camera: apps that find cameras
# through PipeWire do not list it (direct V4L2 apps such as Chrome do). Restart
# WirePlumber once; mixfeedd is ready (Type=notify after its first frame).
camera_node() {
    pw-cli ls Node 2>/dev/null | grep -q "object.path = \"v4l2:$camera_dev\""
}
if [ -e "$camera_dev" ] && have pw-cli && systemctl --user is-active --quiet wireplumber.service && ! camera_node; then
    echo "restarting WirePlumber so that PipeWire lists the camera (audio pauses for a moment)"
    systemctl --user restart wireplumber.service
    i=0
    while ! camera_node && [ $i -lt 10 ]; do
        sleep 1
        i=$((i + 1))
    done
    camera_node || warn "PipeWire lists no camera for $camera_dev; log out and in again"
fi

# ---------- Omarchy bar widget ----------
[ -d "$HOME/.config/omarchy" ] || exit 0
src=linux/omarchy-plugin/$plugin_id
if diff -rq "$src" "$plugin" >/dev/null 2>&1; then
    echo "bar widget unchanged"
    exit 0
fi
# Copy into a hidden folder and move it in at once: the shell reloads a plugin
# on every file change it sees.
stage="$HOME/.config/omarchy/plugins/.$plugin_id.new"
rm -rf "$stage"
install -Dm644 -t "$stage" "$src"/*
rm -rf "$plugin"
mv "$stage" "$plugin"
if ! have_omarchy_shell; then
    echo "bar widget installed; enable it with: omarchy plugin enable $plugin_id"
    exit 0
fi
omarchy-shell shell rescanPlugins >/dev/null 2>&1 || true
if ! grep -q "\"$plugin_id\"" "$HOME/.config/omarchy/shell.json" 2>/dev/null; then
    omarchy plugin enable "$plugin_id" --section right --after omarchy.microphone >/dev/null 2>&1 ||
        omarchy plugin enable "$plugin_id" --section right >/dev/null ||
        warn "could not add the widget to the bar: omarchy plugin enable $plugin_id"
fi
# A hot reload does not always take a changed widget (old QML, or a widget that
# does not open). A restart does; only from a terminal, where the screen is not locked.
if [ -t 0 ] && [ -t 1 ] && have omarchy-restart-shell; then
    omarchy-restart-shell >/dev/null 2>&1 &
    echo "bar widget updated; shell restarted"
else
    echo "bar widget updated; if it looks old or does not open, run: omarchy-restart-shell"
fi
