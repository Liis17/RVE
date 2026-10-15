"""Select changed workspace packages and their transitive dependents for CI."""

import json
import os
from pathlib import Path
import subprocess


def changed_paths(root, event_name, event):
    """None requests a full check when there is no usable comparison base."""
    if event_name == "pull_request":
        pr = event["pull_request"]
        revision_range = f'{pr["base"]["sha"]}...{pr["head"]["sha"]}'
    elif event_name == "push":
        before = event["before"]
        if not before.strip("0"):
            return None
        # A force push can make the previous commit unreachable from the checkout.
        if subprocess.run(
            ["git", "cat-file", "-e", f"{before}^{{commit}}"],
            cwd=root, capture_output=True,
        ).returncode:
            return None
        revision_range = f'{before}..{event["after"]}'
    else:
        return None

    result = subprocess.check_output(
        ["git", "diff", "--name-only", "--no-renames", "-z", revision_range, "--"],
        cwd=root, text=True,
    )
    return [path for path in result.split("\0") if path]


def affected_packages(metadata, paths):
    root = Path(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    packages = {
        package["name"]: package
        for package in metadata["packages"] if package["id"] in members
    }
    if paths is None:
        return sorted(packages)

    directories = {
        Path(package["manifest_path"]).parent: name
        for name, package in packages.items()
    }
    changed = set()
    for path in paths:
        absolute = root / path
        owners = [directory for directory in directories if absolute.is_relative_to(directory)]
        if owners:
            changed.add(directories[max(owners, key=lambda directory: len(directory.parts))])
        elif path.startswith("Obsidian/") or path.endswith(".md") or path == "LICENSE":
            continue
        else:
            # Shared configuration, CI, lockfile, and unknown/deleted package paths.
            return sorted(packages)

    dependents = {name: set() for name in packages}
    for name, package in packages.items():
        # Include normal, dev, build, optional, renamed and target-specific edges.
        for dependency in package["dependencies"]:
            dependency_name = directories.get(Path(dependency.get("path", "")))
            if dependency_name is not None:
                dependents[dependency_name].add(name)

    pending = list(changed)
    while pending:
        for dependent in dependents[pending.pop()] - changed:
            changed.add(dependent)
            pending.append(dependent)
    return sorted(changed)


def main():
    root = Path(__file__).resolve().parents[2]
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    paths = changed_paths(root, os.environ["GITHUB_EVENT_NAME"], event)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=root / "build", text=True,
    ))
    packages = affected_packages(metadata, paths)
    print("Selected packages: " + (", ".join(packages) or "none"))
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"has_changes={str(bool(packages)).lower()}\n")
        output.write("packages=" + " ".join(f"-p {name}" for name in packages) + "\n")


if __name__ == "__main__":
    main()
