#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Collect the music bot web page's emitted npm notice texts into licenses/runtime.
No network, parsing of formula/message input, device access or key material.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath


def sha(data):
    return hashlib.sha256(data).hexdigest()


def read(path, limit=4*1024**2):
    if path.is_symlink() or not path.is_file() or not 0 < path.stat().st_size <= limit:
        raise ValueError('missing/linked/oversized web notice input')
    return path.read_bytes()


def relative(value):
    path = PurePosixPath(value)
    if not value or path.is_absolute() or '\\' in value or any(part in ('', '.', '..') for part in value.split('/')):
        raise ValueError('unsafe web notice path')
    return path


def collect_npm(app, lock):
    """Validate emitted owners against the source lock and retain actual text."""
    inventory = read(app/'licenses/npm/BUNDLED-NPM.json')
    report = json.loads(inventory)
    if report.get('format') != 1 or not 1 <= len(report.get('packages', [])) <= 128:
        raise ValueError('missing actual emitted npm owner evidence')
    locked = json.loads(read(lock))['packages']
    payloads, owners = [], set()
    for package in report['packages']:
        identity = (package['name'], package['version'])
        if identity in owners or not package.get('modules') or not package.get('phases'):
            raise ValueError('duplicate/empty npm owner evidence')
        owners.add(identity)
        matches = [item for name,item in locked.items() if
                   (name == 'node_modules/'+identity[0] or name.endswith('/node_modules/'+identity[0])) and
                   item.get('version') == identity[1] and
                   item.get('resolved') == package['source']['url'] and item.get('integrity') == package['source']['integrity']]
        if not matches or not package['files']:
            raise ValueError('npm owner differs from pinned source lock or lacks real notice text')
        for item in package['files']:
            name = relative(item['path'])
            if not str(name).startswith('licenses/npm/'+identity[0]+'/'+identity[1]+'/'):
                raise ValueError('foreign npm notice path')
            raw = read(app/name)
            raw.decode('utf8')
            if sha(raw) != item['sha256']:
                raise ValueError('actual emitted npm notice bytes differ')
            payloads.append((str(name),raw))
    if not any(name == 'svelte' for name,_ in owners) or not any(name == 'vite' for name,_ in owners):
        raise ValueError('real application must account for runtime Svelte and generated Vite helpers')
    return inventory, report, payloads, owners


def write_notices(output, inventory, lock, payloads):
    output.mkdir(parents=True)
    with (output/'NOTICES.txt').open('xb') as target:
        target.write(b'Actual bundled web runtime notices\nComplete upstream notice files are preserved; they can mention more than the rendered helper subset.\n')
        for name, raw in payloads:
            target.write(('\n===== '+name+' =====\n').encode())
            target.write(raw)
            target.write(b'\n')
        if target.tell() > 4 * 1024**2:
            raise ValueError('web notice aggregate exceeds 4MiB')
    (output/'BUNDLED-NPM.json').write_bytes(inventory)
    (output/'PACKAGE-LOCK.json').write_bytes(read(lock))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--lock', type=Path, required=True)
    parser.add_argument('--own-license', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    inventory, report, payloads, owners = collect_npm(args.app, args.lock)
    if report.get('generated_wasm_cargo_roots') != [] or report.get('packaged_wasm_build_roots') != []:
        raise ValueError('musicbot inventory claims unshipped WASM roots')
    own = read(args.own_license)
    own.decode('utf8')
    payloads.insert(0, ('EUPL-1.2', own))
    write_notices(args.output, inventory, args.lock, payloads)
    (args.output/'EUPL-1.2').write_bytes(own)


if __name__ == '__main__':
    main()
