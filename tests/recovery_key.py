#!/usr/bin/env python3
"""Recovery key in Settings → Identity: shown on request with a secrecy
warning, copied to the clipboard, hidden by I wrote it down and shown again,
and hidden once Settings closes.

Private headless GNOME Shell, AT-SPI and a loopback nuschld with key
transparency. The desktop creates its identity on connect, with the default
offer of a recovery key.

    NUSCHL_WL_TMP=/tmp python3 tests/recovery_key.py --binaries DIR --at-spi DIR \\
        --output DIR [--gnome-shell PATH]
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

from accessibility import items, start_registry
from gnome_session import GnomeSession
from identity_screenshots import register
from local_nuschld import Nuschld, eventually, free_port
from mumble_client import certificate_pin

WIDTH, HEIGHT = 1120, 760


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-recovery-key-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Nuschld(args.binaries, root / "server", port, extra="[kt]\npublish_interval = 1\n")
        server.seed()
        server.start()
        shell = GnomeSession(args.gnome_shell, WIDTH + 160, HEIGHT + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        processes = []

        def spawn(command, env, log):
            process = subprocess.Popen(command, env=env, stdout=(args.output / log).open("w"),
                                       stderr=subprocess.STDOUT, start_new_session=True)
            processes.append(process)
            return process

        try:
            shell.start()
            env = shell.app_environment()
            env.pop("NO_AT_BRIDGE", None)
            registry = start_registry(args.at_spi, env, spawn)
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            register(args.binaries, port, pin, "Andi", profile)
            (profile / "config/servers.json").write_text(json.dumps([{
                "id": str(uuid.uuid4()), "name": "Nuschl test server", "host": "127.0.0.1",
                "port": port, "username": "Andi", "certificate_sha256": pin,
            }]))
            app = spawn([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                         f"mumble://Andi@127.0.0.1:{port}/Lobby"],
                        env | {"RUST_LOG": "nuschl=info"}, "desktop.log")
            eventually(lambda: app.poll() is None and shell.window("Nuschl"), "no desktop window")
            shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
            nuschl = eventually(lambda: next((node for node in registry.children()
                                              if node.name.lower() == "nuschl"), None),
                                "no desktop accessibility tree", timeout=45)

            def named(name, role=None, prefix=False):
                return next((node for node, kind, label, _ in items(nuschl)
                             if (label.startswith(name) if prefix else label == name)
                             and (role is None or kind == role)), None)

            def press(name):
                eventually(lambda: named(name, "push button"), f"missing {name}").press()

            def reveal(name, prefix=False):
                """Scroll Settings until `name` is drawn (drawn controls are in the tree)."""
                def step():
                    node = named(name, prefix=prefix)
                    if node is None:
                        shell.wheel(40 + WIDTH // 2, 40 + HEIGHT // 2, 3)
                        time.sleep(0.2)
                    return node
                return eventually(step, f"Settings never showed {name}", timeout=60)

            St = "imports.gi.St"
            sentinel = f"recovery-key-sentinel-{uuid.uuid4().hex}"

            def clipboard():
                """The shell's clipboard: one call asks, later calls read the answer."""
                shell.eval(f"global._recoveryCopy = undefined; {St}.Clipboard.get_default().get_text("
                           f"{St}.ClipboardType.CLIPBOARD, (c, t) => {{ global._recoveryCopy = t; }}); 'ok'")
                value = eventually(lambda: (value if (value := json.loads(shell.eval(
                    "JSON.stringify({done: global._recoveryCopy !== undefined, "
                    "text: global._recoveryCopy ?? null})")))["done"] else None),
                    "the clipboard did not answer", timeout=10)
                text = value["text"]
                return None if text in (None, sentinel) else text

            press("Settings")
            # The identity is made on connect; until then Settings shows no key.
            # The warning is read before the key is shown.
            reveal("Show recovery key")
            assert named("Keep this key secret", prefix=True), "no warning before the key is shown"
            reveal("Show recovery key").press()
            # The app sets the clipboard while its window is active.
            shell.eval("global.get_window_actors().map(a => a.meta_window).find(w => "
                       "w.get_title().startsWith('Nuschl')).activate(global.get_current_time()); 'ok'")
            shell.eval(f"{St}.Clipboard.get_default().set_text({St}.ClipboardType.CLIPBOARD, "
                       f"{json.dumps(sentinel)}); 'ok'")
            copy = eventually(lambda: named("Copy", "push button"), "missing Copy")
            # A real key press: Wayland lets an app set the clipboard after user input.
            assert copy.call("Component", "GrabFocus")[0]
            eventually(lambda: copy.focused, "Copy did not take keyboard focus")
            shell.key("Return")
            eventually(lambda: named("✓ Copied", "push button"), "Copy did not say it copied")
            key = eventually(clipboard, "Copy put nothing on the clipboard")
            assert named(key, "label"), "the copied text is not the shown recovery key"
            shell.screenshot(args.output / "recovery-key.png")
            eventually(lambda: named("Copy", "push button"), "Copied did not return to Copy", timeout=10)
            # I wrote it down hides the key; it can be shown again.
            press("I wrote it down")
            eventually(lambda: named(key, "label") is None, "I wrote it down left the key shown")
            reveal("Show recovery key").press()
            eventually(lambda: named(key, "label"), "the key could not be shown again")
            press("Done")
            eventually(lambda: named("Settings", "dialog") is None, "Settings did not close")
            press("Settings")
            reveal("Show recovery key")
            assert named(key, "label") is None, "the recovery key showed again after reopening Settings"
            result = {"passed": True, "copied": True, "warned": True, "shown_again": True,
                      "hidden_after_close": True}
            (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result))
        finally:
            for process in reversed(processes):
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
            shell.stop()
            server.stop()


if __name__ == "__main__":
    main()
