# Identity on id.nuschl.eu and light federation

Status: plan for review (2026-11-17). It builds on the identity model, key
custody and key transparency of [e2ee.md](e2ee.md) §3 to §5, the witnesses of
[witness.md](witness.md), "MLS and encrypted history" in
[architecture.md](architecture.md) and "Where secrets live" in
[SECURITY.md](../SECURITY.md). PR #363 gives every identity a recovery key that
the device keeps; PR #273 signs sticker pack versions with the author's identity.

The plan holds the required features and the choices that are hard to change later
(see "Lock-in guards"); everything else is in "Later".

## Decisions (Owner, 2026-11-17)

- **Minimal design.** "lets try to keep the design as minmimal as possible while
  putting in the required features or making sure we don't lock ourselves into a
  wrong design that can't be easily changed later."
- **Migratable identities.** "Ideally I'd want to keep identities fully migratable
  so a disappearing or malicious home-server can't impact much."
- **id.nuschl.eu is the identity service and the passkey rpId**, renamed from
  auth.nuschl.eu (decided 2026-11-13; passkeys are unimplemented). It runs passkey
  ceremonies with PRF, keeps encrypted backups, handles recovery and email, and keeps
  a directory and a key-transparency log with witnesses.
- **Safeguards.** The identity service keeps public data and sealed blobs only; if it
  disappears, existing identities, contacts and conversations keep working.
- **One identity service.** id.nuschl.eu is the only identity service, with one
  username namespace, to avoid ecosystem fragmentation. Self-hosting covers home
  servers.
- **Light federation.** Home servers carry the SFU, voice, direct messages,
  attachments and stickers. The identity service tells where a user's home server
  is and sends all email. A user whom a home server blocks moves to another one.
