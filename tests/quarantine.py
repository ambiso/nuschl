"""Known-flaky test steps, listed with their issues in quarantine.txt.

A listed step still runs. Its result is printed as "QUARANTINED PASS" or
"QUARANTINED FAIL (#issue): step: error" with the traceback, and a failure
does not stop the test. A step's `measured` value (a duration or count) is
printed in both lines; `stolen()` adds the share of CPU time the hypervisor
took meanwhile. A step that is not listed fails as usual.
"""

import contextlib
import os
from pathlib import Path
import sys
import time
import traceback

LIST = Path(__file__).with_name("quarantine.txt")


def issue(name):
    """The issue a quarantined step is tracked in, or None."""
    for line in LIST.read_text().splitlines():
        fields = line.split()
        if fields and not line.startswith("#") and fields[0] == name:
            return fields[1]
    return None


def cpu_counters():
    """Now, the CPU seconds the hypervisor took from this machine, and the seconds tasks waited for a CPU."""
    steal = int(Path("/proc/stat").read_text().split()[8]) / os.sysconf("SC_CLK_TCK")
    waited = int(Path("/proc/pressure/cpu").read_text().split()[4].removeprefix("total=")) / 1e6
    return time.monotonic(), steal, waited


def stolen(before):
    """The share of this machine's CPU time the hypervisor took since `before` (cpu_counters())."""
    now, steal, _ = cpu_counters()
    return f"{(steal - before[1]) / (now - before[0]) / os.cpu_count():.0%} stolen"


@contextlib.contextmanager
def step(name, evidence=None, measured=None):
    """Run a step; after a quarantined failure, `evidence()` (e.g. a screenshot) is kept and named."""
    tracked = issue(name)
    label = name if measured is None else f"{name} [measured: {measured}]"
    if tracked is None:
        yield
        return
    try:
        yield
    except Exception as error:
        report = f"QUARANTINED FAIL ({tracked}): {label}: {type(error).__name__}: {error}\n{traceback.format_exc()}"
        if evidence:
            try:
                report += f"evidence: {evidence()}\n"
            except Exception as failure:
                report += f"no evidence: {failure!r}\n"
        print(report, file=sys.stderr, flush=True)
    else:
        print(f"QUARANTINED PASS ({tracked}): {label}", file=sys.stderr, flush=True)
