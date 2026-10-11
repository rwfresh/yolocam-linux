#!/usr/bin/env python3
"""Regenerate redistribution notices from Cargo's downloaded dependency sources.

This is a packaging helper; the desktop app and CLI have no Python dependency.
License texts are copied intact and identical texts are grouped by SHA-256.
"""
import hashlib
import json
from pathlib import Path
import subprocess

project = Path(__file__).resolve().parent.parent
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], cwd=project))
groups = {}
packages = []
missing = []
supplementary = {}
license_directory = project / "packaging" / "licenses"
manifest = json.loads((license_directory / "manifest.json").read_text())
for notice in manifest["notices"]:
    path = license_directory / notice["file"]
    if hashlib.sha256(path.read_bytes()).hexdigest() != notice["sha256"]:
        raise ValueError(f"Supplementary license has changed: {notice['file']}")
    for identifier in notice["packages"]:
        supplementary.setdefault(identifier, []).append(path)
for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
    if package.get("source") is None:
        continue
    identifier = f'{package["name"]} {package["version"]}'
    packages.append((identifier, package.get("license") or "See package license"))
    directory = Path(package["manifest_path"]).parent
    candidates = [path for path in directory.rglob("*")
                  if path.is_file() and len(path.relative_to(directory).parts) <= 3
                  and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT"))
                  and path.stat().st_size < 500_000]
    if package.get("license_file"):
        candidate = directory / package["license_file"]
        if candidate.is_file():
            candidates.append(candidate)
    extras = supplementary.get(identifier, [])
    if not candidates and not extras:
        missing.append(identifier)
    labeled = [(path, str(path.relative_to(directory))) for path in sorted(set(candidates))]
    labeled.extend((path, str(path.relative_to(project))) for path in extras)
    for path, label in labeled:
        try:
            content = path.read_text(encoding="utf-8")
        except UnicodeError:
            continue
        digest = hashlib.sha256(content.encode()).hexdigest()
        group = groups.setdefault(digest, {"text": content, "packages": set()})
        group["packages"].add(f"{identifier} / {label}")

lines = ["# Third-party notices", "",
         "Rust dependencies retain their own copyrights and licenses. This file includes",
         "the license and notice files supplied in the locked dependency sources and",
         "supplementary pinned upstream notices from packaging/licenses, including",
         "build and platform-specific dependencies. Identical texts are grouped below.", "",
         "| Package | Declared license |", "|---|---|"]
lines.extend(f"| {name} | {license_name.replace('|', '/')} |" for name, license_name in packages)
if missing:
    lines.extend(["", "Some source packages declare their license in Cargo metadata without supplying",
                  "a standalone license text in their crate archive:", ""])
    lines.extend(f"- {name}" for name in missing)
for index, group in enumerate(groups.values(), 1):
    lines.extend(["", f"## Notice {index}", ""])
    lines.extend(f"- {name}" for name in sorted(group["packages"]))
    lines.extend(["", "```text", group["text"].rstrip(), "```"])
(project / "THIRD_PARTY_NOTICES.md").write_text("\n".join(lines) + "\n")
print(f"Included {len(packages)} packages and {len(groups)} distinct license/notice texts")
if missing:
    print("License text absent from crate archive: " + ", ".join(missing))
