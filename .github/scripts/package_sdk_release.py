"""Archive a Windows SDK and verify the extracted distribution without system Rust."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile


def package_sdk(sdk, archive):
    for relative in [
        "rve-editor.exe", "rve-build.exe", "rve-player.exe", "rve-player-dev.exe",
        "rve-sdk.toml", "rve-sdk/Cargo.toml", "rve-sdk/src/lib.rs",
        "toolchain/bin/cargo.exe", "toolchain/bin/rustc.exe",
    ]:
        if not (sdk / relative).is_file():
            raise RuntimeError(f"SDK file missing: {relative}")
    with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as output:
        for path in sorted(sdk.rglob("*")):
            if path.is_file():
                output.write(path, Path("sdk-verified") / path.relative_to(sdk))


def smoke_test(archive, template):
    system_root = Path(os.environ["SystemRoot"])
    with tempfile.TemporaryDirectory(prefix="RVE SDK release ") as temporary:
        root = Path(temporary)
        with zipfile.ZipFile(archive) as source:
            source.extractall(root)
        project = root / "Starter Project"
        shutil.copytree(template, project)
        environment = {
            key: os.environ[key]
            for key in ["SystemRoot", "WINDIR", "TEMP", "TMP", "USERPROFILE"]
            if key in os.environ
        }
        environment["PATH"] = os.pathsep.join([str(system_root / "System32"), str(system_root)])
        builder = root / "sdk-verified/rve-build.exe"
        for arguments in [
            ["scripts", str(project)],
            ["build", str(project), "--output", str(root / "Player")],
        ]:
            subprocess.run([str(builder), *arguments], cwd=root, env=environment, check=True)
        if not (root / "Player/New Project.exe").is_file():
            raise RuntimeError("SDK did not produce the starter player")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sdk", type=Path)
    parser.add_argument("archive", type=Path)
    args = parser.parse_args()
    package_sdk(args.sdk.resolve(), args.archive.resolve())
    template = Path(__file__).resolve().parents[2] / "editor/templates/new_project"
    smoke_test(args.archive.resolve(), template)


if __name__ == "__main__":
    main()
