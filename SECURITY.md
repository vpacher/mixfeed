# Security policy

## Report a vulnerability

Do not report a vulnerability in a public issue, pull request or discussion.

Report it privately through GitHub: [Report a vulnerability](https://github.com/vpacher/mixfeed/security/advisories/new) (tab **Security**, **Advisories**).

Include:

- the Mixfeed version or commit,
- the platform (distribution, kernel, PipeWire version, NDI runtime version: `mfctl ndi`),
- the steps to reproduce, or a proof of concept,
- the effect (for example: crash, memory read, code execution, data sent to another host),
- whether the problem is known in public.

## What happens next

Mixfeed has one maintainer. The answers are best effort:

1. Confirmation that the report arrived: in about 7 days.
2. First assessment (accepted or not, severity): in about 14 days.
3. A fix and a published advisory: as soon as possible. We agree on the date of the disclosure with you.

You get credit in the advisory, if you want it.

## Supported versions

| Version | Supported |
|---|---|
| Latest release | yes |
| `main` | yes |
| Older releases | no; update to the latest release |

## Scope

In scope: the code in this repository.

- `mixfeedd` and its input from the network: source names, audio and video frames from NDI senders.
- The control socket (`$XDG_RUNTIME_DIR/mixfeed/control.sock`) and `mfctl`.
- The settings file (`~/.config/mixfeed/settings.json`).
- `linux/install.sh`.
- The Omarchy bar widget.

Out of scope:

- **The NDI runtime and the NDI protocol.** Mixfeed loads the NDI runtime that you install, but does not include it. Report problems in the runtime or the protocol to Vizrt NDI AB ([ndi.video](https://ndi.video/)).
- **Unauthenticated NDI senders.** NDI does not authenticate senders. Any device on the network can announce a source, and a sender sets the frame size. Use Mixfeed on a network that you trust. A report that shows Mixfeed handles such input unsafely (for example a crash or an out-of-bounds read) is in scope.
- **Other software:** PipeWire, WirePlumber, v4l2loopback, FFmpeg, Omarchy, Quickshell. Report to their projects.
- Problems that need an attacker who already runs code as your user.
