#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""A fixture import service for the music bot's "Add music" (docs/musicbot.md).

Speaks the import service protocol below `/api/integration/v1/` with a bearer
key. Imports advance from queued to downloading to handed_over within a few
seconds. A search for "busy" answers `rate_limited`; album LIMITED answers
`limit_reached` for the requester. `GET /fixture/seen` (no key) lists what was
asked, so a test can check the requesters and the key the bot sent.

tests/musicbot.py serves it in process after setting `State.key`.
"""
import base64
import http.server
import json
import threading
import time
import urllib.parse

# A 2x2 red JPEG.
COVER_JPEG = base64.b64decode(
    "/9j/4AAQSkZJRgABAgAAAQABAAD//gAPTGF2YzYzLjEuMTAyAP/bAEMACAQEBAQEBQUFBQUFBgYGBgYGBgYGBgYGBgcHBwgI"
    "CAcHBwYGBwcICAgICQkJCAgICAkJCgoKDAwLCw4ODhERFP/EAEwAAQEAAAAAAAAAAAAAAAAAAAAGAQEBAAAAAAAAAAAAAAAA"
    "AAAGBxABAAAAAAAAAAAAAAAAAAAAABEBAAAAAAAAAAAAAAAAAAAAAP/AABEIAAIAAgMBIgACEQADEQD/2gAMAwEAAhEDEQA/"
    "AIsATX9//9k=")
# Also in the fixture library ("Waves" by "Oscillators", tests/fake_subsonic.py).
HELD = "00000000000000a1"
NEW = "00000000000000a2"
LIMITED = "00000000000000a3"
ALBUMS = {
    HELD: {"id": HELD, "title": "Waves", "artist": "oscillators", "year": 2026,
           "track_count": 2, "release_type": "album", "has_cover": True},
    NEW: {"id": NEW, "title": "Fixture Album", "artist": "Fixture Artist", "year": None,
          "track_count": 3, "release_type": "ep", "has_cover": True},
    LIMITED: {"id": LIMITED, "title": "Too Much", "artist": "Fixture Artist", "year": 2001,
              "track_count": 1, "release_type": "single", "has_cover": False},
}
# Seconds after the request at which an import starts and is handed over.
STARTS, HANDS_OVER = 2, 5
LIMIT = 100


class State:
    lock = threading.Lock()
    key = ""
    imports = []  # newest first
    seen = []


def view(record, requester):
    album = ALBUMS[record["album_id"]]
    age = time.time() - record["requested_at"]
    total = album["track_count"]
    if age >= HANDS_OVER:
        status, done, handed_over_at = "handed_over", total, int(record["requested_at"]) + HANDS_OVER
    elif age >= STARTS:
        status, done, handed_over_at = "downloading", 1, None
    else:
        status, done, handed_over_at = "queued", 0, None
    return {
        "id": record["id"], "album_id": record["album_id"], "status": status,
        "title": album["title"], "artist": album["artist"], "year": album["year"],
        "release_type": album["release_type"], "has_cover": album["has_cover"],
        "tracks_total": total, "tracks_done": done, "tracks_failed": 0,
        "requested_at": int(record["requested_at"]), "handed_over_at": handed_over_at,
        "mine": requester in record["requesters"],
    }


def budget(requester=None):
    used = sum(ALBUMS[record["album_id"]]["track_count"] for record in State.imports
               if requester is None or requester in record["requesters"])
    return {"limit": LIMIT, "used": used, "next_free_at": None}


class Service(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send(self, status, body, kind="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def error(self, status, code, **extra):
        self.send(status, {"error": code, **extra})

    def authorized(self, path, requester):
        good = self.headers.get("Authorization") == f"Bearer {State.key}"
        with State.lock:
            State.seen.append({"method": self.command, "path": path,
                               "requester": requester, "authorized": good})
        if not good:
            self.error(401, "unauthenticated")
        return good

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        query = urllib.parse.parse_qs(url.query)
        if url.path == "/fixture/seen":
            with State.lock:
                return self.send(200, State.seen)
        requester = query.get("requester", [""])[0]
        if not self.authorized(url.path, requester):
            return
        prefix = "/api/integration/v1/"
        path = url.path[len(prefix):] if url.path.startswith(prefix) else None
        if path == "search":
            text = query.get("q", [""])[0].lower()
            if text == "busy":
                return self.error(429, "rate_limited", retry_after=2)
            hits = [album for album in ALBUMS.values()
                    if text in (album["title"] + " " + album["artist"]).lower()]
            return self.send(200, {"albums": hits[:int(query.get("limit", ["25"])[0])]})
        if path is not None and path.startswith("albums/") and path[7:] in ALBUMS:
            album = ALBUMS[path[7:]]
            tracks = [{"position": index + 1, "title": f"Track {index + 1}",
                       "artist": album["artist"], "duration": 60 + index}
                      for index in range(album["track_count"])]
            return self.send(200, {**album, "tracks": tracks})
        if path is not None and path.startswith("covers/") and ALBUMS.get(path[7:], {}).get("has_cover"):
            return self.send(200, COVER_JPEG, "image/jpeg")
        if path == "imports":
            with State.lock:
                return self.send(200, {
                    "imports": [view(record, requester) for record in State.imports][:50],
                    "budget": {"requester": budget(requester), "total": budget()},
                })
        self.error(404, "not_found")

    def do_POST(self):
        url = urllib.parse.urlsplit(self.path)
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        requester = body.get("requester", "")
        if not self.authorized(url.path, requester):
            return
        if url.path != "/api/integration/v1/imports":
            return self.error(404, "not_found")
        album_id = body.get("album_id")
        if album_id == LIMITED:
            return self.error(409, "limit_reached", scope="requester", limit=LIMIT,
                              remaining=0, next_free_at=None)
        if album_id not in ALBUMS:
            return self.error(404, "not_found")
        with State.lock:
            record = next((record for record in State.imports if record["album_id"] == album_id), None)
            if record is not None:
                if requester not in record["requesters"]:
                    record["requesters"].append(requester)
                return self.send(200, {"import": view(record, requester)})
            record = {"id": f"{len(State.imports) + 1:016x}", "album_id": album_id,
                      "requested_at": time.time(), "requesters": [requester]}
            State.imports.insert(0, record)
            return self.send(202, {"import": view(record, requester)})
