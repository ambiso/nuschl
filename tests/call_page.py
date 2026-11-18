#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The call page in headless Chromium and Firefox (nix/call-page-vm.nix).

Three synthetic publishers (tests/call_publisher.js, Chromium) join a call on the
stream host: the creator and a second participant publish a camera each, a third
a shared screen. A Chromium subscriber and a relay-only Firefox subscriber show
every source decrypted, in its colour, the screen in the large view, and the full
roster; a pinned camera takes the large view; a publisher leaving takes its tile
and roster row along. A page user then publishes Chromium's fake camera and
screen with Start camera and Share screen: nothing is captured before the
buttons, no audio is asked for, the subscribers show both, a participant without
AV1 switches the call to VP9 and back while the streams keep going, and Stop
ends the captures and the tiles. A Chromium without encoded transforms and a
broken link get their refusals. Screenshots land in the directory given as the
only argument.

Usage: call_page.py OUTPUT_DIRECTORY  (CHROMIUM, FIREFOX in the environment)
"""

import base64
import hashlib
import hmac
import json
import os
import secrets
import shutil
import sys
import time
from pathlib import Path

from selenium import webdriver
from selenium.webdriver.common.by import By

ORIGIN = "https://stream.test"
OUT = Path(sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)
COLOURS = {"a": (220, 40, 40), "b": (40, 190, 70), "c": (40, 80, 220)}


def hkdf(secret: bytes, label: str, length: int) -> bytes:
    """The key schedule of apps/stream-sfu/src/protocol.rs."""
    key = hmac.new(b"nuschl/stream/v2", secret, hashlib.sha256).digest()
    return hmac.new(key, f"nuschl/stream/v2 {label}".encode() + b"\x01", hashlib.sha256).digest()[:length]


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


S, P = secrets.token_bytes(32), secrets.token_bytes(32)
LINK = f"#v=1&k={b64(S)}&h={b64(hashlib.sha256(hkdf(P, 'host', 32)).digest())}"
CREATOR = f"{LINK}&p={b64(P)}"


# The page user's capture calls, with whether each asked for audio and the surface
# it preselected, and their tracks. In the screen picker the user picks a monitor.
RECORD_CAPTURE = """
window.captures = []; window.capturedTracks = [];
for (const name of ['getUserMedia', 'getDisplayMedia']) {
  const original = navigator.mediaDevices[name].bind(navigator.mediaDevices);
  navigator.mediaDevices[name] = async (constraints) => {
    window.captures.push([name, Boolean(constraints.audio), constraints.video.displaySurface || null]);
    const stream = await original(name === 'getDisplayMedia' ? {video: true} : constraints);
    window.capturedTracks.push(...stream.getTracks());
    return stream;
  };
}
"""
# A browser that decodes VP9 and no AV1.
WITHOUT_AV1 = """
const capabilities = RTCRtpReceiver.getCapabilities;
RTCRtpReceiver.getCapabilities = (kind) => {
  const found = capabilities.call(RTCRtpReceiver, kind);
  if (found) found.codecs = found.codecs.filter((codec) => !/av1/i.test(codec.mimeType));
  return found;
};
"""


def chromium(*, script=None, devices=False):
    """Headless Chromium; `script` runs before every page's own, `devices` gives it
    Chromium's fake camera and screen without permission prompts."""
    options = webdriver.ChromeOptions()
    options.binary_location = os.environ["CHROMIUM"]
    options.set_capability("goog:loggingPrefs", {"browser": "ALL"})
    for argument in ["--headless=new", "--no-sandbox", "--ignore-certificate-errors", "--window-size=1280,800",
                     "--autoplay-policy=no-user-gesture-required", "--disable-dev-shm-usage"]:
        options.add_argument(argument)
    if devices:
        options.add_argument("--use-fake-device-for-media-stream")
        options.add_argument("--use-fake-ui-for-media-stream")
    driver = webdriver.Chrome(options=options, service=webdriver.ChromeService(shutil.which("chromedriver")))
    if script:
        driver.execute_cdp_cmd("Page.addScriptToEvaluateOnNewDocument", {"source": script})
    return driver


