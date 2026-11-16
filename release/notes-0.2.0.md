# Nuschl 0.2.0

- **Emoji:** a picker beside the message box, short codes such as `:smile:`, and
  the same emoji images on desktop and in the browser.
- **Link previews:** on Linux, the desktop app attaches a preview card (title,
  description and image) to a link you send, fetched directly from your
  computer. You can turn previews off in Settings. The desktop app and the
  browser show received cards without fetching anything.
- **Microphone:** only your own Unmute opens the microphone. Server changes,
  reconnects and restarts keep you muted, and mute and deafen carry over to the
  next connection.
- **Reconnects:** on Nuschl servers, chat history fills in after a reconnect,
  without duplicates. In the browser, an Unmute clicked right after joining a
  channel is no longer refused.
- **Voice:** gaps in incoming audio fade in and out without clicks, and mute
  and other controls take effect at once while voice is busy.
- **Sandboxing:** each helper program runs with its own fixed set of
  permissions.
- **Desktop:** Settings → Voice quality lists only the servers that hold your
  voice below the bitrate you chose. Servers you deafened yourself stay out of the
  voice card. A dialog over another keeps the keyboard focus, and dialogs stop
  scrolling at their end. Nuschl keeps no temporary files in its config folder.
- **Browser:** larger voice toggles for touch, a drawer that slides, and swipe
  gestures.
- **Music bot:** with an import service configured, people can search for albums
  missing from the library and add them from the bot's web page. Covers reach the
  browser as PNG images made by the bot's sandboxed content worker.
- **Server:** importing a Murmur database (`nuschld --import-murmur`) requires
  Linux 6.12 or later.

## Downloads

- **Windows x86_64:** [download the complete portable ZIP](https://github.com/nuschl-eu/nuschl/releases/download/v0.2.0/nuschl-0.2.0-windows-x86_64.zip), unpack it into a writable folder and start
  `nuschl.exe`. Keep the helper executables alongside it. Nuschl 0.1.0 on
  Windows can update itself.
- **macOS Intel and Apple silicon:** [download the complete universal app ZIP](https://github.com/nuschl-eu/nuschl/releases/download/v0.2.0/nuschl-0.2.0-macos-universal.zip), unpack it and move
  `Nuschl.app` to Applications. Requires macOS 14.2 or later. Native macOS runtime
  testing remains outstanding.
- **Linux:** install through the Nix flake at this release's tag. There is no
  portable Linux binary package yet.

Both ZIPs include all required helper programs and licenses. You do not need to
download the individual files listed under Assets; those support automatic updates.

Windows downloads do not yet have Authenticode signatures; the macOS app is
ad-hoc signed and has not been notarised. See the [installation instructions](https://github.com/nuschl-eu/nuschl/blob/v0.2.0/docs/install.md)
for platform requirements and first-start prompts.

## Limits

Ordinary Mumble voice and chat use transport encryption: the server can read
messages and hear voice. Experimental MLS work does not make ordinary voice
end-to-end encrypted. Video calls, screen sharing, local speech models, hosted
identity recovery, and mobile apps remain unfinished. Link previews are created
on Linux only.

Please report problems with the platform, release version and reproduction steps.
Avoid including private conversations, identity keys or server credentials.
