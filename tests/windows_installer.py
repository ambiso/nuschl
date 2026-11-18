#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
r"""The per-user installer in the VM from windows_vm.py.

    windows_installer.py SETUP_EXE RESULT_WINDOWS --output DIR

SETUP_EXE is the .#windows-installer file and RESULT_WINDOWS the .#windows
output it was built from. In the guest the test installs silently into
%LOCALAPPDATA%\Programs\Nuschl, checks the files, the uninstall entry, the
Start menu shortcut and the mumble:// and nuschl:// handlers, starts Nuschl
from the shortcut and from a mumble:// link, uninstalls silently (settings
stay), installs again and uninstalls through the dialog with "Remove settings
and history" ticked (settings go). Existing Nuschl settings in the guest are
moved aside for the run and restored afterwards. tests/windows_update.py
--installer covers an in-place update in an installed folder.
"""
import argparse
import hashlib
import os
import re
import subprocess
import sys
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
VM = [sys.executable, str(HERE / "windows_vm.py")]

PRELUDE = r'''
$dir = "$env:LOCALAPPDATA\Programs\Nuschl"
$uninstall = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Nuschl'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Nuschl.lnk'
$data = @("$env:APPDATA\Nuschl", "$env:LOCALAPPDATA\Nuschl")
$work = "$env:TEMP\nuschl-installer-__RUN__"
$setup = "$work\__SETUP__"
function Wait-Until([scriptblock]$condition, [int]$seconds) {
  $deadline = (Get-Date).AddSeconds($seconds)
  while (-not (& $condition)) {
    if ((Get-Date) -gt $deadline) { return $false }
    Start-Sleep -Milliseconds 500
  }
  $true
}
function Running { Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like "$dir\*" } }
function Desktop { Running | Where-Object { $_.Name -eq 'nuschl.exe' } }
function Stop-Running {
  Running | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
  [void](Wait-Until { @(Running).Count -eq 0 } 30)
}
function Install {
  $process = Start-Process -FilePath $setup -ArgumentList '/S' -PassThru -Wait
  "install exit: $($process.ExitCode)"
}
function Report {
  "folder: " + (Test-Path -LiteralPath $dir)
  "shortcut: " + (Test-Path -LiteralPath $shortcut)
  "uninstall entry: " + (Test-Path $uninstall)
  foreach ($scheme in 'mumble', 'nuschl') { "${scheme} handler: " + (Test-Path "HKCU:\Software\Classes\$scheme") }
  foreach ($folder in $data) { "settings ${folder}: " + (Test-Path -LiteralPath "$folder\installer-test-__RUN__") }
}
'''

PREPARE = r'''
if (Test-Path $uninstall) { 'precondition: Nuschl is already installed'; return }
if (Test-Path -LiteralPath $dir) { "precondition: $dir exists"; return }
foreach ($scheme in 'mumble', 'nuschl') { if (Test-Path "HKCU:\Software\Classes\$scheme") { "precondition: a $scheme handler exists"; return } }
New-Item -ItemType Directory -Path $work | Out-Null
Get-NuschlTestFile -Name '__REMOTE__' -OutFile $setup
foreach ($folder in $data) {
  if (Test-Path -LiteralPath $folder) { Rename-Item -LiteralPath $folder -NewName 'Nuschl.before-__RUN__' }
}
'precondition: ok'
'''

