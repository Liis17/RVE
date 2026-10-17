"""Exercise checkout player reuse and an isolated rve-build SDK bundle."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]


def rewrite_sdk_path(manifest, sdk_path):
    """Point the demo game at a supplied standalone rve-sdk checkout."""
    manifest = Path(manifest)
    sdk_path = Path(sdk_path).resolve().as_posix()
    contents = manifest.read_text()
    replacement = f"rve-sdk = {{ path = {json.dumps(sdk_path)} }}"
    updated, count = re.subn(
        r"^([ \t]*)rve-sdk\s*=.*$",
        lambda match: match.group(1) + replacement,
        contents,
        flags=re.MULTILINE,
    )
    if count != 1:
        raise ValueError(f"expected one rve-sdk dependency in {manifest}, found {count}")
    manifest.write_text(updated)


def copy_demo_game(source, destination):
    """Copy source assets and code without inheriting a local Cargo cache."""
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns(".rve"))


def run_logged(command, cwd):
    result = subprocess.run(
        command,
        cwd=cwd,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n")
    if result.returncode:
        raise RuntimeError(f"command exited with {result.returncode}: {command!r}")
    return result.stdout


def build_game(cargo, build_dir, project, output):
    return run_logged(
        [cargo, "run", "-p", "rve-build", "--locked", "--", "build", str(project), "--output", str(output)],
        build_dir,
    )


def require_contains(output, expected, label):
    if expected not in output:
        raise RuntimeError(f"{label} output did not contain {expected!r}")


def main():
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    root = Path(os.environ.get("GITHUB_WORKSPACE", ROOT)).resolve()
    build_dir = root / "build"
    runner_temp = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir()))
    cargo = os.environ.get("CARGO", "cargo")
    executable_suffix = ".exe" if os.name == "nt" else ""

    with tempfile.TemporaryDirectory(prefix="rve-build-sdk-", dir=runner_temp) as temporary:
        temp_root = Path(temporary)
        game_a = temp_root / "game-a"
        game_b = temp_root / "game-b"
        copy_demo_game(root / "examples/demo-game", game_a)
        copy_demo_game(root / "examples/demo-game", game_b)
        rewrite_sdk_path(game_a / "HeavyScripts/Cargo.toml", root / "engine/rve-sdk")
        rewrite_sdk_path(game_b / "HeavyScripts/Cargo.toml", root / "engine/rve-sdk")

        first_build = build_game(cargo, build_dir, game_a, temp_root / "out-a-first")
        require_contains(first_build, "rve-player:", "first checkout build")
        player = build_dir / "player/release" / f"rve-player{executable_suffix}"
        player_hash = hashlib.sha256(player.read_bytes()).digest()

        scripts = game_a / "HeavyScripts/src/lib.rs"
        with scripts.open("a", encoding="utf-8", newline="") as file:
            file.write("\n// CI SDK cache check.\n")
        second_build = build_game(cargo, build_dir, game_a, temp_root / "out-a-second")
        require_contains(second_build, "rve-player: up to date", "repeat checkout build")
        require_contains(second_build, "Compiling demo-heavy-scripts", "changed game script build")
        if hashlib.sha256(player.read_bytes()).digest() != player_hash:
            raise RuntimeError("rve-player changed after editing only the game script")

        third_build = build_game(cargo, build_dir, game_b, temp_root / "out-b")
        require_contains(third_build, "rve-player: up to date", "second project build")

        sdk_dir = temp_root / "sdk"
        run_logged(
            [cargo, "run", "-p", "rve-build", "--locked", "--", "sdk", "--output", str(sdk_dir)],
            build_dir,
        )
        game_sdk = temp_root / "game-sdk"
        copy_demo_game(root / "examples/demo-game", game_sdk)
        rewrite_sdk_path(game_sdk / "HeavyScripts/Cargo.toml", sdk_dir / "rve-sdk")
        bundled_rve_build = sdk_dir / f"rve-build{executable_suffix}"
        sdk_build = run_logged(
            [str(bundled_rve_build), "build", str(game_sdk), "--output", str(temp_root / "out-sdk")],
            build_dir,
        )
        require_contains(sdk_build, "rve-player: prebuilt (SDK)", "SDK build")
        if "rve-player: rebuilt" in sdk_build or "rve-player: up to date" in sdk_build:
            raise RuntimeError("SDK build reported a checkout player build")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, ValueError) as error:
        print(f"rve-build SDK CI check failed: {error}", file=sys.stderr)
        sys.exit(1)
