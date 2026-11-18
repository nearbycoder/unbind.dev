#!/usr/bin/env python3
"""Build versioned release metadata from the exact shipped executables."""

import hashlib
import json
import re
import sys
import zipfile
from pathlib import Path

ORIGIN = "https://unbind.dev"
ASSETS = (
    "unbind-darwin-amd64",
    "unbind-darwin-arm64",
    "unbind-linux-amd64",
    "unbind-linux-arm64",
    "unbind-windows-amd64.exe",
    "unbind-windows-arm64.exe",
)
DESCRIPTION = "Share TCP or UDP services over encrypted peer connections"
PUBLISHER = "nearbycoder"
PACKAGE_ID = f"{PUBLISHER}.Unbind"
WINGET_SCHEMA = "1.28.0"


def write_text(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8", newline="\n")


def homebrew_formula(version: str, base: str, assets: dict) -> str:
    lines = [
        "class Unbind < Formula",
        f'  desc "{DESCRIPTION}"',
        f'  homepage "{ORIGIN}"',
        f'  version "{version}"',
    ]
    for operating_system, target in (("macos", "darwin"), ("linux", "linux")):
        lines += ["", f"  on_{operating_system} do"]
        for architecture, suffix in (("arm", "arm64"), ("intel", "amd64")):
            name = f"unbind-{target}-{suffix}"
            lines += [
                f"    on_{architecture} do",
                f'      url "{base}/{name}"',
                f'      sha256 "{assets[name]["sha256"]}"',
                "    end",
            ]
        lines.append("  end")
    lines += [
        "",
        "  def install",
        '    bin.install Dir["*unbind-*"].fetch(0) => "unbind"',
        "  end",
        "",
        "  test do",
        '    assert_match version.to_s, shell_output("#{bin}/unbind --version")',
        "  end",
        "end",
        "",
    ]
    return "\n".join(lines)


def scoop_manifest(version: str, base: str, assets: dict) -> dict:
    architecture = {}
    for key, suffix in (("64bit", "amd64"), ("arm64", "arm64")):
        name = f"unbind-windows-{suffix}.exe"
        architecture[key] = {
            "url": f"{base}/{name}#/unbind.exe",
            "hash": assets[name]["sha256"],
        }
    return {
        "version": version,
        "description": DESCRIPTION,
        "homepage": ORIGIN,
        "architecture": architecture,
        "bin": "unbind.exe",
    }


def winget_manifests(version: str, base: str, assets: dict) -> dict[str, str]:
    common = f"PackageIdentifier: {PACKAGE_ID}\nPackageVersion: '{version}'\n"
    schema = f"ManifestVersion: {WINGET_SCHEMA}\n"
    schema_uri = "https://aka.ms/winget-manifest.{kind}." + WINGET_SCHEMA + ".schema.json"
    version_document = common + "DefaultLocale: en-US\nManifestType: version\n" + schema
    locale_document = (
        common
        + "PackageLocale: en-US\n"
        + f"Publisher: {PUBLISHER}\n"
        + f"PublisherUrl: {ORIGIN}\n"
        + "PackageName: Unbind\n"
        + f"PackageUrl: {ORIGIN}\n"
        + "License: Unspecified\n"
        + f"ShortDescription: {DESCRIPTION}\n"
        + "ManifestType: defaultLocale\n"
        + schema
    )
    install_document = common + "InstallerType: portable\nCommands:\n- unbind\nInstallers:\n"
    for architecture, suffix in (("x64", "amd64"), ("arm64", "arm64")):
        name = f"unbind-windows-{suffix}.exe"
        install_document += (
            f"- Architecture: {architecture}\n"
            f"  InstallerUrl: {base}/{name}\n"
            f"  InstallerSha256: {assets[name]['sha256'].upper()}\n"
        )
    install_document += "ManifestType: installer\n" + schema
    documents = {
        f"{PACKAGE_ID}.yaml": ("version", version_document),
        f"{PACKAGE_ID}.locale.en-US.yaml": ("defaultLocale", locale_document),
        f"{PACKAGE_ID}.installer.yaml": ("installer", install_document),
    }
    return {
        name: f'# yaml-language-server: $schema={schema_uri.format(kind=kind)}\n\n{document}'
        for name, (kind, document) in documents.items()
    }


def main(version: str, output: Path) -> None:
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*)){2}", version):
        raise SystemExit("Expected a stable release version such as 0.2.0")

    base = f"{ORIGIN}/downloads/releases/{version}"
    assets = {}
    for name in ASSETS:
        path = output / name
        size = path.stat().st_size
        if not 1 <= size <= 128 * 1024 * 1024:
            raise SystemExit(f"{name} exceeds the updater's supported size range")
        with path.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        assets[name] = {"sha256": digest, "size": size}

    manifest = {"schema": 1, "version": version, "assets": assets}
    write_text(output / "release.json", json.dumps(manifest, indent=2) + "\n")
    write_text(output / "SHA256SUMS", "".join(f"{entry['sha256']}  {name}\n" for name, entry in assets.items()))
    write_text(output / "unbind.rb", homebrew_formula(version, base, assets))
    write_text(output / "unbind.json", json.dumps(scoop_manifest(version, base, assets), indent=2) + "\n")
    winget = winget_manifests(version, base, assets)
    with zipfile.ZipFile(output / "unbind-winget.zip", "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for filename, manifest in winget.items():
            archive.writestr(filename, manifest)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("Usage: package-release.py VERSION DIST_DIRECTORY")
    main(sys.argv[1], Path(sys.argv[2]))