INSTALLED = r'''
Install
$expected = __EXPECTED__
$actual = @{}
Get-ChildItem -LiteralPath $dir -Recurse -File | ForEach-Object {
  $actual[$_.FullName.Substring($dir.Length + 1).Replace('\', '/')] = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
}
$files = $actual.Count -eq $expected.Count + 1 -and $actual.ContainsKey('Uninstall.exe')
foreach ($name in $expected.Keys) { if ($actual[$name] -ne $expected[$name]) { $files = $false } }
"files: $files"
$entry = Get-ItemProperty $uninstall
"entry: " + ($entry.DisplayName -eq 'Nuschl' -and $entry.DisplayVersion -eq '__VERSION__' -and $entry.Publisher -eq 'Nuschl' -and
  $entry.DisplayIcon -eq "$dir\nuschl.exe" -and $entry.InstallLocation -eq $dir -and
  $entry.UninstallString -eq "`"$dir\Uninstall.exe`"" -and $entry.QuietUninstallString -eq "`"$dir\Uninstall.exe`" /S")
foreach ($scheme in 'mumble', 'nuschl') {
  $key = "HKCU:\Software\Classes\$scheme"
  "${scheme}: " + ((Get-ItemProperty $key).'URL Protocol' -eq '' -and
    (Get-ItemProperty "$key\shell\open\command").'(default)' -eq "`"$dir\nuschl.exe`" `"%1`"")
}
"shortcut target: " + ((New-Object -ComObject WScript.Shell).CreateShortcut($shortcut).TargetPath -eq "$dir\nuschl.exe")
'''

START_MENU = r'''
Start-Process -FilePath $shortcut
"started from shortcut: " + (Wait-Until { $desktop = @(Desktop); $desktop.Count -eq 1 -and (Get-Process -Id $desktop[0].ProcessId).MainWindowHandle -ne 0 } 60)
'''

LINK = r'''
Stop-Running
Start-Process 'mumble://127.0.0.1:9/installer-test'
"started from link: " + (Wait-Until { @(Desktop | Where-Object { $_.CommandLine -like '*"mumble://127.0.0.1:9/installer-test"*' }).Count -eq 1 } 60)
'''

SILENT_UNINSTALL = r'''
Stop-Running
foreach ($folder in $data) {
  New-Item -ItemType Directory -Force -Path $folder | Out-Null
  Set-Content -LiteralPath "$folder\installer-test-__RUN__" -Value 'settings'
}
Start-Process -FilePath "$dir\Uninstall.exe" -ArgumentList '/S'
"uninstalled: " + (Wait-Until { -not (Test-Path $uninstall) -and -not (Test-Path -LiteralPath $dir) } 120)
Report
'''

# The uninstaller restarts itself from TEMP, so the test finds its window by
# title and follows the pages by the Next button's label (control ID 1). The
# host clicks: the dialog is centred in the 1280x800 guest.
DIALOG = r'''
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
function Uninstaller { Get-Process | Where-Object { $_.MainWindowTitle -like 'Nuschl Uninstall*' } | Select-Object -First 1 }
function Next {
  $process = Uninstaller
  if ($process) {
    [Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle).FindFirst([Windows.Automation.TreeScope]::Descendants,
      (New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::AutomationIdProperty, '1')))
  }
}
function Shows([string]$label) { Wait-Until { $button = Next; $null -ne $button -and $button.Current.Name -eq $label } 120 }
'''
OPEN = r'''
Start-Process -FilePath "$dir\Uninstall.exe"
"page: " + (Shows 'Uninstall')
'''
DONE = r'''
"page: " + (Shows 'Close')
'''
CLOSED = r'''
"uninstalled: " + (Wait-Until { -not (Test-Path $uninstall) -and -not (Test-Path -LiteralPath $dir) -and $null -eq (Uninstaller) } 60)
Report
'''
REMOVE_BOX = (579, 355)
NEXT_BUTTON = (751, 545)

CLEANUP = r'''
Stop-Running
Get-Process | Where-Object { $_.MainWindowTitle -like 'Nuschl Uninstall*' } | Stop-Process -Force
if ((Test-Path $uninstall) -and (Test-Path -LiteralPath "$dir\Uninstall.exe")) {
  Start-Process -FilePath "$dir\Uninstall.exe" -ArgumentList '/S'
  [void](Wait-Until { -not (Test-Path $uninstall) } 60)
}
foreach ($folder in $data) {
  $saved = Join-Path (Split-Path -Parent $folder) 'Nuschl.before-__RUN__'
  if (Test-Path -LiteralPath $saved) {
    Remove-Item -LiteralPath $folder -Recurse -Force -ErrorAction SilentlyContinue
    Rename-Item -LiteralPath $saved -NewName 'Nuschl'
  } else {
    Remove-Item -LiteralPath "$folder\installer-test-__RUN__" -Force -ErrorAction SilentlyContinue
  }
}
Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
'''


