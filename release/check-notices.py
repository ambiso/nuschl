#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Check a downloadable payload's third-party notices against the scope it ships as."""
from pathlib import Path
import argparse
import runpy

source = Path(__file__).resolve().parent
policy = runpy.run_path(str(source / "prepare-artifacts.py"))
roots = policy["load"](source / "notice-roots.json")
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--scope", choices=sorted(roots), required=True)
parser.add_argument("directory", type=Path)
args = parser.parse_args()
policy["verify_notices"](args.directory, roots, args.scope)
print("third-party notices match their scope")
