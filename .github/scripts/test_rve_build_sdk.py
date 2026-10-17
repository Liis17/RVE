import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

from check_rve_build_sdk import copy_demo_game, rewrite_sdk_path, run_logged


class LoggedCommandTests(unittest.TestCase):
    def test_decodes_utf8_and_replaces_invalid_bytes_without_cargo(self):
        command = [
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(bytes([0xE2, 0x98, 0x83, 0xFF]))",
        ]

        with contextlib.redirect_stdout(io.StringIO()):
            output = run_logged(command, Path.cwd())

        self.assertEqual(output, "☃�")


class SdkPathRewriteTests(unittest.TestCase):
    def test_rewrites_the_single_sdk_dependency_and_keeps_other_manifest_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "Cargo.toml"
            manifest.write_text(
                '[dependencies]\nrve-sdk = { path = "old/sdk" }\nother = "1"\n',
                encoding="utf-8",
            )

            sdk_path = root / "local SDK" / "rve-sdk"
            rewrite_sdk_path(manifest, sdk_path)

            contents = manifest.read_text(encoding="utf-8")
            self.assertIn(f'path = "{sdk_path.resolve().as_posix()}"', contents)
            self.assertIn('other = "1"', contents)

    def test_rejects_missing_or_duplicate_sdk_dependencies(self):
        with tempfile.TemporaryDirectory() as temporary:
            manifest = Path(temporary) / "Cargo.toml"
            for contents in ["[dependencies]\n", '[dependencies]\nrve-sdk = "1"\nrve-sdk = "2"\n']:
                with self.subTest(contents=contents):
                    manifest.write_text(contents, encoding="utf-8")
                    with self.assertRaises(ValueError):
                        rewrite_sdk_path(manifest, Path(temporary) / "sdk")


class DemoCopyTests(unittest.TestCase):
    def test_copy_excludes_rve_cargo_cache_but_keeps_game_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "project.rveproject").write_text("project", encoding="utf-8")
            (source / ".rve/target").mkdir(parents=True)
            (source / ".rve/target/cache").write_text("cache", encoding="utf-8")

            destination = root / "copy"
            copy_demo_game(source, destination)

            self.assertEqual(
                (destination / "project.rveproject").read_text(encoding="utf-8"),
                "project",
            )
            self.assertFalse((destination / ".rve").exists())


if __name__ == "__main__":
    unittest.main()