def vm(*arguments, timeout=600):
    return subprocess.run([*VM, *arguments], check=True, capture_output=True, text=True, timeout=timeout).stdout


def expected_files(build):
    """The zip's layout: the programs at the top, the notices in licenses/."""
    files = {path.name: path for path in (build / "bin").iterdir()}
    notices = build / "share/licenses/nuschl"
    files.update({"licenses/" + path.relative_to(notices).as_posix(): path
                  for path in notices.rglob("*") if path.is_file()})
    if "nuschl.exe" not in files or any(not re.fullmatch(r"[A-Za-z0-9_./-]+", name) for name in files):
        raise ValueError("unexpected Windows build layout")
    return {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in files.items()}


def check(output, *lines):
    for line in lines:
        assert re.search(rf"^{re.escape(line)}\r?$", output, re.M), f"expected {line!r}"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("setup", type=Path, help="nuschl-<version>-windows-x86_64-setup.exe")
    parser.add_argument("build", type=Path, help="the .#windows output the installer holds")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if "NUSCHL_WINDOWS_VM_DIR" not in os.environ:
        parser.error("explicit NUSCHL_WINDOWS_VM_DIR for the owned installed guest is required")
    version = re.fullmatch(r"nuschl-([0-9A-Za-z.-]+)-windows-x86_64-setup\.exe", args.setup.name)
    if not version:
        parser.error("the installer keeps its release file name")
    expected = expected_files(args.build)
    args.output.mkdir(parents=True, exist_ok=True)
    run = uuid.uuid4().hex
    remote = f"installer-{run}.exe"
    prelude = PRELUDE.replace("__RUN__", run).replace("__SETUP__", args.setup.name)
    log = (args.output / "guest.log").open("w")

    def guest(script, wait=600):
        output = vm("run", prelude + script.replace("__RUN__", run), "--timeout", str(wait), timeout=wait + 60)
        log.write(output)
        log.flush()
        print(output, end="", flush=True)
        if "ERROR: control job failed:" in output:
            raise RuntimeError("guest script failed; see guest.log")
        return output

    vm("push", str(args.setup), remote)
    check(guest(PREPARE.replace("__REMOTE__", remote)), "precondition: ok")
    try:
        table = "@{" + ";".join(f"'{name}'='{digest}'" for name, digest in expected.items()) + "}"
        output = guest(INSTALLED.replace("__EXPECTED__", table).replace("__VERSION__", version[1]))
        check(output, "install exit: 0", "files: True", "entry: True", "mumble: True", "nuschl: True",
              "shortcut target: True")
        check(guest(START_MENU), "started from shortcut: True")
        time.sleep(5)
        vm("screenshot", str(args.output / "start-menu.png"))
        check(guest(LINK), "started from link: True")
        time.sleep(5)
        vm("screenshot", str(args.output / "link.png"))
        output = guest(SILENT_UNINSTALL)
        check(output, "uninstalled: True", "folder: False", "shortcut: False", "uninstall entry: False",
              "mumble handler: False", "nuschl handler: False")
        assert len(re.findall(r"^settings .*: True\r?$", output, re.M)) == 2, "settings did not stay"
        check(guest("Install"), "install exit: 0")
        check(guest(DIALOG + OPEN), "page: True")
        vm("click", *map(str, REMOVE_BOX))
        time.sleep(1)
        vm("screenshot", str(args.output / "uninstall-dialog.png"))
        vm("click", *map(str, NEXT_BUTTON))
        check(guest(DIALOG + DONE), "page: True")
        vm("click", *map(str, NEXT_BUTTON))
        output = guest(DIALOG + CLOSED)
        check(output, "uninstalled: True", "folder: False", "shortcut: False", "uninstall entry: False",
              "mumble handler: False", "nuschl handler: False")
        assert len(re.findall(r"^settings .*: False\r?$", output, re.M)) == 2, "settings stayed"
    finally:
        guest(CLEANUP, wait=180)
    print("Windows installer: installed, started from the Start menu and a mumble:// link, "
          "uninstalled keeping settings and removing them.")


if __name__ == "__main__":
    main()
