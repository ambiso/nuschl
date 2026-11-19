#!/usr/bin/env python3
"""Calls on a stock Mumble server: Start call, the call card, Join, Rotate link,
End call and the creator's lease (docs/desktop.md, "Calls").

Two real desktops (Andi and Bea) in a private headless GNOME Shell, operated
over AT-SPI, a stock Mumble client ("Witness") in the same channel of a
loopback Murmur, and an owned loopback call host that answers the lease. Each
desktop's `xdg-open` is a script that records the URL in place of a browser.
It exercises the browser fallback against a local call host. Checks:

- Start call in browser opens the creator's page (S, V and P, V matching P) and posts
  the Join link (S and V) as text, which the stock client receives;
- the lease reaches the call host with stream_id and host_key; P stays with Andi;
- Bea's card names Andi and the host, and Join opens the page with S and V;
- Rotate link opens a new creator page and Bea's Join follows the new S;
- the call ends when the call host reports the room gone; Share screen in browser starts
  a new one, and End call ends it and releases the lease; Bea's card goes away
  both times;
- a direct call from Andi to Bea shows Bea's card in her conversation with
  Andi, Join opens its link, and the Witness sees neither; a direct call to
  the Witness reaches it as a private message.

    python3 tests/calls_desktop.py <binaries> --murmur <mumble-server> \\
        --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin --opus-library <libopus.so> \\
        --gnome-shell <gnome-shell> --output <directory>
"""
import argparse
import base64
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import ssl
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client  # noqa: E402
from voice_servers import VoiceTest, WIDTH, HEIGHT  # noqa: E402

SECRET = r"[A-Za-z0-9_-]{43}"
LABEL = b"nuschl/stream/v2"


def hkdf(secret, name, length):
    """HKDF-SHA256 as nuschl-stream derives from S and P."""
    prk = hmac.new(LABEL, secret, hashlib.sha256).digest()
    return hmac.new(prk, LABEL + b" " + name + b"\x01", hashlib.sha256).digest()[:length]


def decode(value):
    return base64.urlsafe_b64decode(value + "=")


