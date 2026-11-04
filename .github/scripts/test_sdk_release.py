from pathlib import Path
import tempfile
import unittest
import zipfile

from package_sdk_release import package_sdk
from select_sdk_release import needs_sdk_release
from test_affected_packages import ROOT, package


class SDKReleaseSelectionTests(unittest.TestCase):
    def setUp(self):
        packages = [
            package("core"),
            package("render", ["core"]),
            package("rve-editor", ["render"], directory="editor"),
            package("rve-player", ["core"]),
            package("rve-build", directory="tools/rve-build"),
            package("rve-sdk"),
            package("sandbox", ["render"]),
            package("rvepak", directory="tools/rvepak"),
        ]
        self.metadata = {
            "workspace_root": str(ROOT), "packages": packages,
            "workspace_members": [entry["id"] for entry in packages],
        }

    def test_distribution_and_transitive_dependency_changes_release(self):
        for path in [
            "editor/src/app.rs", "editor/templates/new_project/Assets/models/cube.fbx",
            "engine/core/src/lib.rs", "engine/render/Cargo.toml",
            "engine/rve-player/src/main.rs", "engine/rve-sdk/src/lib.rs",
            "tools/rve-build/src/sdk.rs",
        ]:
            with self.subTest(path=path):
                self.assertTrue(needs_sdk_release(self.metadata, [path]))

    def test_unrelated_changes_skip_release(self):
        for paths in [
            [], ["README.md"], ["editor/README.md"], ["AGENTS.md"],
            ["Obsidian/RVEVault/Architecture.md"],
            ["examples/demo-game/Assets/scenes/demo.rvescene"],
            ["engine/sandbox/src/main.rs"], ["tools/rvepak/src/main.rs"],
            [".github/workflows/ci.yml"], [".github/scripts/test_sdk_release.py"],
            ["tools/unrelated.py"], [".gitignore"],
        ]:
            with self.subTest(paths=paths):
                self.assertFalse(needs_sdk_release(self.metadata, paths))

    def test_shared_build_and_release_inputs_release(self):
        for path in [
            "Cargo.toml", "Cargo.lock", ".cargo/config.toml", "rust-toolchain.toml",
            ".github/workflows/sdk-release.yml", ".github/scripts/select_sdk_release.py",
            ".github/scripts/affected_packages.py", ".github/scripts/package_sdk_release.py",
        ]:
            with self.subTest(path=path):
                self.assertTrue(needs_sdk_release(self.metadata, [path]))

    def test_unavailable_base_and_mixed_changes_release(self):
        self.assertTrue(needs_sdk_release(self.metadata, None))
        self.assertTrue(needs_sdk_release(self.metadata, ["README.md", "editor/src/app.rs"]))
        self.assertTrue(needs_sdk_release(self.metadata, ["engine/deleted/src/lib.rs"]))


class SDKArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.sdk = self.root / "sdk"
        self.archive = self.root / "sdk-verified.zip"
        for relative in [
            "rve-editor.exe", "rve-build.exe", "rve-player.exe", "rve-player-dev.exe",
            "rve-sdk.toml", "rve-sdk/Cargo.toml", "rve-sdk/src/lib.rs",
            "toolchain/bin/cargo.exe", "toolchain/bin/rustc.exe",
            "toolchain/lib/rustlib/x86_64-pc-windows-gnu/bin/rust-lld.exe",
            "toolchain/lib/rustlib/x86_64-pc-windows-gnu/lib/self-contained/crt2.o",
        ]:
            target = self.sdk / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(relative.encode())

    def test_archive_preserves_layout_toolchain_and_contents(self):
        package_sdk(self.sdk, self.archive)
        with zipfile.ZipFile(self.archive) as archive:
            self.assertIsNone(archive.testzip())
            self.assertEqual(
                set(archive.namelist()),
                {"sdk-verified/" + path.relative_to(self.sdk).as_posix()
                 for path in self.sdk.rglob("*") if path.is_file()},
            )
            self.assertEqual(archive.read("sdk-verified/rve-editor.exe"), b"rve-editor.exe")

    def test_incomplete_sdk_is_not_archived(self):
        (self.sdk / "toolchain/bin/rustc.exe").unlink()
        with self.assertRaisesRegex(RuntimeError, "rustc.exe"):
            package_sdk(self.sdk, self.archive)
        self.assertFalse(self.archive.exists())

    def test_existing_archive_is_not_overwritten(self):
        self.archive.write_bytes(b"existing")
        with self.assertRaises(FileExistsError):
            package_sdk(self.sdk, self.archive)
        self.assertEqual(self.archive.read_bytes(), b"existing")


if __name__ == "__main__":
    unittest.main()
