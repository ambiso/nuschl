#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Write THIRD-PARTY.txt and THIRD-PARTY.json from cargo-about JSON reports.

Each report covers one packaged root and target. Every crate must carry license
text from its package or from an about.toml clarification; a crate that only has
cargo-about's canonical SPDX text fails the build. Proc-macro crates run at build
time only and are left out. Components outside Cargo are directories or files of
notice texts, given as NAME=PATH; JSON files in them are metadata and skipped.
"""
import argparse
import hashlib
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--scope", required=True)
parser.add_argument("--policy", type=Path, required=True)
parser.add_argument("--component", action="append", default=[])
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("reports", type=Path, nargs="+")
args = parser.parse_args()

crates, texts, missing = {}, {}, set()
for path in args.reports:
    report = json.loads(path.read_text())
    for entry in report["crates"]:
        package = entry["package"]
        if any("proc-macro" in target["kind"] for target in package["targets"]):
            continue
        crates[(package["name"], package["version"])] = package["license"] or entry["license"]
    for license in report["licenses"]:
        users = {(u["crate"]["name"], u["crate"]["version"]) for u in license["used_by"]} & crates.keys()
        if not users:
            continue
        if license["source_path"] is None:
            missing |= users
            continue
        texts.setdefault((license["id"], license["text"]), set()).update(users)
missing |= crates.keys() - set().union(*texts.values())
if missing:
    raise SystemExit("no license text in the package; clarify these crates in about.toml:\n" +
                     "\n".join(f"  {name} {version}: {crates[(name, version)]}" for name, version in sorted(missing)))

components = {}
for item in args.component:
    name, _, path = item.partition("=")
    path = Path(path)
    files = sorted(p for p in path.rglob("*") if p.is_file()) if path.is_dir() else [path]
    for file in files:
        if file.suffix != ".json":
            relative = file.relative_to(path).as_posix() if path.is_dir() else file.name
            components.setdefault(name, []).append((relative, file.read_text()))
policy = json.loads(args.policy.read_text())[args.scope]
if set(components) != set(policy["components"]):
    raise SystemExit(f"notice components {sorted(components)} differ from {sorted(policy['components'])}")

args.output.mkdir(parents=True)
with (args.output / "THIRD-PARTY.txt").open("w") as out:
    out.write(f"Nuschl third-party notices ({args.scope})\n\n"
              "Nuschl is licensed under the EUPL-1.2. It includes the following Rust crates\n"
              "and other components under the license texts below.\n\n")
    for (name, version), license in sorted(crates.items()):
        out.write(f"{name} {version}: {license}\n")
    for (license, text), users in sorted(texts.items(), key=lambda item: (item[0][0], sorted(item[1]))):
        out.write(f"\n===== {license}, used by: {', '.join(f'{n} {v}' for n, v in sorted(users))} =====\n\n{text}\n")
    for name, files in sorted(components.items()):
        for relative, text in files:
            out.write(f"\n===== {name}: {relative} =====\n\n{text}\n")
digest = hashlib.sha256((args.output / "THIRD-PARTY.txt").read_bytes()).hexdigest()
(args.output / "THIRD-PARTY.json").write_text(json.dumps({
    "scope": args.scope, "policy": policy, "notices_sha256": digest,
    "crates": [{"name": n, "version": v, "license": crates[(n, v)]} for n, v in sorted(crates)],
}, indent=2) + "\n")