- **Moving home servers** is a statement signed by the identity ("I now live at
  server Y"), recorded in the identity service's directory and log.
- **Direct conversations live on both participants' home servers.** Either one
  suffices to restore the conversation, also when every device of both users has
  lost its state and only the recovery key remains.
- **Usernames** are globally unique and managed by id.nuschl.eu, with free display
  names.
- **Lookups** go straight to the identity service.
- **Recovery** works with a synced passkey, with the recovery key on a later login
  screen, or by email. An email reset makes a new identity that the identity service
  vouches for: public, delayed, open to objection, and unverified for contacts.
  The delay is 24 hours, or 7 days as the user's opt-in. Email at registration is
  offered and optional. Linux desktops recover with a passkey through a page on
  id.nuschl.eu.
- **Invite tree.** "an invite tree like lobste.rs so spam or sockpuppet accounts can
  be identified more easily (and a later public registration phase which can be more
  heavily moderated)". id.nuschl.eu keeps one tree. Invites are available at once,
  and operator identities get unlimited invites. id.nuschl.eu moderates obvious spam
  and abuse only, by cutting the tree at the abusing account. Later public
  registration: email maybe, no proof of work, no probation for now.

## Roles and data

| Role | Holds |
| --- | --- |
| Identity service (id.nuschl.eu) | Directory and KT log, backups, passkey credentials, email addresses, invite tree |
| Home server (a `nuschld` the user chooses) | The user's account, an inbox for conversation invites, direct conversations and calls, attachments, stickers, the user's sealed server-side keys |
| Channel server (any `nuschld`) | Channels, ACLs and accounts, as in e2ee.md |
| Witness (`nuschl-witness`) | Cosignatures over the identity service's and every server's log |
| Device (crypto worker) | All private keys (e2ee.md §3.5), the recovery key (#363), contacts' cached records |

One `nuschld` can be a home server and a channel server at once. A client connects
to the identity service and to every server that hosts one of its conversations.

**Identity service.** Per identity fingerprint, the directory holds the identity
record of e2ee.md §3.1 (root key, device-signing certificate, device certificates,
revocations), the home statement, the username and display name, an email binding
and reset entries. Its KT log, built from `nuschl-kt` and witnessed as in
witness.md, covers them with the labels `id:<fingerprint>` (reserved in e2ee.md
§5.1), `home:<fingerprint>`, `user:<username>` and `reset:<fingerprint>`. Backups
are the `RecoveryBlob` under the recovery key and one blob per passkey under a key
derived from its PRF output; each holds the root seed. The email address stays with
the service to send codes; the record carries its salted hash, signed by the
device-signing key. The invite tree stays outside the log.

**Home server.** The account is bound by an `AccountStatement` (e2ee.md §3.8). The
server stores ciphertext and delivery metadata only: who talks to whom on it, sizes,
times and IP addresses.

## Flows

### Register

1. On first connect the device creates the identity and its recovery key (#363).
2. The user redeems a registration invite and picks a username at the identity
   service; the device publishes the identity record there.
3. The home server is the server the user registers with. The device binds the
   account and publishes `Home { origin, seq = 1 }`, signed by the device-signing key.
4. Optional on the same screen: a passkey with PRF, which stores a backup blob, and
   an email address confirmed by a code.

### Add a device

Linking as in e2ee.md §3.4; the new device's certificate also goes to the identity
service's record.

### Look up a user

The client asks the identity service by username or fingerprint (from an invite
link, a QR code or a channel server's `acct:` binding), checks the proofs and the
witnesses' cosignatures (e2ee.md §5.4, witness.md), and pins the identity on first
use (e2ee.md §4). To start a conversation it puts a sealed MLS invite into the
contact's inbox on their home server.

### Direct conversations

Both participants' home servers keep the conversation. Requirements:

- Each of the two servers holds the full encrypted history and what a member needs
  to rejoin the group or to start a successor with the same members (e2ee.md §6).
- Every key needed to read the history (history keys, e2ee.md §6.6, and the
  device-sync key of §3.7) is also stored on the user's home server, sealed under a
  key derived from the root seed. The recovery key and the passkey backup open the
  root seed, so either restores the conversations without any device.
- If one server disappears, the other keeps serving, and the participants continue
  there or on a new home server.

Recommended for keeping the copies in step (open question 1):

- MLS messages are small and always go to both home servers. The initiator's server
  orders commits, and both servers deduplicate application messages by epoch, sender
  and generation, so either one holds the full history.
- An attachment is uploaded once, to the recipient's home server, which returns a
  signed receipt. Without a receipt within a short timeout, or when the upload
  stalls, the client also uploads to the sender's home server.
- The other home server fetches the encrypted blob by its content hash from the first,
  in the background. The hash verifies the blob, so this narrow server-to-server
  fetch needs no trust between servers.

This fits MLS: each attachment's key and hash travel inside MLS messages, and the
blobs are opaque ciphertext, so where a blob is stored is independent of the group.

### Move home server

1. The user registers at server Y, binds the account, and a device signs
   `Home { Y, seq + 1 }`; the identity service publishes it. A replayed old
   statement fails on `seq`.
2. Contacts see the move on their next lookup or monitoring pass (e2ee.md §5.4).
3. The user's client copies its conversations to Y from the other participants' home
   servers or from its devices.

Every step works without the old server.

### Recover with a passkey or the recovery key

A synced passkey (for example in a password manager) runs a WebAuthn assertion for
rpId `id.nuschl.eu` with PRF; the PRF output, through HKDF, opens that passkey's
backup blob. The later login screen (#363) does the same with the 56-character
recovery key and the `RecoveryBlob`. With the root seed, the device rotates the
device-signing key, certifies itself, revokes the devices the user marks as lost
(e2ee.md §3.5), and opens its sealed keys on the home server. Identity, verification
and history survive.

Native apps run the ceremony through the platform's passkey API where the app may
claim `id.nuschl.eu` (Apple associated domains, Android Digital Asset Links, the
Windows WebAuthn API), so the root seed stays in the crypto worker. On Linux
desktops and elsewhere, a page on id.nuschl.eu unwraps the root, certifies the new
device over the linking protocol of e2ee.md §3.4 and discards the root; that page's
code is trusted while it runs.

### Recover by email

Email proves control of an address only, so an email reset creates a new identity,
and the identity service vouches for it.

1. A code goes to the bound address; the new device submits a new root with it.
2. The service publishes `reset:<old fingerprint>`: "replaced by <new fingerprint>,
   vouched by id.nuschl.eu (email)", effective after 24 hours, or 7 days if the user
   opted in, and mails the address at start and end.
3. During the delay, the old identity's devices see the entry through self-monitoring
   and raise the blocking alarm of e2ee.md §4. A `ResetObjection` signed by the old
   device-signing key, or by the root opened with the recovery key, cancels the reset
   in the log.
4. After the delay, the username, home statement and email binding move to the new
   identity. Contacts see a key change labelled "reset, vouched by id.nuschl.eu";
   verified contacts drop to unverified.

Signal binds an identity to a phone number and re-registers a new install with an
SMS code: at once without Registration Lock; with Registration Lock the user's PIN,
or with a forgotten PIN a wait of 7 days after the account's last activity. A new
identity key shows contacts "safety number changed", and contacts rely on Signal's
server for the binding until they compare safety numbers. Here the KT entry and the delay make the identity
service's vouching visible and contestable. Self-monitoring runs at least hourly
while a device is online, so the 24-hour delay reaches every device online that day;
users whose devices stay offline for days opt in to 7 days.

The new identity starts with an empty history. Sticker pack versions are signed by
the author's identity and `pack_id` derives from its fingerprint (#273), so only the
old identity adds versions to its packs. The new identity publishes a new pack whose
metadata says "replaces pack X"; clients show that note as unverified and leave
installing it to the user.

## Invite tree

Registration at the identity service starts with an invite, as on lobste.rs. An
invite is a single-use code signed by the inviter's device-signing key over the
code's hash and an expiry; the invitee's first publication signs it too. Each
identity's inviter is recorded, so the identities form one tree.

- **Visibility.** The service's moderators see the tree; each user sees their own
  inviter and invitees.
- **Moderation.** Invites are available at once and unlimited by default.
  id.nuschl.eu acts on obvious spam and abuse only: a moderator cuts the tree at the
  abusing account:
  that account and its subtree lose their usernames and invite rights, and the
  service stops publishing their new records. Signed revocations stay accepted, so
  users keep control of their keys. Moderators can limit one account's invites and
  re-admit individual accounts of a cut subtree.
- **Operators.** An operator identity has unlimited invites and admits whoever the
  operator wants; the community is one subtree, cut at the operator if needed. Each
  home server decides which registered identities it hosts.
- **Interplay.** An email reset keeps the old place in the tree, since the service
  vouches for it. Moving home servers leaves the tree unchanged.

## When a party is malicious or gone

| Party | If it disappears | If it is malicious, it can | Limited by |
| --- | --- | --- | --- |
| Identity service | Existing identities, contacts and conversations keep working from cached records and home servers; new registrations, recoveries, lookups and moves wait for its return or a successor | Deny service; delay updates; see registrations, emails and lookups; vouch for an email reset; serve a hostile browser ceremony page | Signatures on every record; witnesses bound split views and freezes to 24 h; backups need the PRF output or recovery key; resets are logged, delayed and objectable |
| Home server | The other participant's home server keeps each direct conversation; the user moves | Deny service; drop invites; see metadata | End-to-end encryption; moves need the user's signature |
| Channel server | Its channels are gone | As in e2ee.md §2 | As in e2ee.md §2 and §5 |
| Stolen unlocked device | — | Add devices, move the home server, object to a reset | Self-monitoring alarms on the other devices; revocation and a move back |

## Lock-in guards

- Every new record and statement uses the KT value framing of e2ee.md §5.1
  (`uint16 format`), so later formats add without breaking proofs.
- Records, statements and references name their identity service, so a successor
  service or an emergency migration can take over.
- Moderation decisions are signed records kept apart from the tree, so other
  moderators can publish their own later.
- Lookup requests are self-contained and leave the asker anonymous, so a home server
  can later forward them unchanged for privacy.
- Server-side keys derive from the root seed, so every recovery path that opens the
  root restores history.
- Home statements carry a sequence number, so moves are ordered and replays fail.

## Bounds

Proposals: KT values at most 4096 bytes (e2ee.md §5.1); a backup blob at most
16 KiB and 8 passkeys per identity; one move per hour; one bound email address,
8-digit codes valid 15 minutes with 5 attempts, one pending reset per identity;
self-monitoring at least hourly while online; invite codes expire after 7 days; an
attachment receipt within 10 s of the upload's end, and 30 s without progress counts
as a stall. Clients validate every reply
against these limits before allocating.

## Slices

1. Identity service: a `nuschl-id` role in `nuschld` with the directory, `id:` and
   `user:` labels, invites and the tree, witness submission and a NixOS module.
2. Home statements, lookups, inboxes and the move flow.
3. Direct conversations on both home servers, with root-derived server-side keys.
4. Recovery-key login screen (#363), then passkey backup and recovery.
5. Email binding and resets.

Each slice has unit tests and loopback integration tests through `nuschl-testkit`;
the move, restore and reset flows also get a NixOS VM test with two home servers and
an identity service.

## Later

- Lookup privacy through a home-server proxy or batched queries.
- Public registration (email maybe; no proof of work; no probation for now).
- Social recovery: k of n chosen contacts confirm a reset, with the email reset's
  delay and objection.
- Moderation communities: users and servers subscribe to moderators they trust.
- Migration to a successor identity service with a signed `MovedTo` statement.
- Reducing channel servers' `acct:` roster commitment once `id:` records exist.

## Open question (recommended default first)

1. **Keeping both copies of a direct conversation:** MLS messages to both home
   servers; attachments once to the recipient's server with a signed receipt, to the
   sender's server as fallback; the other server fetches blobs by content hash
   ("Direct conversations"). Alternative: the sender's client uploads the second copy
   later at low priority.
