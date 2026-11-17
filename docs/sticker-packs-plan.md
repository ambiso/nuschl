# Sticker packs: content-hashed packs, one copy per device and per server

Planning document, 2026-11-17. Nothing here is implemented. Part of #253; it
grants no approval. The importer follows the clean-room sticker interface
spec, kept outside the repository and cited as "the spec".

## 1. Decisions

- **Owner 2026-11-17:** a sticker message carries only a reference:
  `pack_id`, `pack_hash` and `sticker_id`; deduplication is global.
  `pack_hash` is an infohash-style content hash over the pack and its
  stickers, and recipients verify it. It is the integrity primitive: a key
  holder can write new ciphertext, and new ciphertext has a new hash.
- **Owner 2026-11-17:** `pack_id` may be random; content addressing is also
  acceptable (D2).
- **Owner 2026-11-17:** a pack is visible to whoever sees one of its stickers
  or holds a link to it.
- **Owner 2026-11-14:** the reference also carries the pack key ("pack id +
  key + sticker id"), so servers store ciphertext only.
- **Owner 2026-11-14, deduplication:** each device keeps one copy of each
  sticker image, whatever conversation or server it came from. Each server
  keeps one encrypted copy of each pack, and every conversation on that server
  references it. This keeps long-term history, retrievable by members who join
  later, at one copy per pack per server, and it replaces the
  conversation-scoped copies of the 2026-11-13 note.
- **Owner 2026-11-13/14, hosting:** clients choose which packs they have, and
  the first sticker sent from a pack on a server uploads that pack there.
  Showing one sticker fetches only that sticker.
- **Owner 2026-11-13/14, pack flow:** tapping a sticker shows the whole pack
  with an Install button, as in §9 of the spec. Packs spread through the
  stickers people send.
- **Owner 2026-11-13/14, import:** the desktop client imports packs from
  external pack links. The external service's trademark appears nowhere, and
  the importer presents itself as an independent Nuschl feature. Before any
  request to the external service, the client checks whether that exact pack
  was imported before.
- **Owner 2026-11-13/14 and 2026-11-17, rights:** one general Terms of
  Service, accepted once, grant the needed licence and require users to keep
  the service out of risk (§5).
- **Owner, moderation:** no deniability. Stickers are reportable and
  moderatable through message franking (#274).
- **Owner, crypto:** a modern AEAD. Packs use ChaCha20-Poly1305 from ring, as
  files/1 does, so the format adds no crate.
- **Open for the owner:** who fetches external packs, the server or the client
  (D1).

## 2. Packs

### 2.1 Objects, index and hash

A pack is a list of objects: object 0 is the manifest, object `1 + j` is file
`j`. Files `0..N-1` are the stickers (`1 <= N <= 200`), and file `N` is an
optional cover. The index lists the objects' hashes and is public:

```proto
message PackIndex {
  uint32 version = 1;          // 1
  uint32 format = 2;           // 1 = Nuschl, 2 = external (§2.3)
  bytes pack_id = 3;           // 16 bytes
  bytes key_commit = 4;        // SHA-256("nuschl stickers/1 key" || pack_key)
  repeated bytes objects = 5;  // SHA-256 of each blob, object 0 first
}
```

```
pack_hash = SHA-256("nuschl stickers/1 pack" || index bytes as stored)
```

- `pack_hash` fixes `pack_id`, the key commitment and every blob. The message
  fixes `pack_hash` and the key. So `(pack_hash, sticker_id)` names one image
  for every recipient, every server and every moderator; a recipient whose key
  misses `key_commit` refuses the sticker before decrypting.
- Servers verify uploads with the index alone and hold no key.
- Packs are immutable. Editing a pack makes a new pack under a new key; a
  Nuschl pack keeps its `pack_id` across edits (D2).

### 2.2 Nuschl format

```
k_i    = HKDF-SHA256(IKM = pack_key[32], salt = "nuschl stickers/1",
                     info = u16 i, 32)
inner  = u32 len || bytes || zeros to Padmé(4 + len)        (as files/1)
blob_i = "NST1" || nonce[12] || ChaCha20-Poly1305(k_i, nonce, inner,
                                                   aad = "NST1" || u16 i)
```

The manifest:

```proto
message PackManifest {
  uint32 version = 1;             // 1
  string title = 2;               // <= 64 graphemes
  string author = 3;              // <= 64 graphemes
  repeated Sticker stickers = 4;  // display order; entry j is file j
  bool has_cover = 5;             // file N is the cover
}
message Sticker {
  string emoji = 1;               // one emoji of #250's table
  string alt = 2;                 // optional, <= 120 bytes
}
```

Image kind, size and frames come from the content worker (§4.4). The crate
`nuschl-stickers` (no I/O) holds both formats, the index and the link parser;
HKDF, ChaCha20-Poly1305 and SHA-256 come from ring.

### 2.3 External format

An imported pack keeps the external blobs byte for byte: `pack_id` is the
external pack id, `pack_key` the external key, and the objects are the
external manifest and files as the external CDN serves them. Clients decrypt
them as the spec's §3 describes (HKDF and HMAC from ring, AES-256-CBC from
RustCrypto `aes` and `cbc`, already in the lockfile). Every importer of one
external pack computes the same `pack_hash`, so each server stores it once,
whoever imported it.

## 3. Sending, showing and installing

### 3.1 The reference

```proto
message StickerRef {
  bytes pack_id = 1;      // 16 bytes
  bytes pack_hash = 2;    // 32 bytes
  uint32 sticker_id = 3;  // file index, < N
  bytes pack_key = 4;     // 32 bytes
  string emoji = 5;       // one emoji of #250's table
  string alt = 6;         // optional, <= 120 bytes
}
```

A sticker message is one `StickerRef` with no text and no attachment.
Receivers validate every field as the spec's §4.3 does.

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Mumble |
| --- | --- | --- | --- |
| Image | the server's pack store | the same | the inline legacy image |
| Carrier | a `sticker` field in `ChatSend` | `sticker` in kind 13's `ChatBody` (#216), group feature `stickers/1` | `nuschl/sticker/1` over message 26 (#93) |
| Legacy text | the sender's inline image, for stock clients | none | the visible `TextMessage` |
| Pack | the server's pack store | the same | packs already on the device (D6) |

On stock servers the visible message is the sticker's first frame as an
`<img>` from the content worker's `EncodeImage`, within the server's limit,
then the emoji, linked to the annotation by `msg_id` and text digest (#216
§2.4).

### 3.2 Upload on first use

On a server with capability `stickers/1`:

1. `StickerHave {pack_hash}` returns which objects the server holds, or that
   the pack is blocked. A blocked pack stops the send.
2. The client uploads the index and the sent sticker's blob with `StickerPut`
   (content-addressed, write-once), with the terms record (§5.1). The message
   follows once both are stored, so every receiver can open it.
3. The rest of the pack uploads in the background. Blobs the server already
   holds, for example in its external cache (§4.2), are adopted by hash.

Pack objects travel on files/1's second TLS connection (docs/files.md), also
through the web relay, and live in the server database, one row per blob.
The server checks each blob's hash against an index it holds, its size and
its header shape (`NST1`, or the spec's §3.2 length rule).

### 3.3 Receiving and installing

1. The client keeps images by SHA-256 of the image (`digest`) and a map from
   `(pack_hash, sticker_id)` to `digest`. A mapped sticker shows at once.
2. Otherwise it fetches the index by `pack_hash` from the server the message
   arrived on, checks `pack_hash`, `pack_id` and `key_commit`, fetches object
   `1 + sticker_id`, checks its hash, decrypts it, has the content worker
   check it, stores it under its digest and maps it.
3. Tapping the sticker fetches and checks the manifest and shows the pack:
   title, author and every sticker, fetched as they scroll into view.
4. "Install" fetches the remaining objects, verifies each, and adds the pack
   to the library. A pack still uploading shows its present stickers; a
   removed one shows "Pack unavailable on this server".

The content worker (on the web, the content WASM worker) decodes every image,
shown at 160 logical px; animation plays while visible, holds its first frame
under reduced motion and plays on hover or focus. The accessible name is
"Sticker: <alt text, or the emoji's name>"; copying copies the emoji.

### 3.4 Visibility

The server serves an index or blob to an authenticated user who names its
hash, and it offers no listing or search. A `pack_hash` appears only in
sticker messages, and the key only in sticker messages and external pack
links, so whoever sees a sticker or holds a link can view and install the
pack. The operator learns pack hashes and ids, blob sizes, who uploaded each
pack, and who fetched which object and when. Fetch timing can link the users
of one E2EE conversation that use the same pack; Oblivious HTTP (#285) can
hide it. A `history/1` server reads the messages, keys included.

### 3.5 Library and local store

The desktop keeps a `sticker_packs` table in the storage worker (settings.json
cannot gain keys, #250) and images under `stickers/<digest>` in the profile;
the web client uses IndexedDB. Stickers received outside the library share
that store, bounded to 256 MiB, least recently used first. Packs created from
image files pass the content worker checks (§4.4) and a review.

## 4. Importer for external pack links

### 4.1 Flow

1. "Add pack" takes a pasted link in either form of the spec's §8 and
   validates both values; the client registers no URL scheme handler. The
   help line reads "Nuschl is independent of the apps pack links come from."
2. A pack in the library with that external id and key commitment ends the
   import with zero requests.
3. Otherwise the client gets the external manifest and files (D1), decrypts
   them, checks each MAC, parses the manifest (§4.4), has the content worker
   check each image and shows a review. Missing emoji are chosen in #250's
   picker and kept in the library entry; refs carry them.
4. The client builds the index (format 2) and adds the pack to the library.

### 4.2 Who fetches (D1, open)

**Server fetch and cache (recommended).** The client asks its Nuschl server
with `StickerExternal {ext_pack_id, file}`. nuschld answers from a cache keyed
by external id and file, or fetches the object once, with one request in
flight per object however many users ask, under §4.3's rules. The server
holds no key: it checks the blob's size and length rule, and clients check
the MAC and the content. Cached blobs that an uploaded index references are
adopted into the pack store (§3.2); the rest expire 30 days after their last
request (D4), and a 403 or 404 from the source removes them.

- *Load:* one fetch per object per server; the external service sees each
  Nuschl server as one client with a `Nuschl/<version> (+<operator contact>)`
  User-Agent, and can rate-limit or block it.
- *Privacy:* users' IP addresses stay with their Nuschl server.
- *Integrity:* only the server's own fetches fill the cache.
- *Legal:* the server keeps copies, encrypted and unreadable to it, of
  third-party images. Keeping them to serve later identical requests matches
  the caching service of DSA Art. 5, whose exemption holds while the cache
  leaves content unchanged, follows the source's access conditions and
  removes content promptly once the source removed it or an authority
  ordered removal; the bounded lifetime and removal on 403/404 serve that.
  Copies kept for days reach past the temporary copies of InfoSoc Directive
  Art. 5(1) (UrhG §44a), so this needs the legal check. The external
  service's terms bind its users; whether they allow a third-party server to
  fetch its CDN is unknown.
- *Code:* an outbound HTTPS client to one fixed host in nuschld, bounded by
  §4.4's sizes, placed and confined as SECURITY.md requires; a cache table
  and a queue.

**Client fetch.** Each importing client fetches from the external CDN under
§4.3's rules after the library check, and asks its servers
`StickerFind {pack_id}` for uploaded format-2 indexes first.

- *Load:* one request per file per importing user who misses both checks,
  spread across users' addresses; the service can tell Nuschl apart by
  User-Agent only.
- *Privacy:* the external CDN sees each importer's address and pack id, as
  with the official app.
- *Integrity:* any holder of the link can upload altered blobs with a valid
  MAC under that pack id; only a fetch from the CDN reveals it.
- *Legal:* each user fetches with their own link, as the official app does.
- *Code:* the HTTPS client and bounds move to the desktop; a lookup on the
  server.

Recommendation: server fetch and cache, on by default with an operator
switch; v1 imports need a server with `stickers/1`.

### 4.3 Polite fetching

- Only on a user's import: no background fetching, prefetch or crawling.
- HTTPS to the configured CDN host only, no redirects to other hosts.
- Two requests in flight per fetcher, 100 ms between starts, each distinct
  file once; 15 s connect, 30 s per file, reads capped at 10 KiB and 301 KiB.
- 429 and 503 wait for `Retry-After` (at most 10 minutes); other errors back
  off 1–32 s with jitter, four attempts per file; three failed files in a row
  stop the import. 403 and 404 mean "not found".

The external link host and CDN host (spec §2, §8) are two constants in the
`external_pack` module and its tests, with a switch that turns the importer
off (D5); identifiers, files and UI use neutral names.

### 4.4 Untrusted manifest and images

The external manifest is parsed after its MAC check, within 10 KiB: invalid
UTF-8 becomes U+FFFD, Cc and Cf characters go, and duplicate or out-of-range
indexes reject the pack. Images go to the content worker one at a time
through typed requests, `StickerCheck` (kind, size, frames, duration, a 96 px
preview) and `StickerFrames` (frames at display size), for imports, created
packs and every received sticker. Results are bound to their import or
message, request and generation.

| Bound | Value |
| --- | --- |
| Input | <= 300 KiB; PNG, APNG or WebP by magic bytes; declared MIME ignored |
| Header dimensions | <= 512 x 512, checked before decoding |
| Frames | <= 150 and <= 3 s, decoded one at a time, two resident |
| Time | 2 s wall per image, under the worker's memory limit |
| Reply | frames at <= 160 px, under the 12 MiB IPC frame |
| Desktop cache | 64 MiB of decoded frames, least recently used first |

## 5. Terms of Service

### 5.1 Acceptance and record

Nuschl's Terms of Service are accepted once: in the first-run dialog (beside
#205's feedback choice), and on existing installs the first time the user
sends a sticker or adds a pack (D8). Until then the Stickers tab shows:

> **Nuschl terms**
> Stickers you share must be yours to share. People you send them to can
> use them in Nuschl. [Read the terms]
> [Accept] [Not now]

The text is `terms/nuschl-terms-1.en.txt`, shipped in the client, with id
`nuschl-terms/1` and the SHA-256 of its canonical bytes (UTF-8, NFC, LF). The
client stores the accepted id, hash and time and sends them with its first
pack upload to each server, which records them with the account and its own
time.

### 5.2 The stickers section, version 1

1. **Your confirmation.** By adding or sending stickers you confirm that you
   created them, or that everyone holding rights in them allows you to
   license them on these terms, and that their use under these terms
   infringes no rights and breaks no law.
2. **Keep the service safe.** You use Nuschl in ways that keep its servers
   and operators out of risk: you share only legal content you may share,
   and you keep automated or bulk use from overloading Nuschl or the
   services packs come from.
3. **Servers.** You grant the operator of every Nuschl server a pack or
   sticker is sent on, by you or by people who received it, a simple
   (non-exclusive), worldwide, royalty-free, perpetual and irrevocable right
   to host, store, reproduce, distribute, publicly display and make it
   available through that server, including format conversion, resizing,
   compression, padding and encryption.
4. **Recipients.** Everyone who receives a pack or its stickers gets the same
   kind of right to use, display, install and send them on within Nuschl.
5. **Irrevocable.** You can delete a pack from your library and ask an
   operator to stop hosting it. Copies people received, and their rights,
   stay.
6. **Removal.** Each operator decides what its server hosts and may refuse
   or remove any pack or sticker, for example after a report or a legal
   notice.
7. **Other sources.** Imported packs come from third-party sources. Nuschl
   and its importer are independent of them and carry no endorsement by them.
8. **Moral rights.** You remain the author, with the rights EU and German law
   keep with you. You agree that stickers appear in messages with no author
   line, and that a pack shows the author you entered.
9. **What is recorded.** A server you upload a pack to records your account,
   the terms version and the time, as proof of the licence while it hosts
   your packs and for three years after (GDPR Art. 6(1)(b) and (f)). You can
   ask its operator for a copy.

Best-effort wording by a non-lawyer; it needs a legal check before launch.

## 6. Moderation

- **Report:** a message report under message franking (#274). The franked
  envelope carries the `StickerRef`, so the moderator fetches the index and
  the object by hash, opens it with the pack key and sees the same image as
  every recipient, with the sender and the time.
- **Removal:** deleting the message tombstones it. Removing a pack deletes its
  blobs and blocks its `pack_hash`, and for format 2 also its external id in
  the cache, so re-uploads and re-fetches fail. Every message referencing it,
  in every conversation on that server, shows its emoji and "Sticker
  unavailable".
- **Scope:** one removal covers every conversation on that server; other
  servers keep their copies until their moderators act.
- **Uploaders** get reasons and one appeal (DSA Art. 17, 20); three upheld
  removals in a year suspend uploads there. Anyone can hide a pack locally.

## 7. Storage and limits

Each pack is stored once per server and counts toward its first uploader's
quota. It stays as long as the server keeps history, since any kept message
may reference it; a server with time-limited history removes a pack once its
last upload check or fetch is older than that limit.

| Limit | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Formats | PNG, APNG, WebP still and animated | the content worker's decoders |
| Animation | <= 3 s, <= 150 frames, >= 20 ms per frame | bounded decode |
| Manifest | <= 64 KiB (format 1), <= 10 KiB (format 2) | 200 entries with alt text |
| Index | <= 8 KiB | 202 hashes |
| Pack | <= 64 MiB of blobs | 201 x 301 KiB padded |
| Packs | 1 GiB and 100 packs first uploaded per user, 20 GiB per server | operator cost |
| External cache | 2 GiB per server, 30 days after last request | import dedupe |

`services.nuschl.stickers` in `nix/module.nix` sets `enable` (default true),
`maxPackBytes`, the pack quotas, `externalFetch` and the cache bounds.

## 8. The picker (#250)

The Stickers tab replaces "Stickers are coming soon." with the library: pack
tabs like the emoji category row (Recent, the packs, "+" Add pack: link or
files), cells keyed by grid position with §3.3's names and arrow keys, and
search over each sticker's emoji names, keywords and alt text. Click sends and
closes, Shift+click sends and stays open. Empty state: "No sticker packs yet"
and "Add pack". A received sticker opens its pack on a click, with "Install"
and "Report". The web picker matches; adding from links and files is desktop
only (D7).

## 9. Slices

| # | Slice | Needs | Size |
| --- | --- | --- | --- |
| S1 | `nuschl-stickers`: index, both formats, manifest, links, vectors | — | M |
| S2 | Content worker `StickerCheck` and `StickerFrames`, bounds | — | M |
| S3 | Desktop library, digest store, picker tab, create from files, terms | S1, S2 | M |
| S4 | nuschld pack store, first-use upload, NixOS options | S1 | M |
| S5 | Importer: review in the desktop, external fetch and cache (D1) | S3, S4 | L |
| S6 | Sending and showing stickers on the three paths | S3, S4, #93, #216 | M |
| S7 | Pack view and Install | S6 | M |
| S8 | Reports through #274, pack removal and blocks | S4, #274 | S |
| S9 | Web: library, picker, rendering, sending, pack view | S1, S7 | M |

## 10. Tests

- Unit: vectors for `k_i`, `key_commit`, the index and `pack_hash` in both
  formats; a blob outside the index, in another slot, or under a key that
  misses `key_commit` is refused; the external decryptor refuses a changed
  byte. The content worker refuses a declared 65535 x 65535 PNG, a
  10,000-frame APNG, long WebP, truncated files and MIME mismatches.
- Integration through `nuschl-testkit`: a synthetic external pack on a
  loopback CDN fixture; a library hit makes 0 requests, two importers through
  one server make one request per file, and both compute one `pack_hash`
  that the server stores once.
- VM, one nuschld, two conversations: the pack uploads once and both
  reference it; a receiver with the sticker mapped downloads nothing, one
  lacking it fetches the index and one object; a member who joins later sees
  old stickers; tapping fetches the manifest, Install the rest; wrong hashes
  are refused; a removed pack shows unavailable in both conversations;
  uploads wait for accepted terms and the server records id, hash and time.
- Local only (GUI): desktop smoke with AT-SPI names; stock Mumble shows the
  inline image and Nuschl one sticker.

## 11. Open questions

- **D1. External fetch:** the server fetches and caches (recommended, §4.2) or
  each client fetches.
- **D2. `pack_id` of created packs:** 16 random bytes, kept across edits
  (recommended), or the first 16 bytes of `pack_hash`.
- **D3. Pack links:** v1 accepts external pack links only, matching the
  pack flow decision (recommended). The alternative is a Nuschl link with
  server, `pack_id`, `pack_hash` and key, accepted by Add pack and offered
  as "Copy link" in the pack view.
- **D4. External cache lifetime:** 30 days after the last request, removed on
  403/404 (recommended), or bounded by size only.
- **D5. External hosts:** two constants plus an off switch (recommended), or a
  configuration file.
- **D6. Stock servers:** tapping shows packs already on the device
  (recommended for v1), or a request to the sender through the transfer
  worker.
- **D7. Scope:** adding from links and files on the desktop only in v1
  (recommended).
- **D8. Terms:** at first run and, on existing installs, at first sticker use
  (recommended); binding language; three years' record retention.
- **D9. Server defaults:** 1 GiB and 100 packs per user, 20 GiB per server,
  2 GiB of external cache, on (recommended); nuschl.eu's servers review
  reports in triage.nuschl.eu (#205).
- **D10. External CDN facts** to settle before S5: its certificate chain
  (public or the private CA the spec mentions, then pinned for that host),
  its rate limits and `Retry-After` use, and whether its terms allow
  third-party fetching.
