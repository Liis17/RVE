import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / "tools" / "build-cache.sh"
DEP = "dep-1111111111111111"
DEP_BUILD = "dep-3333333333333333"
MEMBER = "engine-core-2222222222222222"


class BuildCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cache = self.root / "cache"
        # Stands in for `cargo tree`: reports the only workspace member.
        shim = self.root / "bin" / "cargo"
        shim.parent.mkdir()
        shim.write_text("#!/bin/sh\necho 'engine-core v0.1.0 (/repo)'\n", newline="\n")
        shim.chmod(0o755)

    def checkout(self, name):
        repo = self.root / name
        (repo / "tools").mkdir(parents=True)
        shutil.copy(SCRIPT, repo / "tools" / "build-cache.sh")
        return repo

    def sync(self, repo, cache=True):
        env = dict(os.environ)
        env["PATH"] = str(self.root / "bin") + os.pathsep + env["PATH"]
        env.pop("RVE_BUILD_CACHE", None)
        if cache:
            env["RVE_BUILD_CACHE"] = self.cache.as_posix()
        subprocess.run(
            [shutil.which("bash"), (repo / "tools" / "build-cache.sh").as_posix()],
            env=env,
            check=True,
            capture_output=True,
        )

    def built_checkout(self):
        repo = self.checkout("built")
        debug = repo / "build" / "debug"
        for path in (
            f".fingerprint/{DEP}/lib-dep",
            f".fingerprint/{DEP_BUILD}/run-build-script",
            f".fingerprint/{MEMBER}/lib-engine_core",
            "deps/libdep-1111111111111111.rlib",
            "deps/libengine_core-2222222222222222.rlib",
            f"build/{DEP_BUILD}/out/generated.rs",
            "incremental/engine_core/data",
        ):
            (debug / path).parent.mkdir(parents=True, exist_ok=True)
            (debug / path).write_text(path)
        return repo

    def test_pushes_only_third_party_units_and_seeds_another_checkout(self):
        built = self.built_checkout()
        self.sync(built)

        cached = self.cache / "debug"
        rlib = "deps/libdep-1111111111111111.rlib"
        self.assertTrue(os.path.samefile(cached / rlib, built / "build" / "debug" / rlib))
        self.assertTrue((cached / ".fingerprint" / DEP / "lib-dep").is_file())
        self.assertTrue((cached / "build" / DEP_BUILD / "out" / "generated.rs").is_file())
        self.assertEqual(os.listdir(cached / "deps"), ["libdep-1111111111111111.rlib"])
        self.assertEqual(sorted(os.listdir(cached / ".fingerprint")), [DEP, DEP_BUILD])
        self.assertFalse((cached / "incremental").exists())

        fresh = self.checkout("fresh")
        self.sync(fresh)

        seeded = fresh / "build" / "debug"
        self.assertTrue(os.path.samefile(seeded / rlib, cached / rlib))
        self.assertEqual(sorted(os.listdir(seeded / ".fingerprint")), [DEP, DEP_BUILD])
        self.assertTrue((seeded / "build" / DEP_BUILD / "out" / "generated.rs").is_file())

    def test_does_nothing_without_cache_directory_setting(self):
        built = self.built_checkout()
        self.sync(built, cache=False)

        self.assertFalse(self.cache.exists())


if __name__ == "__main__":
    unittest.main()
