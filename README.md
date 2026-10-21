# Mixfeed

Mixfeed receives an [NDI®](https://ndi.video/) source on Linux and gives it to your applications as a microphone and a webcam. Use it to bring the program output of a video mixer (for example OBS with DistroAV, vMix, a hardware mixer or an NDI converter) into a video call, a browser or a recording application.

Mixfeed runs as a small user service. A bar widget for Omarchy controls it.

NDI® is a registered trademark of Vizrt NDI AB. Mixfeed is not a product of NDI and is not affiliated with NDI. Mixfeed uses the NDI runtime that you install. It does not include NDI libraries. See section 9 for the NDI terms.

## 1. What Mixfeed does

- **Microphone "Mixfeed Mic".** A PipeWire audio source, 48 kHz, 2 channels. The audio of the NDI source comes in with a stable clock: Mixfeed matches the clock of the sender to the clock of your computer with adaptive resampling. The result has no drift, no clicks and no dropouts.
- **Camera "Mixfeed".** A V4L2 camera (v4l2loopback), 1920x1080, YUYV. Mixfeed receives NDI video only while an application uses the camera, because full-bandwidth video costs network and CPU.
- **One webcam in browsers.** Chrome shows the camera and the microphone as one device (a webcam with a built-in microphone). Firefox uses the same rule (not tested).
- **Automatic connection.** Select a source once. Mixfeed connects when the source appears and connects again after the source was lost.
- **On air (tally).** The bar widget lights up while an application uses the microphone or the camera.
- **Mute.** The microphone sends silence and stays connected. Applications keep the microphone. Unmute is instant.
- **Headphone monitor.** Plays the microphone to your headphones. The monitor does not count as on air.
- **Keep connected.** Stays connected to the source without an application, so that the microphone is ready at once.
- **Reference level.** Matches the audio level convention of the sender (see section 5).

## 2. Limits

- **One source.** The microphone and the camera use the same NDI source.
- **One camera application at a time.** v4l2loopback lets only one application stream from the camera. A second application gets an error until the first one stops.
- **Camera size.** The camera always sends 1920x1080. Mixfeed does not scale 1920x1080 sources. It scales other sizes with a simple (nearest-neighbour) scaler. For the best picture, send 1920x1080 from the sender.
- **CPU.** The NDI runtime decodes the video. A 1920x1080 source costs about one CPU core (measured on a 2017 laptop) while an application uses the camera. A 3840x2160 source costs about two cores.
- **NDI HX needs FFmpeg 7.** NDI HX sources (H.264 and HEVC: phone apps, HX cameras) are decoded by the NDI runtime with FFmpeg 7 (`libavcodec.so.61`). Arch Linux has a newer FFmpeg. Without FFmpeg 7, the camera shows "Video decoder not found" and the microphone is silent. Full-bandwidth NDI sources need nothing extra. See section 7.
- **Firewall.** NDI video needs inbound ports (see section 3, step 5).
- **Receive only.** Mixfeed does not send NDI, does not record and does not control the sender.
- **Tested platform.** x86_64 only. ARM (aarch64) builds, but is not tested on hardware.

Tested with: Omarchy 4.0 (Arch Linux, x86_64), PipeWire 1.6, NDI runtime 6.3, Chrome 153. Senders: OBS with DistroAV, NDI Test Patterns, a BirdDog converter, NDI HX Camera (with FFmpeg 7).

## 3. Install

### Requirements

| Part | Package (Arch Linux) | Needed for |
|---|---|---|
| Rust (edition 2024) | `rustup` | build |
| clang | `clang` | build (PipeWire bindings) |
| PipeWire development files | `pipewire`, `pkgconf` | build, microphone |
| NDI runtime 6 ([ndi.video](https://ndi.video/)) | AUR `ndi-sdk` | microphone, camera |
| v4l2loopback | `v4l2loopback-dkms` and the headers of your kernel | camera (optional) |
| FFmpeg 7 | AUR `ffmpeg7.1` | NDI HX sources (optional) |

### Procedure

1. Install the build requirements and the NDI runtime:

   ```sh
   sudo pacman -S --needed rustup clang pkgconf pipewire
   rustup default stable
   yay -S ndi-sdk
   ```

2. Optional, for the camera: install v4l2loopback and the headers of your kernel. On Omarchy the kernel is `linux-omarchy`:

   ```sh
   sudo pacman -S --needed v4l2loopback-dkms linux-omarchy-headers
   ```

3. Optional, for the camera: load v4l2loopback at boot, with the name "Mixfeed" on `/dev/video10`:

   ```sh
   echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
   echo 'options v4l2loopback video_nr=10 card_label="Mixfeed" exclusive_caps=1 max_buffers=2' | sudo tee /etc/modprobe.d/v4l2loopback.conf
   sudo modprobe v4l2loopback
   ```

4. Get Mixfeed and install it:

   ```sh
   git clone https://github.com/vpacher/mixfeed.git
   cd mixfeed
   linux/install.sh
   ```

   The script:
   - checks the requirements and stops with a hint if one is missing,
   - builds `mixfeedd` and `mfctl` and installs them in `~/.local/bin`,
   - installs and starts the systemd user service `mixfeedd`,
   - on Omarchy, installs the bar widget from [omarchy-mixfeed](https://github.com/vpacher/omarchy-mixfeed) (a git checkout, so `omarchy plugin update` updates it; without network, the copy in this repository) and adds it to the right side of the bar.

   It warns about optional parts that are missing (camera, monitor, FFmpeg 7). It does not use `sudo`.

5. If ufw is active: allow NDI video from your network. Replace `192.168.0.0/24` with your network:

   ```sh
   sudo ufw allow proto udp from 192.168.0.0/24 to any port 5960:5999 comment 'NDI receive (RUDP)'
   sudo ufw allow proto tcp from 192.168.0.0/24 to any port 6960:7959 comment 'NDI receive (TCP)'
   sudo ufw allow proto udp from 192.168.0.0/24 to any port 6960:7959 comment 'NDI receive (UDP)'
   ```

   Audio and source discovery work without these rules. Without them, the camera connects but shows no picture. Some senders (for example BirdDog converters) connect back to the receiver on ports 6960 and up.

6. Restart the browser if it was open during the install. Browsers read the camera list when they start.

### Update

```sh
cd mixfeed
git pull
linux/install.sh
```

Settings stay.

### Remove

```sh
linux/install.sh remove
```

The script stops and removes the service, the programs and the bar widget. Your settings stay in `~/.config/mixfeed`. To remove the camera module configuration, delete `/etc/modules-load.d/v4l2loopback.conf` and `/etc/modprobe.d/v4l2loopback.conf`.

## 4. Use

### Bar widget

Click the Mixfeed icon in the bar to open the panel.

| Section | Content |
|---|---|
| Mixfeed Mic | state of the microphone and the camera, buffer, sender clock, underruns |
| NDI® source | sources on the network; select one to connect |
| Reference level | NDI SMPTE, NDI EBU, Raw 0 dB (see section 5) |
| Microphone | Mute, Keep connected, Monitor |

Keys in the panel: `j`/`k` or the arrow keys move, `Enter` or `Space` selects, `r` refreshes, `Esc` closes.

### Command line

| Command | Action |
|---|---|
| `mfctl status` | show the state in one line |
| `mfctl info` | show the state with all values |
| `mfctl sources` | list the NDI sources on the network |
| `mfctl source <name part>` | select a source (the first source whose name contains the text) |
| `mfctl level raw\|smpte\|ebu` | set the reference level |
| `mfctl mute on\|off` | mute or unmute the microphone |
| `mfctl warm on\|off` | keep connected on or off |
| `mfctl monitor on [<output>]\|off` | headphone monitor on (default output or a named PipeWire node) or off |
| `mfctl ndi` | show the NDI runtime that Mixfeed loads |

### In applications

- Microphone: select **Mixfeed Mic**.
- Camera: select **Mixfeed**.

## 5. Reference level

NDI senders do not tell the receiver how their audio level maps to digital full scale. Select the option that matches your sender:

| Option | Gain | Use for |
|---|---|---|
| NDI SMPTE (default) | ×0.1 (-20 dB) | most NDI senders (NDI Tools, NDI devices): +4 dBu = -20 dBFS |
| NDI EBU | ×0.2 (-14 dB) | senders aligned to EBU R68, for example BirdDog converters: +4 dBu = -14 dBFS |
| Raw 0 dB | ×1 | senders that send full scale as 1.0, for example OBS with DistroAV |

To check the level, look at the peak value on the Microphone line of the widget. A level that is 14 to 20 dB too high or too low shows a wrong option.

## 6. Network and privacy

- Mixfeed finds sources with NDI discovery (mDNS) and receives from the selected source only.
- Mixfeed sends no data to other places. It has no telemetry.
- The sender sets the frame size. A sender that sends very large frames makes the NDI runtime use much memory and CPU. Mixfeed drops frames with a wrong format or line length.
- The control socket is `$XDG_RUNTIME_DIR/mixfeed/control.sock`. Only your user can open it.
- Settings: `~/.config/mixfeed/settings.json`.

## 7. Troubleshooting

| Symptom | Cause | Action |
|---|---|---|
| Widget shows "mixfeedd not running" | service stopped or failed | `systemctl --user status mixfeedd`, then `journalctl --user -u mixfeedd` |
| No sources in the list | sender on another network, or mDNS blocked | put the sender and the computer on the same network; allow mDNS (UDP 5353) |
| Camera not in the browser | v4l2loopback not loaded, or the browser started before the camera | `ls /dev/video10`; do step 3 of the install; restart the browser |
| Camera "Live", but no picture | firewall blocks NDI video | do step 5 of the install |
| Camera shows "Video decoder not found", microphone silent | NDI HX source without FFmpeg 7 | `yay -S ffmpeg7.1`, then `linux/install.sh` again (it finds FFmpeg 7 and sets the library path for `mixfeedd`) |
| Second application cannot open the camera | v4l2loopback allows one stream | close the camera in the first application |
| Audio too loud or too quiet by 14 to 20 dB | wrong reference level | see section 5 |
| `mfctl ndi` fails | NDI runtime not installed | `yay -S ndi-sdk` |

When you report a problem, add the output of `mfctl info`, `mfctl ndi` and `journalctl --user -u mixfeedd -b`.

## 8. Build without the install script

```sh
cargo build --release -p mixfeedd
cargo test
```

Mixfeed looks for the NDI runtime in this order: `$MIXFEED_LIBNDI`, `/usr/local/lib/libndi.so.6`, `/usr/lib/libndi.so.6`.

## 9. NDI terms

Mixfeed loads the NDI runtime that you install (for example from the NDI SDK or NDI Tools, [ndi.video](https://ndi.video/)). The runtime is not part of Mixfeed. Its use is subject to the [NDI SDK License Agreement](https://ndi.link/ndisdk_license) of Vizrt NDI AB:

- Do not modify, reverse engineer, decompile or disassemble the NDI runtime, the NDI SDK or the NDI protocols.
- Do not remove or change the notices in the NDI runtime or the NDI SDK.
- NDI and its licensors give no warranty for the NDI runtime and the NDI SDK, and are not liable for damages from the use of Mixfeed with them, to the extent that the law permits.

The NDI SDK and the NDI runtime are copyright Vizrt NDI AB. `crates/mixfeed-ndi` uses declarations from the NDI SDK header `Processing.NDI.Lib.h`, which is available under the MIT licence (see `crates/mixfeed-ndi/NOTICE`).

## 10. Licence

MIT or Apache-2.0, at your choice. See `LICENSE-MIT` and `LICENSE-APACHE`.
