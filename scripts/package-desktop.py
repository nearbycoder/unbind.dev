#!/usr/bin/env python3
"""Package a native desktop executable built for the current host."""

import argparse
import plistlib
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP_ID = "dev.unbind.Unbind"
MINIMUM_MACOS = "13.0"
NOTICES = ROOT / "desktop/THIRD-PARTY-NOTICES.txt"


def macos_package(binary: Path, stage: Path, archive: Path, version: str) -> None:
    app = stage / "Unbind.app"
    contents = app / "Contents"
    executables = contents / "MacOS"
    resources = contents / "Resources"
    executables.mkdir(parents=True)
    resources.mkdir()
    executable = executables / "unbind-desktop"
    shutil.copyfile(binary, executable)
    executable.chmod(0o755)
    shutil.copyfile(ROOT / "assets/icon.icns", resources / "Unbind.icns")
    shutil.copyfile(NOTICES, resources / NOTICES.name)
    with (contents / "Info.plist").open("wb") as info:
        plistlib.dump({
            "CFBundleDevelopmentRegion": "en",
            "CFBundleDisplayName": "Unbind",
            "CFBundleExecutable": "unbind-desktop",
            "CFBundleIconFile": "Unbind.icns",
            "CFBundleIdentifier": APP_ID,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": "Unbind",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleURLTypes": [{
                "CFBundleTypeRole": "Viewer",
                "CFBundleURLName": f"{APP_ID}.Invite",
                "CFBundleURLSchemes": ["unbind"],
            }],
            "CFBundleVersion": version,
            "LSMinimumSystemVersion": MINIMUM_MACOS,
            "NSHighResolutionCapable": True,
        }, info)

    subprocess.run(["plutil", "-lint", contents / "Info.plist"], check=True)
    subprocess.run(["codesign", "--force", "--sign", "-", app], check=True)
    subprocess.run(["codesign", "--verify", "--strict", app], check=True)
    subprocess.run(["ditto", "-c", "-k", "--keepParent", app, archive], check=True)


def linux_package(binary: Path, stage: Path, archive: Path) -> None:
    app = stage / "Unbind"
    app.mkdir()
    executable = app / "unbind-desktop"
    installer = app / "install.sh"
    shutil.copyfile(binary, executable)
    shutil.copyfile(ROOT / "scripts/desktop-install-linux.sh", installer)
    shutil.copyfile(ROOT / "assets/icon.svg", app / "Unbind.svg")
    shutil.copyfile(NOTICES, app / NOTICES.name)
    executable.chmod(0o755)
    installer.chmod(0o755)
    (app / "README.txt").write_text(
        "Run unbind-desktop from this folder to open Unbind.\n"
        "Run ./install.sh to add Unbind and activate invite links for your account.\n"
        "The app needs an X11 or Wayland session and working Vulkan graphics.\n"
        "Ubuntu 24.04 packages: libxkbcommon0 libxkbcommon-x11-0 libwayland-client0\n"
        "libvulkan1 (with a Vulkan driver, for example mesa-vulkan-drivers)\n"
        "and xdg-utils for invite links. See https://unbind.dev/downloads/.\n",
        encoding="utf-8",
    )
    with tarfile.open(archive, "w:gz") as output:
        output.add(app, arcname=app.name)


def windows_package(binary: Path, archive: Path) -> None:
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
        output.write(binary, "Unbind/Unbind.exe")
        output.write(NOTICES, f"Unbind/{NOTICES.name}")
        output.writestr("Unbind/README.txt", "Extract this archive and open Unbind.exe.\r\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("platform", choices=("darwin", "linux", "windows"))
    parser.add_argument("architecture", choices=("amd64", "arm64"))
    parser.add_argument("version")
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*)){2}", args.version):
        parser.error("version must be a stable X.Y.Z release")
    if not args.binary.is_file() or args.binary.stat().st_size == 0:
        parser.error(f"executable is missing or empty: {args.binary}")

    args.output.mkdir(parents=True, exist_ok=True)
    extension = "tar.gz" if args.platform == "linux" else "zip"
    archive = args.output.resolve() / f"unbind-desktop-{args.platform}-{args.architecture}.{extension}"
    with tempfile.TemporaryDirectory() as temporary:
        stage = Path(temporary)
        if args.platform == "darwin":
            macos_package(args.binary, stage, archive, args.version)
        elif args.platform == "linux":
            linux_package(args.binary, stage, archive)
        else:
            windows_package(args.binary, archive)
    print(archive)


if __name__ == "__main__":
    main()
