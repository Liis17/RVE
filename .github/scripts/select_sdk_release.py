"""Select pushes that affect the editor distribution, using Cargo's dependency graph."""

import json
import os
from pathlib import Path
import subprocess

from affected_packages import affected_packages, changed_paths


SDK_PACKAGES = {"rve-editor", "rve-build", "rve-player", "rve-sdk"}
RELEASE_INPUTS = {
    "Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml",
    ".github/workflows/sdk-release.yml",
    ".github/scripts/select_sdk_release.py",
    ".github/scripts/affected_packages.py",
    ".github/scripts/package_sdk_release.py",
}


def needs_sdk_release(metadata, paths):
    if paths is None:
        return True
    root = Path(metadata["workspace_root"])
    package_directories = [
        Path(package["manifest_path"]).parent
        for package in metadata["packages"]
        if package["id"] in metadata["workspace_members"]
    ]
    relevant = []
    for path in paths:
        if path in RELEASE_INPUTS or path.startswith(".cargo/"):
            return True
        if path.endswith(".md"):
            continue
        if path.startswith(("editor/", "engine/", "tools/rve-build/")) or any(
            (root / path).is_relative_to(directory) for directory in package_directories
        ):
            relevant.append(path)
    return bool(SDK_PACKAGES.intersection(affected_packages(metadata, relevant)))


def main():
    root = Path(__file__).resolve().parents[2]
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))
    paths = changed_paths(root, os.environ["GITHUB_EVENT_NAME"], event)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=root / "build", text=True, encoding="utf-8",
    ))
    selected = needs_sdk_release(metadata, paths)
    print(f"SDK release required: {selected}")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        output.write(f"build_sdk={str(selected).lower()}\n")


if __name__ == "__main__":
    main()
