# Nuschl 0.3.0

- **Video calls and screen sharing (early):** **Start call** and **Share
  screen** in a channel open a call page in your browser, on stock Mumble
  servers too. Nuschl users in the channel get a call card with **Join**; stock
  Mumble users get the link as a message. The page at
  `https://stream.nuschl.eu/call` shows cameras in a grid, a shared screen in a
  large view, and everyone in a roster. Video runs through Nuschl's companion
  SFU in AV1 or VP9, one codec per call. With **Relay-only connections** on,
  the page connects only through the call host's TURN server.
- **Calls in direct conversations:** the same buttons in a direct conversation
  start a call with that person alone.
- **Web app:** connects your saved servers when it opens and returns to the
  channel you were last in, muted and deafened. A double-click on a channel
  joins voice, with a mouse or a pen. Collapsed channels show how many people
  are inside. Ctrl+K opens the quick switcher, Ctrl+F searches the
  conversation. In voice, the page keeps the screen on and shows media
  controls. When the browser stops the microphone, Nuschl mutes and shows
  "Microphone stopped".
- **Audio:** Opus comes from opus-rs, a Rust port of libopus. Mute and unmute
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
- **Desktop:** Right and Left open, close and move through the channel tree.
  Dialogs keep their title and close button in view.
- **Windows:** a per-user installer that runs with your own rights and adds a
  Start menu entry and an uninstall entry.
- **Call host:** the NixOS module `services.nuschl-stream` serves the call page
  and runs the SFU, which hands out credentials for your TURN server.

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

- **Calls:** end-to-end encryption for calls is unfinished. The Mumble server
  sees the call link, and anyone with the link can join.
- **Desktop video:** the desktop app opens calls in the browser; native video
  comes later.
- **Phones:** in a phone browser, voice ends when the screen turns off.
- **Call quality:** the call page's quality menu offers Auto only.

Ordinary Mumble voice and chat use transport encryption: the server can read
messages and hear voice. Local speech models, hosted identity recovery and mobile
apps remain unfinished.

Please report problems with the platform, release version and reproduction steps.
Leave private conversations, identity keys and server credentials out of reports.