def firefox():
    options = webdriver.FirefoxOptions()
    options.binary_location = os.environ["FIREFOX"]
    options.add_argument("-headless")
    options.accept_insecure_certs = True
    options.set_preference("media.autoplay.default", 0)
    driver = webdriver.Firefox(options=options, service=webdriver.FirefoxService(shutil.which("geckodriver")))
    driver.set_window_size(1280, 800)
    return driver


def wait(driver, script, what, timeout=90):
    """Polls `script` (returning a value) until it is truthy; returns it."""
    deadline = time.monotonic() + timeout
    while True:
        value = driver.execute_script(script)
        if value:
            return value
        if time.monotonic() > deadline:
            logs = driver.execute_script("return document.body.innerText")
            raise AssertionError(f"{what}: timed out; page text: {logs!r}")
        time.sleep(0.5)


# Each tile's peer, placement, state, size and colour below the label and the bar.
TILES = """
return [...document.querySelectorAll('.tile')].map(tile => {
  const video = tile.querySelector('video');
  const canvas = Object.assign(document.createElement('canvas'), {width: 16, height: 9});
  const context = canvas.getContext('2d');
  context.drawImage(video, 0, 0, 16, 9);
  const [r, g, b] = context.getImageData(8, 6, 1, 1).data;
  return {peer: Number(tile.dataset.peer), screen: 'screen' in tile.dataset, state: tile.dataset.state,
          large: tile.parentNode.id === 'large', width: video.videoWidth, colour: [r, g, b],
          label: tile.querySelector('figcaption').textContent};
});
"""


def close(colour, expected):
    if callable(expected):
        return expected(colour)
    return all(abs(a - b) <= 45 for a, b in zip(colour, expected))


def green(colour):
    """Chromium's fake camera and screen: a green picture."""
    r, g, b = colour
    return g > 48 and g > r + 30 and g > b + 30


def showing(driver, expected, what):
    """Waits until the tiles are exactly `expected` ({(peer, screen): (colour, large)}),
    playing; a large of None takes either place."""
    def matches(tile):
        colour, large = expected.get((tile["peer"], tile["screen"]), (None, None))
        return (colour is not None and tile["state"] == "playing" and tile["width"] > 0
                and close(tile["colour"], colour) and large in (None, tile["large"]))

    def check():
        tiles = driver.execute_script(TILES)
        ok = len(tiles) == len(expected) and all(matches(tile) for tile in tiles)
        return tiles if ok else None
    deadline = time.monotonic() + 120
    while (tiles := check()) is None:
        if time.monotonic() > deadline:
            raise AssertionError(f"{what}: {json.dumps(driver.execute_script(TILES))}")
        time.sleep(0.5)
    print(what, json.dumps(tiles), flush=True)
    return tiles


def roster(driver):
    return driver.execute_script("return [...document.querySelectorAll('#roster li')].map(li => li.innerText)")


def heading(driver):
    return wait(driver, "const h = document.querySelector('#ending:not([hidden]) h1'); return h && h.textContent",
                "refusal")


