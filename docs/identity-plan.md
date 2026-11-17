# Identity on id.nuschl.eu and light federation

Status: plan for review (2026-11-17). It builds on the identity model, key
custody and key transparency of [e2ee.md](e2ee.md) §3 to §5, the witnesses of
[witness.md](witness.md), "MLS and encrypted history" in
[architecture.md](architecture.md) and "Where secrets live" in
[SECURITY.md](../SECURITY.md). PR #363 gives every identity a recovery key that
the device keeps; PR #273 signs sticker pack versions with the author's identity.

## Decisions (Owner, 2026-11-17)

- **Migratable identities.** "Ideally I'd want to keep identities fully migratable
  so a disappearing or malicious home-server can't impact much."
- **id.nuschl.eu is the identity service and the passkey rpId.** It replaces
  auth.nuschl.eu, the rpId decided on 2026-11-13; passkeys are still unimplemented,
  so the change is a rename. The service runs passkey ceremonies with PRF, keeps encrypted
  identity backups, handles recovery, keeps a directory, and runs a key-transparency
  log with witnesses.
- **Safeguards** (proposed, accepted by the Owner):
  1. The identity service keeps public data and sealed blobs only: a directory, a KT
     log and backups that only the passkey's PRF output or the recovery key opens. If
     it disappears, existing identities, contacts and conversations keep working;
     new devices, recoveries and lookups stop.
  2. Self-hosters can configure their own identity service (`id.example.org`, same
     protocol); id.nuschl.eu is the default.
  3. *Proposed, awaiting confirmation:* lookups go through the user's home server
     as a proxy, or as batched KT queries, so the identity service sees which
     identities are looked up and leaves the asking user unknown.
- **Light federation.** Home servers carry what costs per user and is easy to run:
  the SFU for streams and calls, voice, direct messages, attachments and stickers.
  The identity service finds a user's home server and sends all email, so it carries
  the mail setup for every home server. A user whom a home server blocks moves to another
  one and is reachable again.
- **Moving home servers** is a statement signed with the user's identity ("I now live
  at server Y"), recorded in the identity service's directory and KT log.
- **Recovery** works with a synced passkey, with the recovery key on a later login
  screen, or by email. An email reset makes a new identity that the identity service
  vouches for; it is public, delayed, open to objection, and contacts see it as
  unverified.

## Roles

| Role | Who runs it | Purpose |
| --- | --- | --- |
| Identity service | id.nuschl.eu by default; any operator with the same protocol | Directory, KT log, encrypted backups, passkey ceremonies, recovery, email |
| Home server | a `nuschld` the user chooses | Inbox, direct messages and calls, attachments, stickers, device sync, SFU |
| Channel server | any `nuschld` where the user has an account | Channels, ACLs and accounts as in e2ee.md §6 and extension.md |
| Witness | `nuschl-witness` (witness.md) | Cosigns the identity service's log and every server's log |
| Device | the user's crypto worker | All private keys (e2ee.md §3.5) and the cached records of contacts |

One `nuschld` can be a user's home server and a channel server for many others.
Servers federate through the identity service's directory and through clients:
a client connects to every server that hosts one of its conversations, and servers
talk only to the identity service.

## Data each role holds

**Identity service**

- *Directory*, per identity fingerprint: the identity record of e2ee.md §3.1 (root
  public key, device-signing certificate, device certificates, revocations), the
  current home statement, an optional handle, an optional email binding, and reset
  entries.
- *KT log* over the directory, built from `nuschl-kt` and witnessed as in witness.md,
  origin `id.nuschl.eu:443/nuschl-kt/v1`. Labels: `id:<fingerprint>` (reserved in
  e2ee.md §5.1), `home:<fingerprint>`, `handle:<name>`, `reset:<fingerprint>`.
- *Backups*: the `RecoveryBlob` sealed under the recovery key, and one blob per
  passkey sealed under a key derived from that passkey's PRF output. A blob holds the
  root seed plus pins and verified contacts under the device-sync key (e2ee.md §3.7).
- *Passkey credentials*: credential ids and public keys for rpId `id.nuschl.eu`.
- *Email*: the address behind each email binding, kept by the service to send codes.
  The record carries a salted hash of the address, signed by the device-signing key.

**Home server**

- The user's account, bound by an `AccountStatement` in its own KT log (e2ee.md §3.8).
- An inbox per user: sealed invites (MLS Welcomes, key packages) from any identity.
- Conversations it hosts: direct messages and calls, as ciphertext through the
  delivery service of e2ee.md §6.3, with the SFU and voice for them.