class CallHost(BaseHTTPRequestHandler):
    """The lease endpoint over loopback HTTP: 204 while `room` is set, else 404."""
    room = True
    requests = []

    def answer(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        CallHost.requests.append((self.command, self.path, json.loads(body)))
        self.send_response(204 if CallHost.room else 404)
        self.end_headers()

    do_POST = do_DELETE = answer

    def log_message(self, *_):
        pass


class CallTest(VoiceTest):
    def desktop(self, name, registry, known):
        profile = self.root / name
        (profile / "config").mkdir(parents=True)
        (profile / "config/servers.json").write_text(json.dumps([{
            "id": str(uuid.uuid4()), "name": "Calls", "host": "127.0.0.1", "port": self.murmur.port,
            "username": name, "certificate_sha256": self.pin,
        }]))
        (profile / "config/settings.json").write_text(json.dumps({
            "server_icons": False, "call_host": f"127.0.0.1:{self.host.server_port}",
        }))
        opened = self.root / f"{name}-opened.txt"
        bin_dir = self.root / f"{name}-bin"
        bin_dir.mkdir()
        (bin_dir / "xdg-open").write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> {opened}\n")
        (bin_dir / "xdg-open").chmod(0o755)
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        env.pop("BROWSER", None)
        env["PATH"] = f"{bin_dir}:{env['PATH']}"
        self.spawn([str(self.args.binaries / "nuschl"), "--profile-dir", str(profile),
                    f"mumble://{name}@127.0.0.1:{self.murmur.port}/Lobby"],
                   env | {"NUSCHL_AUDIO_BACKEND": "pulse"}, f"desktop-{name}.log")
        app = eventually(lambda: next((node for node in registry.children() if node.name.lower() == "nuschl"
                                       and node.destination not in known), None),
                         f"no accessibility tree for {name}", timeout=45)
        self.witness.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == name
                          and event["user"]["channel"] == 1, timeout=45)
        return app, opened

    @staticmethod
    def find(app, role, name, prefix=False):
        return next((node for node, node_role, node_name, _ in items(app) if role in (None, node_role)
                     and (node_name.startswith(name) if prefix else node_name == name)), None)

    def direct(self, app, name):
        """Open the direct conversation with `name` from the channel tree."""
        eventually(lambda: self.find(app, "tree item", name, prefix=True), f"no user {name!r}").press()
        eventually(lambda: any(role == "list" and label.startswith("Messages with") and label.endswith(name)
                               for _, role, label, _ in items(app)), f"no conversation with {name}")

    def press(self, app, name):
        eventually(lambda: self.find(app, "push button", name), f"no button {name!r}").press()

    @staticmethod
    def opened(path, count):
        return eventually(lambda: path.exists() and len(lines := path.read_text().split()) >= count
                          and lines[count - 1], f"{path.name}: URL {count} not opened")

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, 2 * WIDTH + 120, HEIGHT + 100,
                                  log=(self.args.output / "shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()
        self.host = ThreadingHTTPServer(("127.0.0.1", 0), CallHost)
        threading.Thread(target=self.host.serve_forever, daemon=True).start()
        try:
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
            andi, andi_opened = self.desktop("Andi", registry, set())
            bea, bea_opened = self.desktop("Bea", registry, {andi.destination})
            origin = f"https://127.0.0.1:{self.host.server_port}"
            creator = rf"{re.escape(origin)}/call#v=1&k=({SECRET})&h=({SECRET})&p=({SECRET})"

            # Start call: Andi's page holds S, V and P; the channel gets S and V.
            self.press(andi, "Start call in browser")
            page = self.opened(andi_opened, 1)
            match = re.fullmatch(creator, page)
            assert match, page
            secret, check, publisher = match.groups()
            host_key = hkdf(decode(publisher), b"host", 32)
            assert decode(check) == hashlib.sha256(host_key).digest()
            text = self.witness.wait(lambda event: event["kind"] == "text" and secret in event["body"],
                                     timeout=15)["body"]
            assert f"/call#v=1&amp;k={secret}&amp;h={check}" in text and publisher not in text, text
            # The lease carries the room and host_key; P stays with Andi.
            lease = {"stream_id": hkdf(decode(secret), b"stream id", 16).hex(), "host_key": host_key.hex()}
            eventually(lambda: ("POST", "/sfu/lease", lease) in CallHost.requests, "no lease renewal", timeout=15)
            assert all(publisher not in json.dumps(body) and decode(publisher).hex() not in json.dumps(body)
                       for _, _, body in CallHost.requests)

            # Bea's card and Join.
            eventually(lambda: self.find(bea, None, f"Video call · Andi · 127.0.0.1:{self.host.server_port}"),
                       "Bea has no call card")
            self.press(bea, "Open in browser")
            assert self.opened(bea_opened, 1) == f"{origin}/call#v=1&k={secret}&h={check}"
            self.shot("card")

            # Rotate link: a new page for Andi, a new S for Bea.
            self.press(andi, "Rotate link")
            rotated = self.opened(andi_opened, 2)
            new_secret, new_check, _ = re.fullmatch(creator, rotated).groups()
            assert new_secret != secret and new_check != check
            self.witness.wait(lambda event: event["kind"] == "text" and new_secret in event["body"], timeout=15)

            def joins(expected):
                self.press(bea, "Open in browser")
                time.sleep(0.5)
                return bea_opened.read_text().split()[-1] == expected
            eventually(lambda: joins(f"{origin}/call#v=1&k={new_secret}&h={new_check}"),
                       "Join did not follow the new link")

            # The room goes away: the lease ends the call and the card goes.
            CallHost.room = False
            eventually(lambda: self.find(andi, "push button", "Start call in browser"), "the lease did not end the call",
                       timeout=30)
            eventually(lambda: not self.find(bea, "push button", "Open in browser"), "Bea kept an ended call")

            # Share screen starts a call too; End call releases the lease.
            CallHost.room = True
            self.press(andi, "Share screen in browser")
            assert re.fullmatch(creator, self.opened(andi_opened, 3))
            eventually(lambda: self.find(bea, "push button", "Open in browser"), "Bea has no card for the new call")
            self.press(andi, "End call")
            eventually(lambda: any(method == "DELETE" for method, _, _ in CallHost.requests),
                       "End call kept the lease")
            eventually(lambda: not self.find(bea, "push button", "Open in browser"), "Bea kept an ended call")
            self.witness.wait(lambda event: event["kind"] == "text" and "Call ended" in event["body"], timeout=15)

            # A direct call: Andi and Bea alone. The Witness gets neither the
            # link nor a card; Bea's card is in her conversation with Andi.
            self.direct(andi, "Bea")
            self.press(andi, "Start call in browser")
            secret, check, _ = re.fullmatch(creator, self.opened(andi_opened, 4)).groups()
            self.direct(bea, "Andi")
            eventually(lambda: self.find(bea, None, f"Video call · Andi · 127.0.0.1:{self.host.server_port}"),
                       "Bea has no card for the direct call")
            self.press(bea, "Open in browser")
            eventually(lambda: bea_opened.read_text().split()[-1] == f"{origin}/call#v=1&k={secret}&h={check}",
                       "Join did not open the direct call")
            self.shot("direct-card")
            assert not any(secret in event.get("body", "") for event in list(self.witness.events)), \
                "the Witness saw the direct call"
            self.press(andi, "End call")
            eventually(lambda: not self.find(bea, "push button", "Open in browser"), "Bea kept an ended direct call")

            # A direct call with a stock client: the link arrives as a private message.
            self.direct(andi, "Witness")
            self.press(andi, "Share screen in browser")
            secret = re.fullmatch(creator, self.opened(andi_opened, 5)).group(1)
            private = self.witness.wait(lambda event: event["kind"] == "text" and secret in event["body"],
                                        timeout=15)
            andi_session = next(user["session"] for user in self.witness.users.values() if user["name"] == "Andi")
            assert private["target"] == {"kind": "direct", "id": andi_session}, private
            print("Calls: Start call, card, Join, Rotate link, lease end, End call and direct calls passed.",
                  flush=True)
        finally:
            self.host.shutdown()

    def shot(self, name):
        time.sleep(0.8)
        print(self.shell.screenshot(self.args.output / f"calls-{name}.png"), flush=True)


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
