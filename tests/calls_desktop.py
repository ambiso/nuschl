#!/usr/bin/env python3
"""Native desktop call controls and invitations on a loopback Mumble server.

Andi and Bea run in a private headless GNOME Shell with synthetic PulseAudio.
A Mumble-protocol witness checks the public and private chat links. The call
host listens on loopback and holds TLS admission open, so the test exercises the
call UI and invitation lifecycle without granting camera or screen capture.
Set TMPDIR and NUSCHL_WL_TMP to a short disk-backed scratch path.

    python3 tests/calls_desktop.py <binaries> --murmur <mumble-server> \
        --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin --opus-library <libopus.so> \
        --gnome-shell <gnome-shell> --output <directory>
"""
import argparse
import hashlib
import html
import json
from pathlib import Path
import re
import shlex
import socket
import ssl
import sys
import tempfile
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client  # noqa: E402
from voice_servers import VoiceTest  # noqa: E402

AT_SPI_RADIO_MENU_ITEM = "role 45"


class CallTest(VoiceTest):
    def desktop(self, name, registry, known):
        profile = self.root / name
        (profile / "config").mkdir(parents=True)
        (profile / "config/servers.json").write_text(json.dumps([{
            "id": str(uuid.uuid4()), "name": "Calls", "host": "127.0.0.1", "port": self.murmur.port,
            "username": name, "certificate_sha256": self.pin,
        }]))
        (profile / "config/settings.json").write_text(json.dumps({
            "server_icons": False, "call_host": self.call_host,
        }))
        opened = self.root / f"{name}-opened.txt"
        bin_dir = self.root / f"{name}-bin"
        bin_dir.mkdir()
        launcher = bin_dir / "xdg-open"
        launcher.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> {shlex.quote(str(opened))}\n")
        launcher.chmod(0o755)
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        env.pop("BROWSER", None)
        env["PATH"] = f"{bin_dir}:{env['PATH']}"
        env.update(NO_PROXY="*", no_proxy="*")
        process = self.spawn([str(self.args.binaries / "nuschl"), "--profile-dir", str(profile),
                              f"mumble://{name}@127.0.0.1:{self.murmur.port}/Lobby"],
                             env | {"NUSCHL_AUDIO_BACKEND": "pulse"}, f"desktop-{name}.log")
        app = eventually(lambda: next((node for node in registry.children() if node.name.lower() == "nuschl"
                                       and node.destination not in known), None),
                         f"no accessibility tree for {name}", timeout=45)
        self.witness.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == name
                          and event["user"]["channel"] == 1, timeout=45)
        return app, opened, process.pid

    @staticmethod
    def find(app, role, name, prefix=False):
        return next((node for node, node_role, node_name, _ in items(app) if role in (None, node_role)
                     and (node_name.startswith(name) if prefix else node_name == name)), None)

    def direct(self, app, name):
        eventually(lambda: self.find(app, "tree item", name, prefix=True), f"no user {name!r}").press()
        eventually(lambda: any(role == "list" and label.startswith("Messages with") and label.endswith(name)
                               for _, role, label, _ in items(app)), f"no conversation with {name}")

    def control(self, app, name, shown=True):
        def matches():
            node = self.find(app, "push button", name)
            return node if shown else node is None

        return eventually(matches, f"expected {name!r} to be {'shown' if shown else 'hidden'}", timeout=15)

    def press(self, app, name):
        self.control(app, name).press()

    def video_stats(self, app, show):
        self.press(app, "Video options")
        label = "Video stats" if show else "Hide video stats"
        eventually(lambda: self.find(app, "menu item", label), f"missing {label!r} menu item").press()
        eventually(lambda: not self.find(app, "menu", "Context menu"), "video options did not close")
        eventually(lambda: bool(self.find(app, None, "Video stats")) == show,
                   f"expected video stats to be {'shown' if show else 'hidden'}")
        if show:
            for field in ("Resolution", "Video payload bitrate", "Connection"):
                eventually(lambda: self.find(app, None, field), f"video stats lack {field!r}")

    def invitation(self, app, sender="Andi"):
        eventually(lambda: self.find(app, None, f"Video call · {sender} · {self.call_host}"),
                   f"no call card for {sender}", timeout=15)

    def witness_link(self):
        marker = f"https://{self.call_host}/call#"
        event = self.witness.wait(lambda event: event["kind"] == "text" and marker in event["body"])
        body = html.unescape(event["body"])
        secret = r"[A-Za-z0-9_-]{43}"
        link = re.search(rf"{re.escape(marker)}v=1&k={secret}&h={secret}(?![A-Za-z0-9_-])", body)
        assert link, body
        assert "&p=" not in body, body
        return event["target"], link.group()

    def copy_link(self, app, pid, link):
        # The compositor, its clipboard selection and both clients are private.
        self.shell.eval("global.get_window_actors().map(actor => actor.meta_window)"
                        f".find(window => window.get_pid() === {pid}).activate(global.get_current_time()); 'ok'")
        eventually(lambda: self.shell.eval(f"global.display.focus_window?.get_pid() === {pid}") == "true",
                   "the call window did not get focus")
        clipboard = "imports.gi.St.Clipboard.get_default()"
        selection = "imports.gi.St.ClipboardType.CLIPBOARD"
        self.shell.eval(f"{clipboard}.set_text({selection}, 'call fixture sentinel'); 'ok'")
        x, y, width, height = self.control(app, "Copy link").extents()
        left, top = json.loads(self.shell.eval(
            "const bounds = global.display.focus_window.get_buffer_rect(); JSON.stringify([bounds.x, bounds.y])"))
        assert width > 0 and height > 0
        self.shell.click(left + x + width // 2, top + y + height // 2)

        def copied():
            self.shell.eval(f"global._callLink = undefined; {clipboard}.get_text({selection},"
                            " (_clipboard, text) => { global._callLink = text; }); 'ok'")
            return self.shell.eval(f"global._callLink === {json.dumps(link)}") == "true"

        eventually(copied, "the private clipboard did not receive the exact guest link", timeout=10)

    def inline(self, app):
        self.control(app, "Full screen")

        def regions_are_disjoint():
            full_screen = self.find(app, "push button", "Full screen")
            messages = self.find(app, "list", "Messages in #Lobby")
            if not full_screen or not messages:
                return False
            _, call_top, _, call_height = full_screen.extents()
            _, chat_top, _, chat_height = messages.extents()
            return call_height > 0 and chat_height > 0 and call_top + call_height <= chat_top

        eventually(regions_are_disjoint, "inline call and chat have overlapping or empty regions", timeout=15)

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, 1440, 950,
                                  log=(self.args.output / "shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()

        # TCP connects to this owned listener. TLS waits with no application
        # reading it, so the desktop never reaches the portal request.
        with socket.create_server(("127.0.0.2", 0)) as listener:
            self.call_host = f"127.0.0.2:{listener.getsockname()[1]}"
            directory = self.root / "murmur"
            directory.mkdir()
            self.murmur = Murmur(self.args.murmur, directory, free_port())
            self.murmurs.append(self.murmur)
            self.murmur.seed()
            self.murmur.start()
            der = ssl.PEM_cert_to_DER_cert(ssl.get_server_certificate(("127.0.0.1", self.murmur.port)))
            self.pin = hashlib.sha256(der).hexdigest()
            self.witness = self.peers["Witness"] = Client(self.args.binaries, "127.0.0.1", self.murmur.port,
                                                          self.pin, "Witness", self.root / "witness")
            self.witness.send(kind="join", channel=1)
            andi, andi_opened, andi_pid = self.desktop("Andi", registry, set())
            bea, bea_opened, bea_pid = self.desktop("Bea", registry, {andi.destination})
            self.control(andi, "Share screen")
            self.control(andi, "Start call", shown=False)
            self.control(andi, "Start call in browser", shown=False)

            self.press(andi, "Start meeting")
            self.inline(andi)
            self.invitation(andi, "You")
            target, link = self.witness_link()
            assert target == {"kind": "channel", "id": 1}
            self.copy_link(andi, andi_pid, link)
            self.invitation(bea)
            self.copy_link(bea, bea_pid, link)
            self.press(andi, "Minimize call")
            self.invitation(andi, "You")
            self.copy_link(andi, andi_pid, link)
            self.press(andi, "Return to call")
            self.inline(andi)
            self.press(andi, "End call")
            self.control(andi, "Start meeting")
            self.control(bea, "Join video call", shown=False)
            self.witness.wait(lambda event: event["kind"] == "text" and "Call ended" in event["body"])

            self.press(andi, "Start camera")
            self.inline(andi)
            self.control(andi, "Open in browser", shown=False)
            chat = self.find(andi, "list", "Messages in #Lobby")
            chat_bounds = chat.extents()
            self.press(andi, "Quality · Auto")
            quality = eventually(lambda: self.find(andi, "menu", "Video quality"),
                                 "video quality dropdown did not open")
            assert chat.extents() == chat_bounds, "quality choices changed the chat layout"
            eventually(lambda: self.find(quality, AT_SPI_RADIO_MENU_ITEM, "720p30"),
                       "720p30 radio menu item missing").press()
            self.control(andi, "Quality · 720p30")
            eventually(lambda: not self.find(andi, "menu", "Video quality"),
                       "video quality dropdown did not close")
            self.control(andi, "Video stats", shown=False)
            self.video_stats(andi, show=True)
            self.video_stats(andi, show=False)
            assert self.witness_link()[0] == {"kind": "channel", "id": 1}
            self.invitation(bea)
            self.control(bea, "Open in browser", shown=False)

            self.press(bea, "Join video call")
            self.inline(bea)
            self.press(bea, "Full screen")
            self.control(bea, "Exit full screen")
            self.video_stats(bea, show=True)
            self.video_stats(bea, show=False)
            eventually(lambda: not self.find(bea, "list", "Messages in #Lobby"),
                       "full screen kept the inline chat tree", timeout=15)
            self.press(bea, "Exit full screen")
            self.inline(bea)
            self.press(bea, "Minimize call")
            self.invitation(bea)
            self.control(bea, "Full screen", shown=False)
            self.press(bea, "Return to call")
            self.inline(bea)

            self.press(andi, "End call")
            self.control(andi, "Start camera")
            self.control(bea, "Leave video", shown=False)
            self.control(bea, "Join video call", shown=False)
            self.witness.wait(lambda event: event["kind"] == "text" and "Call ended" in event["body"])

            # A direct screen call makes the card available to Bea in the
            # direct conversation. The Mumble witness sees only channel links.
            self.direct(andi, "Bea")
            self.direct(bea, "Andi")
            self.press(andi, "Share screen")
            self.invitation(andi, "You")
            self.invitation(bea)
            self.press(bea, "Join video call")
            self.press(bea, "Leave video")
            self.control(bea, "Join video call")
            self.press(andi, "End call")
            self.control(bea, "Join video call", shown=False)

            # The same action carries the link as a private Mumble message
            # when the call recipient uses the Mumble protocol only.
            self.direct(andi, "Witness")
            self.press(andi, "Share screen")
            andi_session = next(user["session"] for user in self.witness.users.values() if user["name"] == "Andi")
            assert self.witness_link()[0] == {"kind": "direct", "id": andi_session}
            self.press(andi, "End call")

            assert not andi_opened.exists() and not bea_opened.exists(), "a call launched the browser"
            assert not self.microphone_open()
            print("Calls passed for native controls, channel and direct invitations, and cleanup.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    for name in ("murmur", "at-spi", "pulse-bin", "opus-library", "gnome-shell", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-calls-") as temporary:
        test = CallTest(args, Path(temporary))
        try:
            test.run()
        finally:
            test.close()


if __name__ == "__main__":
    main()
