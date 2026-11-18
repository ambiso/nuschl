#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Validate same-run immutable build evidence before exposing signing secrets.

No network, execution of artifact programs, trust enrollment, or secret access.
Only regular files/directories are accepted; links and special tar members fail.
Missing runtime attribution is a publication blocker, not a legal assurance.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tarfile

ROOTS = {"windows-x86_64": "windows", "macos-universal": "Nuschl.app"}


def unique(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError("duplicate JSON field")
        out[key] = value
    return out


def load(path, limit=16384):
    if not path.is_file() or path.is_symlink() or path.stat().st_size > limit:
        raise ValueError("missing, linked or oversized JSON evidence")
    return json.loads(path.read_bytes(), object_pairs_hook=unique)


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            result.update(block)
    return result.hexdigest()


def checked_relative(value):
    path = PurePosixPath(value)
    if (not value or len(value) > 512 or "\\" in value or ":" in value or path.is_absolute() or
            any(part in {"", ".", ".."} for part in value.split("/"))):
        raise ValueError("unsafe artifact path")
    return path


def verify_notices(root, policy, scope):
    """Check a bundle against the notice policy of the scope its caller expects."""
    if scope not in policy:
        raise ValueError("unknown notice scope")
    report = load(root / "THIRD-PARTY.json", 8 * 1024 * 1024)
    text = root / "THIRD-PARTY.txt"
    if (report.get("scope") != scope or report.get("policy") != policy[scope] or not report.get("crates") or
            not text.is_file() or text.is_symlink() or digest(text) != report.get("notices_sha256")):
        raise ValueError("missing or mismatched third-party notices")


def evidence(directory, platform, *, repository, commit, run_id, attempt, version, filenames):
    info = load(directory / "build-info.json")
    expected = {"repository": repository, "commit": commit, "run_id": run_id,
                "platform": platform, "version": version, "host": "x86_64-linux",
                "os_code_signed": False, "native_platform_runtime_tested_by_this_job": False,
                "macos_ad_hoc_signature": platform == "macos-universal"}
    if any(info.get(key) != value or type(info.get(key)) is not type(value) for key, value in expected.items()):
        raise ValueError("artifact source/run/platform provenance mismatch")
    prior = info.get("run_attempt", "")
    if not isinstance(prior, str) or not re.fullmatch(r"[1-9][0-9]{0,8}", prior) or not 1 <= int(prior) <= attempt:
        raise ValueError("invalid/future artifact run attempt")
    sums_path = directory / "SHA256SUMS"
    if not sums_path.is_file() or sums_path.is_symlink() or sums_path.stat().st_size > 4096:
        raise ValueError("missing bounded artifact checksums")
    sums = {}
    for line in sums_path.read_text().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (" + "|".join(re.escape(name) for name in filenames) + ")", line)
        if not match or match[2] in sums:
            raise ValueError("ambiguous artifact checksums")
        sums[match[2]] = match[1]
    if set(sums) != set(filenames):
        raise ValueError("incomplete artifact checksums")
    for name, sha in sums.items():
        path = directory / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 4 * 1024**3 or digest(path) != sha:
            raise ValueError("artifact digest/size mismatch")
    return info


def prepare(directory, platform, output, *, repository, commit, run_id, attempt, version, policy):
    if platform not in ROOTS:
        raise ValueError("unsupported platform")
    info = evidence(directory, platform, repository=repository, commit=commit, run_id=run_id,
                    attempt=attempt, version=version, filenames=("install.tar", "build-info.json"))
    # Preflight every member before extracting anything. A fixed root and
    # regular-only archive eliminate symlink/hardlink ordering and traversal.
    with tarfile.open(directory / "install.tar", "r:") as archive:
        members, names, total, file_count = [], set(), 0, 0
        for member in archive:
            if len(members) >= 16384:
                raise ValueError("too many archive members")
            relative = checked_relative(member.name.rstrip("/"))
            if relative.parts[0] != ROOTS[platform] or not (member.isfile() or member.isdir()):
                raise ValueError("foreign root, link or special archive member")
            if member.name.lower().rstrip("/") in names or member.size > 512 * 1024**2:
                raise ValueError("duplicate/oversized archive member")
            names.add(member.name.lower().rstrip("/"))
            if member.isfile():
                file_count += 1
                if file_count > 256:
                    raise ValueError("installation exceeds updater's 256-file bound; compact notices without dropping them")
            total += member.size
            if total > 4 * 1024**3:
                raise ValueError("oversized installation")
            members.append(member)
        destination = output / ROOTS[platform]
        if destination.exists():
            raise ValueError("installation destination already exists")
        output.mkdir(parents=True, exist_ok=True)
        archive.extractall(output, members=members, filter="data")
    root = output / ROOTS[platform]
    binaries = root if platform == "windows-x86_64" else root / "Contents/MacOS"
    suffix = ".exe" if platform == "windows-x86_64" else ""
    for name in ("nuschl", "nuschl-update-verify-ed25519", "nuschl-update-verify-mldsa44"):
        if not (binaries / (name + suffix)).is_file():
            raise ValueError("missing application or fixed hybrid verifier")
    notices = root / "licenses" if platform == "windows-x86_64" else root / "Contents/Resources/licenses"
    verify_notices(notices, policy, platform)
    return info


def prepare_linux(directory, output, *, repository, commit, run_id, attempt, version, policy):
    filenames = ("web-site.tar", "nix-paths.json", "INSTALL-NIX.txt", "flake.lock", "build-info.json")
    info = evidence(directory, "linux-nix", repository=repository, commit=commit, run_id=run_id,
                    attempt=attempt, version=version, filenames=filenames)
    if digest(directory / "flake.lock") != digest(Path(__file__).resolve().parents[1] / "flake.lock"):
        raise ValueError("Linux reference artifact has a foreign lock file")
    paths = load(directory / "nix-paths.json", 1024 * 1024)
    if isinstance(paths, dict):
        records = list(paths.items())
    elif isinstance(paths, list) and all(isinstance(item, dict) for item in paths):
        records = [(item.get("path"), item) for item in paths]
    else:
        raise ValueError("invalid Nix evidence container")
    if len(records) != 4:
        raise ValueError("Linux evidence must cover desktop/server/relay/web outputs")
    for path, item in records:
        if (not isinstance(path, str) or not re.fullmatch(r"/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[A-Za-z0-9+._-]+", path) or
                not isinstance(item, dict) or not isinstance(item.get("narHash"), str) or
                not re.fullmatch(r"sha256-[A-Za-z0-9+/]{43}=", item["narHash"])):
            raise ValueError("invalid Nix output evidence")
    destination = output / "web-site"
    if destination.exists():
        raise ValueError("web destination already exists")
    destination.mkdir(parents=True)
    with tarfile.open(directory / "web-site.tar", "r:") as archive:
        members, names, total = [], set(), 0
        for member in archive:
            name = member.name[2:] if member.name.startswith("./") else member.name
            if name in ("", ".") and member.isdir():
                continue
            relative = checked_relative(name.rstrip("/"))
            if (len(members) >= 32768 or not (member.isfile() or member.isdir()) or
                    member.size > 64 * 1024**2 or str(relative).lower() in names):
                raise ValueError("unsafe/duplicate/oversized web archive")
            names.add(str(relative).lower())
            total += member.size
            if total > 512 * 1024**2:
                raise ValueError("oversized static web bundle")
            member.name = str(relative)
            members.append(member)
        archive.extractall(destination, members=members, filter="data")
    if not (destination / "index.html").is_file() or not (destination / "app/index.html").is_file():
        raise ValueError("static web bundle lacks landing/client entry points")
    verify_notices(destination / "licenses/nuschl", policy, "web")
    return info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", action="append", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--notice-roots", type=Path, default=Path(__file__).with_name("notice-roots.json"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit) or not re.fullmatch(r"[1-9][0-9]*", args.run_id) or args.attempt < 1:
        raise ValueError("invalid expected source/run")
    policy = load(args.notice_roots)
    seen = set()
    for item in args.artifact:
        platform, separator, path = item.partition("=")
        if not separator or platform in seen:
            raise ValueError("duplicate artifact")
        seen.add(platform)
        operation = prepare_linux if platform == "linux-nix" else prepare
        extra = {} if platform == "linux-nix" else {"platform": platform}
        operation(Path(path), output=args.output, repository=args.repository, commit=args.commit,
                  run_id=args.run_id, attempt=args.attempt, version=args.version, policy=policy, **extra)
    if seen != set(ROOTS) | {"linux-nix"}:
        raise ValueError("Linux/web, Windows and macOS artifacts are mandatory")
    print("same-source build artifacts, hybrid workers and bounded notice payloads verified")


if __name__ == "__main__":
    main()
