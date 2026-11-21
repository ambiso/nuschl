# Nuschl 0.3.0

- **Video calls and screen sharing** are available in early form in the
  Linux, Windows, macOS, and web apps. Start or join a call from a channel or
  direct conversation, including on stock Mumble servers. Nuschl shows a
  call card with **Join**. Stock Mumble users receive a link to join in
  their browser. Share your camera and screen at the same time and minimize
  the call while you use chat. The desktop has a local preview and fullscreen
  gallery with the option to pin a feed.
- The desktop **quality menu** offers Auto and manual choices. **Video
  stats** shows frame rate, resolution, bitrate, and what limits the
  stream. Screen sharing adjusts bitrate independently of resolution
  and frame rate. Supported Linux hardware uses quality-based AV1
  encoding. Quiet screens keep their upload estimate when a bandwidth
  probe is limited by the sender.
- **Call media** is encrypted. Two native participants
  can switch to a direct connection when the path can carry the call. The call
  host's forwarding server handles group calls and provides a fallback.
  **Relay-only connections** uses the configured TURN relay.
- The **web app** connects saved servers on startup and reopens the last channel
  muted and deafened. Double-click a channel to join voice with a mouse or
  pen. Collapsed channels show how many people are inside. Ctrl+K opens the
  quick switcher and Ctrl+F searches the conversation. Voice playback adapts
  to uneven packet arrival, and the page asks the browser to keep the screen on.
  When the browser stops the microphone, Nuschl mutes and shows
  **Microphone stopped**.
- **Desktop voice playback** can use Deep PLC to fill missing speech packets.
  Nuschl uses opus-rs, a Rust port of libopus. Mute and unmute
  keep incoming voice playing. The echo canceller finds the real echo delay
  and models stereo music. Voice keeps running when PipeWire moves it to a new
  default device, such as Bluetooth headphones.
- **Chat history:** the storage helper gets up to a minute to open its database
  on a slow disk and keeps running after a refused request.
- **Channel permissions:** permissions as a table with explanations, **Check
  access** to see who may do what in a channel, groups picked from a list,
  channel passwords as in Mumble, and temporary sub-channels by choice.
  Channels are sorted as Mumble sorts them.
- **Link previews:** the desktop app creates previews on Windows and macOS too,
  and for long pages such as YouTube.
- New identities have a **recovery key** you can show or copy again from
  Settings.
- **Desktop:** Right and Left open, close and move through the channel tree.
  Dialogs keep their title and close button in view. Tiled Wayland windows
  keep their edges aligned when focus changes.
- **Windows:** a per-user installer that runs with your own rights and adds a
  Start menu entry and an uninstall entry.
- **Call host:** the NixOS module `services.nuschl-stream` serves the call page
  and runs the SFU, which hands out credentials for your TURN server. Operators
  can set egress and traffic budgets for relayed calls.

## Downloads

- **Windows x86_64:** [download the installer](https://github.com/nuschl-eu/nuschl/releases/download/v0.3.0/nuschl-0.3.0-windows-x86_64-setup.exe)
  or [the complete portable ZIP](https://github.com/nuschl-eu/nuschl/releases/download/v0.3.0/nuschl-0.3.0-windows-x86_64.zip).
  Unpack the ZIP into a writable folder and start `nuschl.exe`; keep the helper
  executables alongside it. Nuschl 0.2.0 on Windows can update itself.
- **macOS Intel and Apple silicon:** [download the complete universal app ZIP](https://github.com/nuschl-eu/nuschl/releases/download/v0.3.0/nuschl-0.3.0-macos-universal.zip), unpack it and move
  `Nuschl.app` to Applications. Requires macOS 14.2 or later. Native macOS runtime
  testing remains outstanding.
- **Linux:** install through the Nix flake at this release's tag.

The installer and both ZIPs include all required helper programs and licenses.
The individual files listed under Assets support automatic updates.

Authenticode signatures for Windows and notarisation for macOS come later; the
macOS app is ad-hoc signed. See the [installation instructions](https://github.com/nuschl-eu/nuschl/blob/v0.3.0/docs/install.md)
for platform requirements and first-start prompts.

## Limits

- Anyone with the **call link** can join. The Mumble server can see that link.
  Voice chat uses the Mumble voice connection.
- **Windows and macOS video** use software AV1 capture up to
  720p at 15 fps. Screen sharing was tested in a Windows 11 VM. Native
  macOS camera and screen-sharing permission testing remains outstanding.
- Voice in a **phone browser** may pause when the screen turns off.

Ordinary Mumble voice and chat use transport encryption: the server can read
messages and hear voice. Local speech models, hosted identity recovery and mobile
apps remain unfinished.

Please report problems with the platform, release version and reproduction steps.
Leave private conversations, identity keys and server credentials out of reports.