drivers = []
try:
    # Every browser starts first, before the media load: a busy host slows startup most.
    user = chromium(script=RECORD_CAPTURE, devices=True)
    vp9 = chromium(script=WITHOUT_AV1)
    plain = chromium(script="delete window.RTCRtpScriptTransform")
    drivers += [user, vp9, plain]
    publishers, pages = {}, {}
    for name, fragment in [("a", CREATOR), ("b", LINK), ("c", LINK)]:
        driver = pages[name] = chromium()
        drivers.append(driver)
        driver.get(f"{ORIGIN}/call_publisher.html{fragment}")
        publishers[name] = wait(driver, "return window.peer && window.peer()", f"publisher {name} joined")
        if name == "a":
            chrome, fox = chromium(), firefox()
            drivers += [chrome, fox]
            chrome.get(f"{ORIGIN}/call{LINK}")
            fox.get(f"{ORIGIN}/call?relay{LINK}")
    a, b, c = (publishers[name] for name in "abc")
    for driver in [chrome, fox]:
        status = wait(driver, "const s = document.getElementById('status').textContent;"
                              "return s !== 'Connecting' && s", "subscriber joined")
        print("status", status, flush=True)
    assert "Relay only" in fox.find_element(By.ID, "status").text
    assert "Relay only" not in chrome.find_element(By.ID, "status").text
    codec = chrome.find_element(By.ID, "status").text.split(" · ")[0]
    assert codec in ("AV1", "VP9") and fox.find_element(By.ID, "status").text.startswith(codec), codec

    pages["a"].execute_script("publish([{color: 'rgb(220,40,40)', label: 'Camera A'}])")
    pages["b"].execute_script("publish([{color: 'rgb(40,190,70)', label: 'Camera B'}])")
    cameras = {(a, False): (COLOURS["a"], False), (b, False): (COLOURS["b"], False)}
    for driver in [chrome, fox]:
        showing(driver, cameras, "two cameras in the grid")
    chrome.save_screenshot(str(OUT / "grid.png"))

    pages["c"].execute_script("publish([{color: 'rgb(40,80,220)', label: 'Screen C', screen: true}])")
    shared = {**cameras, (c, True): (COLOURS["c"], True)}
    for driver in [chrome, fox]:
        showing(driver, shared, "the screen in the large view")
    chrome.save_screenshot(str(OUT / "large-screen.png"))
    fox.save_screenshot(str(OUT / "large-screen-firefox.png"))

    # The full roster on both subscribers: themselves, the other subscriber and the publishers.
    for driver in [chrome, fox]:
        rows = roster(driver)
        print("roster", rows, flush=True)
        assert len(rows) == 5 and rows[0].startswith("You"), rows
        named = {row.split("\n")[0]: row for row in rows[1:]}
        assert "Host" in named[f"Participant {a}"] and "Camera" in named[f"Participant {a}"], rows
        assert "Screen" in named[f"Participant {c}"], rows
        assert driver.find_element(By.ID, "count").text == "5"
        assert not driver.find_element(By.ID, "host-actions").is_displayed()

    # A pinned camera takes the large view; the screen joins the grid.
    chrome.find_element(By.CSS_SELECTOR, f".tile[data-peer='{a}'] .pin").click()
    pinned = {(a, False): (COLOURS["a"], True), (c, True): (COLOURS["c"], False)}
    showing(chrome, {**pinned, (b, False): cameras[b, False]}, "pinned camera")
    chrome.save_screenshot(str(OUT / "large-pinned.png"))

    # A publisher that leaves takes its tile and roster row along.
    pages["b"].quit()
    drivers.remove(pages["b"])
    showing(chrome, pinned, "after a publisher left")
    wait(chrome, "return document.querySelectorAll('#roster li').length === 4", "roster after a leave")

    # A page user publishes Chromium's fake camera and screen from the page's own
    # buttons. Nothing is captured before a button, and nothing asks for audio.
    user.get(f"{ORIGIN}/call{LINK}")
    wait(user, "return !document.getElementById('camera').disabled", "Start camera ready")
    time.sleep(3)
    assert user.execute_script("return window.captures") == [], "a capture before the button"
    user.find_element(By.ID, "camera").click()
    wait(user, "return document.getElementById('camera').textContent === 'Stop camera'", "camera on")
    user.find_element(By.ID, "screen").click()
    wait(user, "return document.getElementById('screen').textContent === 'Stop sharing'", "screen on")
    assert user.execute_script("return window.captures") == [["getUserMedia", False, None],
                                                              ["getDisplayMedia", False, "window"]]
    assert user.find_element(By.ID, "notice").text == "Sharing your whole screen"
    u = user.execute_script("return Number(document.querySelector('.tile[data-own]').dataset.peer)")
    published = {(u, False): (green, False), (u, True): (green, None)}
    everyone = {chrome: {**pinned, **published},
                fox: {(a, False): (COLOURS["a"], None), (c, True): (COLOURS["c"], None), **published}}
    for driver, expected in everyone.items():
        showing(driver, expected, "a page user's camera and screen")
    showing(user, {(a, False): (COLOURS["a"], False), (c, True): (COLOURS["c"], True),
                   (u, False): (green, False), (u, True): (green, False)}, "the page user's own view")
    user.save_screenshot(str(OUT / "publishing.png"))
    chrome.save_screenshot(str(OUT / "publishing-subscriber.png"))

    # A participant without AV1 switches the call to VP9: every publisher re-encodes,
    # and the streams keep going; its leave switches back to AV1.
    def codec_everywhere(codec):
        for driver in [chrome, fox, user]:
            wait(driver, f"return document.getElementById('status').textContent.startsWith('{codec}')", codec)

    def advancing(driver, peer, what):
        """The peer's tiles play and their pictures change (the fake devices animate)."""
        showing(driver, everyone[driver], what)
        pictures = f"""return [...document.querySelectorAll('.tile[data-peer="{peer}"] video')].map(video => {{
          const canvas = Object.assign(document.createElement('canvas'), {{width: 32, height: 18}});
          canvas.getContext('2d').drawImage(video, 0, 0, 32, 18);
          return Array.from(canvas.getContext('2d').getImageData(0, 0, 32, 18).data).join();
        }})"""
        before = driver.execute_script(pictures)
        time.sleep(2)
        after = driver.execute_script(pictures)
        assert len(before) == 2 and all(x != y for x, y in zip(before, after)), what

    vp9.get(f"{ORIGIN}/call{LINK}")
    codec_everywhere("VP9")
    showing(vp9, {(a, False): (COLOURS["a"], False), (c, True): (COLOURS["c"], None), **published},
            "VP9 from every publisher")
    for driver in [chrome, fox]:
        advancing(driver, u, "after the switch to VP9")
    vp9.quit()
    drivers.remove(vp9)
    codec_everywhere("AV1")
    for driver in [chrome, fox]:
        advancing(driver, u, "after the switch back to AV1")

    # Stop ends both captures and takes the tiles off every page.
    user.find_element(By.ID, "camera").click()
    user.find_element(By.ID, "screen").click()
    for driver in [chrome, fox]:
        wait(driver, f"return !document.querySelector('.tile[data-peer=\"{u}\"]')", "tiles after Stop")
    assert user.execute_script("return window.capturedTracks.every(track => track.readyState === 'ended')")
    assert user.find_element(By.ID, "camera").text == "Start camera"

    # Refusals: a browser without encoded transforms, and a link without its keys.
    plain.get(f"{ORIGIN}/call{LINK}")
    assert heading(plain) == "Browser not supported"
    assert plain.save_screenshot(str(OUT / "refusal.png"))
    chrome.get(f"{ORIGIN}/call?again#v=1&k={b64(S)}")
    assert heading(chrome) == "Link not valid"
    print(json.dumps({"passed": True, "codec": codec}), flush=True)
except BaseException:
    for index, driver in enumerate(drivers):
        driver.save_screenshot(str(OUT / f"failure-{index}.png"))
        print(f"page {index}:", driver.current_url.split("#")[0], driver.execute_script(
            "return document.body.innerText"), flush=True)
        if driver.capabilities["browserName"] != "firefox":
            for entry in driver.get_log("browser"):
                print(f"page {index} console:", entry["message"], flush=True)
    raise
finally:
    for driver in drivers:
        driver.quit()
