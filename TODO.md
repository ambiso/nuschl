# Nuschl roadmap

Open work is tracked in [GitHub issues](https://github.com/nuschl-eu/nuschl/issues). Use the [roadmap and task index](https://github.com/nuschl-eu/nuschl/issues/181) to browse all work by area. Issue status is authoritative; this file is not a separate checklist.

## Tracking issues

- [Deferred: iOS client and Linux-hosted Simulator feasibility](https://github.com/nuschl-eu/nuschl/issues/3)
- [Track measured runtime memory reductions without weakening isolation](https://github.com/nuschl-eu/nuschl/issues/47)
- [Track remaining E2EE feature and historical-membership work](https://github.com/nuschl-eu/nuschl/issues/133)
- [Track reproducible Nix-built Linux distribution artifacts and runtime gates](https://github.com/nuschl-eu/nuschl/issues/148)
- [Provide simple profile-preserving desktop installers](https://github.com/nuschl-eu/nuschl/issues/163)
- [Track portable SDK, browser, terminal and mobile client milestones](https://github.com/nuschl-eu/nuschl/issues/167)
- [Plan the Android client after the initial iOS milestone](https://github.com/nuschl-eu/nuschl/issues/170)
- [Track remaining browser client parity and real SDK coverage](https://github.com/nuschl-eu/nuschl/issues/171)

## Active work areas

- Audio: [Measure and reduce remaining unmute-to-first-speech latency](https://github.com/nuschl-eu/nuschl/issues/72); [Preserve incoming playback across microphone mute/unmute](https://github.com/nuschl-eu/nuschl/issues/73); [Validate the integrated VAD fix across quiet speech, noise and AEC conditions](https://github.com/nuschl-eu/nuschl/issues/76).
- Worker boundaries: [Make worker failure recovery bounded, reaped and role-isolated](https://github.com/nuschl-eu/nuschl/issues/20); [Remove runtime panic paths from credential-bearing supervisors](https://github.com/nuschl-eu/nuschl/issues/19).
- Browser: [Complete the receipt-bound browser content broker and readable recovery](https://github.com/nuschl-eu/nuschl/issues/172); [Implement bounded image selection, paste and sending in the browser](https://github.com/nuschl-eu/nuschl/issues/12).
- Chat: [Implement sender-generated link previews with explicit fetch privacy](https://github.com/nuschl-eu/nuschl/issues/11); [Implement persistent accessible polls with authenticated voting](https://github.com/nuschl-eu/nuschl/issues/26).
- Music bot: [Integrate the musicbot branch with isolated codecs and bounded runtime checks](https://github.com/nuschl-eu/nuschl/issues/22).

## Working with the queue

Record new work in an issue, agree on ownership before concurrent edits, and link any real prerequisites. Keep implementation and validation gaps open until their stated completion conditions are met. A staged branch or a passing unit suite does not establish platform or deployment readiness. Follow [AGENTS.md](AGENTS.md) for implementation boundaries and evidence.

`status:approved` marks approved work: the migrated TODO scopes, the owner's direct requests and proposals the owner accepted. Other agent proposals carry `status:needs-approval` until the owner approves them. The shared account alone does not establish human authorship; 👍 reactions only rank work. Existing explicit holds remain in force.

Use issues and PRs for owner-facing work updates. Assigned agents mark active work `status:in-progress`, then remove that label on handoff and explain any remaining integration or validation. Agent-authored bodies and comments end with the attribution trailer specified in [AGENTS.md](AGENTS.md), so the owner's own posts remain distinguishable.

For eligible work, use the issue's **Labels** menu to choose one of `priority:urgent`, `priority:high`, `priority:medium` or `priority:low`. These [priority labels](https://github.com/nuschl-eu/nuschl/labels?q=priority%3A) determine the queue order; an unset priority counts as medium. Within a priority, the number of 👍 reactions on the original issue post breaks ties. Necessary prerequisites may run first, and explicit priorities in the current conversation take precedence. To browse high-priority work by votes, filter with `label:priority:high sort:reactions-+1-desc`. Priority labels do not grant implementation approval.

Private security reports, credentials, recordings and profiles belong outside Git and public issues. Security findings and even the existence of unpublished reports stay in private vulnerability reports, draft security advisories, CodeQL alerts or the owned private cache. Obtain approval privately; do not create public security approval trackers or publish advisories without explicit owner instruction.

## Historical evidence

The [2026-11-13 TODO snapshot](docs/history/2026-11-13-todo-snapshot.md) preserves the completed history and original scoped evidence. Its checkbox states are historical, not a current backlog. The [source-to-issue map](docs/history/2026-11-13-todo-issue-map.tsv) accounts for all 206 original unchecked entries, including two already-resolved entries, plus the residual work found in prose. Deferred work retains its existing status.

The [musicbot submission's deferred workspace checks](docs/history/2026-11-12-musicbot-deferred-checks.md)
preserve the findings left unfixed at the user's request, with their original
checkpoint and verification limits.