- Attachments ([files.md](files.md)), sticker packs (#273) and device-sync blobs.
- Metadata: who talks to whom on it, sizes, times and IP addresses.

**Channel server**: unchanged from e2ee.md; its log keeps `acct:`, `name:` and
`reg:` for its own accounts.

**Device**: the keys of e2ee.md §3.5, the recovery key (#363), and per contact the
cached identity record, home statement and the id service's head they were checked at.

## Flows

### Register

1. On first connect the device creates the identity and its recovery key (#363).
2. The server the user registers with becomes the home server; the device binds the
   account with an `AccountStatement`.
3. The device publishes the identity record and `Home { origin, log id, seq = 1 }`,
   signed by the device-signing key, to the identity service.
4. Optional, offered on the same screen: a passkey with PRF at id.nuschl.eu, which
   stores a backup blob; an email address, confirmed by a code; a handle.

### Add a device

Linking stays as in e2ee.md §3.4. The new device's certificate goes to the identity
service's record as well as to every channel server's log. A user without another
device at hand uses passkey recovery below, which adds the device the same way.

### Move home server

1. The user registers at server Y and binds the account there.
2. A device signs `Home { Y, seq + 1 }`; the identity service publishes it. The
   sequence number rises with every move, so a replayed old statement fails.
3. Contacts learn the move on their next lookup or monitoring pass (e2ee.md §5.4
   monitors direct contacts on every connect) and send new invites to Y.
4. For each conversation hosted on the old server X, the user's client starts a
   successor group on Y with the same members and posts "moved to Y" in the old group
   while X still serves it. Devices keep their local history; attachments are uploaded
   again from devices that hold them.

Every step works without X, so a vanished or hostile home server costs the user the
conversations and files it hosted, and keeps identity, contacts and verification.

### Look up a user

1. Input: an invite link or QR code (fingerprint, the identity service's origin when
   it differs from the default, a home hint), a handle, or a fingerprint from a
   channel server's `acct:` binding.
2. The client sends the KT query to its home server, which forwards it to the
   identity service, batched with other users' queries and stripped of the asker.
3. The client checks the lookup proofs and the witnesses' cosignatures itself
   (e2ee.md §5.4, witness.md), then pins the identity on first use (e2ee.md §4).
4. To start a conversation, the client puts a sealed invite into the contact's inbox
   on their home server.

### Recover with a passkey

A synced passkey (for example in a password manager) restores the identity without
the recovery key.

1. On the login screen, "Recover with passkey" runs a WebAuthn assertion for rpId
   `id.nuschl.eu` with the PRF extension.
2. The PRF output, through HKDF, opens that passkey's backup blob.
3. The root rotates the device-signing key, certifies the new device, and revokes the
   devices the user marks as lost, as e2ee.md §3.5 describes for recovery. The
   identity and every contact's verification survive.

Native apps run the ceremony through the platform's passkey API where the app may
claim `id.nuschl.eu` (Apple associated domains, Android Digital Asset Links, the
Windows WebAuthn API), so the root stays in the crypto worker. Elsewhere the browser
opens a page on id.nuschl.eu that unwraps the root, certifies the new device over the
linking protocol of e2ee.md §3.4, and discards the root (e2ee.md §3.1).

### Recover with the recovery key

The later login screen (#363) takes the 56-character key, fetches the `RecoveryBlob`
from the identity service or from an export file (e2ee.md §3.7), and continues as in
step 3 above.

### Recover by email

Email proves control of an address only, so an email reset creates a new
identity, and the identity service vouches for it.

1. "Recover by email" sends a code to the bound address. The new device creates a new
   root and submits it with the code.
2. The identity service publishes `reset:<old fingerprint>`: "replaced by <new
   fingerprint>, vouched by id.nuschl.eu (email)", with an effective time after the
   delay.
3. During the delay, any device of the old identity sees the entry through
   self-monitoring (e2ee.md §5.4) and raises the blocking alarm of §4. A
   `ResetObjection` signed by the old device-signing key, or by the root opened with
   the recovery key, cancels the reset; the cancellation is logged too. The service
   also mails the address at start and end.
4. After the delay, the handle, home statement and email binding move to the new
   identity. Contacts see a key change (e2ee.md §4) labelled "reset, vouched by
   id.nuschl.eu"; verified contacts drop to unverified and verify again.

Signal binds an identity to a phone number. A new install re-registers with an SMS
code, Registration Lock requires the user's PIN for that, and a new identity key
shows contacts "safety number changed"; contacts rely on Signal's server for the
binding until they compare safety numbers. The email reset follows that
pattern, and the KT entry and the delay make the identity service's vouching visible
and contestable.

### Social recovery

Chosen contacts confirm a reset jointly, as an alternative to email. The shape
matches the email reset (public entry, delay, objection) with k of n contact
signatures in place of the code. Open question 6.

## Sticker packs after a reset

Pack versions are signed by the author's identity and the `pack_id` derives from its
fingerprint (#273), so only the old identity adds versions to an old pack. The new
identity publishes a new pack whose metadata says "replaces pack X". Clients show
that note as unverified, and installing the new pack is the user's choice. Passkey and recovery-key recovery keep the identity, so their packs continue.

## When a party is malicious or gone

| Party | If it disappears | If it is malicious, it can | It is limited by |
| --- | --- | --- | --- |
| Identity service | Existing identities, contacts and conversations keep working from cached records. New devices without linking, recoveries, new lookups, handle changes and moves stop until it returns or the user switches services | Deny service; delay or omit updates; see registrations, emails, ceremony IPs and which identities are looked up; vouch for an email reset; serve a hostile ceremony page to browser users | Records, moves and bindings carry the identity's signatures; witnesses bound split views and freezes to 24 h (witness.md); backups need the PRF output or recovery key; resets are logged, delayed and objectable; native ceremonies keep the root on the device |
| Home server | Its hosted conversations and files are gone; the user moves (above) | Deny service; drop invites; see metadata of its conversations | MLS keeps content end-to-end; a move needs the user's signature; contacts follow the signed home statement |
| Channel server | Its channels are gone | As in e2ee.md §2 | As in e2ee.md §2 and §5 |
| Witness | Clients see "Automatic verification unavailable" (e2ee.md §5.9) | Cosign a split view with a colluding log | The witness set; manual verification |
| Stolen unlocked device | — | Add devices, move the home server, object to a reset | Self-monitoring alarms on every other device; revocation and a move back |

## Bounds

All numbers are proposals.

- Identity record values keep the KT value limit of e2ee.md §5.1 (4096 bytes per
  value; certificates come from the directory and match the commitment).
- Backup blob: at most 16 KiB; at most 8 passkeys per identity.
- Home statements: `seq` strictly increasing; one move per hour per identity.
- Email: one bound address; codes of 8 digits valid for 15 minutes with 5 attempts;
  one pending reset per identity; reset delay 7 days.
- Lookups: at most 64 labels per batch; the home server rate-limits per account and
  the identity service per home server.
- Inbox: invites up to the delivery service's frame limit; per-sender and
  per-recipient quotas set by the home server.
- Clients validate every reply against these limits before allocating, as AGENTS.md
  requires for network input.

## Slices

1. **Rename.** Record id.nuschl.eu as the rpId in the docs that name auth.nuschl.eu.
2. **Identity service.** A `nuschl-id` role in `nuschld` with the directory, the
   `id:` label and witness submission, a NixOS module, and the client setting with
   id.nuschl.eu as default.
3. **Home statements.** `Home` signing, the `home:` label, client lookup and
   monitoring, and the move flow with successor groups.
4. **Lookups.** The home-server proxy with batching; invites into home inboxes;
   visiting identities admitted to conversations they hold a Welcome for.
5. **Recovery-key login screen** (#363) against the stored `RecoveryBlob`.
6. **Passkey backup and recovery**: native ceremonies per platform, then the browser
   page.
7. **Email**: binding, mail delivery, `reset:` entries, delay, objection and the
   client's reset UI.
8. **Stickers**: the "replaces pack X" note (#273).
9. **Social recovery**, if the Owner chooses it.

Each slice has unit tests and loopback integration tests through `nuschl-testkit`;
the move and reset flows also get a NixOS VM test with two home servers and an
identity service.

## Open questions (recommended default first)

1. **Hosting a direct conversation:** the initiator's home server hosts it and the
   other side connects there as a visiting identity. Alternative: home servers relay
   to each other, which adds server-to-server trust and protocol.
2. **Reset delay and objection:** 7 days; any device with the device-signing key, or
   the recovery key, can object. Alternative: a shorter delay when the identity was
   inactive for 30 days, like Registration Lock.
3. **Email at registration:** offered and optional. Alternative: required for
   identities registered through id.nuschl.eu.
4. **Handles:** optional, unique per identity service, as `handle:` labels.
   Alternative: handles of the form `name@home-server`, which change with a move.
5. **Lookup privacy:** the home-server proxy with batching first; private
   information retrieval later if needed.
6. **Social recovery:** later, after email; k of n chosen contacts, same delay and
   objection.
7. **Changing identity service:** a signed `MovedTo { id service }` statement in the
   old service's log, and references that carry the new origin. Default: later; first
   slices support one service per identity.
8. **Channel servers' `acct:` roster commitment:** kept until the identity service's
   `id:` record is deployed, then reduced to the root and the statement hash.
9. **Browser ceremony on Linux desktops:** the id.nuschl.eu page as the linking device
   until a desktop passkey API exists for that rpId.
10. **Backup contents:** the root seed plus pins and verified contacts. Alternative:
    the root seed only.
