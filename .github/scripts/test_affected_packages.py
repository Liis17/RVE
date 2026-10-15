import copy
from pathlib import Path
import subprocess
import tempfile
import unittest

from affected_packages import affected_packages, changed_paths


ROOT = Path("/workspace")


def package(name, dependencies=(), directory=None, dependency_directories=None):
    package_directory = Path(directory) if directory else Path("engine") / name
    dependency_directories = dependency_directories or {}
    return {
        "id": name,
        "name": name,
        "manifest_path": str(ROOT / package_directory / "Cargo.toml"),
        "dependencies": [
            {
                "name": dependency,
                "path": str(ROOT / dependency_directories.get(
                    dependency, Path("engine") / dependency
                )),
            }
            for dependency in dependencies
        ],
    }


class PackageSelectionTests(unittest.TestCase):
    def setUp(self):
        self.metadata = {
            "workspace_root": str(ROOT),
            "packages": [
                package("audio"), package("core"), package("platform-windows"),
                package("platform", ["platform-windows"]),
                package("render", ["platform"]),
                package("sandbox", ["core", "render"]),
            ],
        }
        self.all_packages = sorted(p["name"] for p in self.metadata["packages"])
        self.metadata["workspace_members"] = self.all_packages

    def select(self, paths):
        return affected_packages(self.metadata, paths)

    def test_independent_package_does_not_select_other_projects(self):
        self.assertEqual(self.select(["engine/audio/src/lib.rs"]), ["audio"])

    def test_dependency_selects_consumers_but_not_unrelated_packages(self):
        self.assertEqual(self.select(["engine/core/src/lib.rs"]), ["core", "sandbox"])

    def test_transitive_dependents_are_selected(self):
        self.assertEqual(self.select(["engine/platform/src/lib.rs"]),
                         ["platform", "render", "sandbox"])

    def test_consumer_does_not_select_dependencies_for_separate_checks(self):
        self.assertEqual(self.select(["engine/sandbox/tests/cli.rs"]), ["sandbox"])

    def test_multiple_changes_are_combined_and_deduplicated(self):
        self.assertEqual(self.select([
            "engine/core/src/lib.rs", "engine/audio/src/lib.rs", "engine/core/Cargo.toml",
        ]), ["audio", "core", "sandbox"])

    def test_package_files_including_docs_and_build_scripts_select_owner(self):
        for filename in ["Cargo.toml", "build.rs", "README.md", "data/input.bin"]:
            with self.subTest(filename=filename):
                self.assertEqual(self.select([f"engine/core/{filename}"]), ["core", "sandbox"])

    def test_non_build_documents_and_empty_diff_skip_rust_checks(self):
        self.assertEqual(self.select(["README.md", "AGENTS.md", "LICENSE",
                                      "Obsidian/RVEVault/.obsidian/app.json"]), [])
        self.assertEqual(self.select([]), [])

    def test_shared_or_unrecognized_files_select_every_package(self):
        for path in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml", "rust-toolchain.toml",
                     ".github/workflows/ci.yml", ".github/scripts/affected_packages.py",
                     "shared/input.bin", "engine/deleted/src/lib.rs", "engine/audio-extra/lib.rs"]:
            with self.subTest(path=path):
                self.assertEqual(self.select([path]), self.all_packages)
        self.assertEqual(self.select(None), self.all_packages)

    def test_all_dependency_kinds_renames_and_targets_are_included(self):
        for kind in [None, "dev", "build"]:
            with self.subTest(kind=kind):
                metadata = copy.deepcopy(self.metadata)
                dependency = metadata["packages"][3]["dependencies"][0]
                dependency.update(kind=kind, rename="native", optional=True,
                                  target='cfg(target_os = "windows")')
                self.assertEqual(affected_packages(metadata, ["engine/platform-windows/src/lib.rs"]),
                                 ["platform", "platform-windows", "render", "sandbox"])

    def test_registry_names_do_not_create_workspace_edges(self):
        self.metadata["packages"][0]["dependencies"].append({"name": "core"})
        self.assertEqual(self.select(["engine/core/src/lib.rs"]), ["core", "sandbox"])

    def test_dependency_cycles_terminate(self):
        self.metadata["packages"][1]["dependencies"] = package("core", ["sandbox"])["dependencies"]
        self.assertEqual(self.select(["engine/core/src/lib.rs"]), ["core", "sandbox"])

    def test_nested_workspace_member_owns_its_files(self):
        nested = package("nested")
        nested["manifest_path"] = str(ROOT / "engine/audio/nested/Cargo.toml")
        self.metadata["packages"].append(nested)
        self.metadata["workspace_members"].append("nested")
        self.assertEqual(self.select(["engine/audio/nested/src/lib.rs"]), ["nested"])

    def test_tool_ownership_and_engine_pak_dependents_are_selected(self):
        pak_directory = "engine/engine-pak"
        self.metadata["packages"] = [
            package("engine-pak", directory=pak_directory),
            package("engine-resources", ["engine-pak"],
                    directory="engine/engine-resources",
                    dependency_directories={"engine-pak": pak_directory}),
            package("rvepak", ["engine-pak"], directory="tools/rvepak",
                    dependency_directories={"engine-pak": pak_directory}),
        ]
        self.metadata["workspace_members"] = ["engine-pak", "engine-resources", "rvepak"]

        self.assertEqual(self.select(["tools/rvepak/src/main.rs"]), ["rvepak"])
        self.assertEqual(self.select(["engine/engine-pak/src/builder.rs"]),
                         ["engine-pak", "engine-resources", "rvepak"])


class ChangeRangeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.git("init", "--initial-branch=main")
        self.git("config", "user.email", "ci@example.invalid")
        self.git("config", "user.name", "CI tests")
        self.base = self.commit("engine/core/src/lib.rs")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
            cwd=self.root, text=True, stderr=subprocess.PIPE,
        ).strip()

    def commit(self, path):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(path + "\n")
        self.git("add", "--all")
        self.git("commit", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def test_pull_request_uses_merge_base_and_excludes_new_base_branch_changes(self):
        self.git("checkout", "-b", "feature")
        head = self.commit("engine/audio/src/lib.rs")
        self.git("checkout", "main")
        base = self.commit("engine/render/src/lib.rs")
        event = {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}
        self.assertEqual(changed_paths(self.root, "pull_request", event), ["engine/audio/src/lib.rs"])

    def test_push_includes_all_commits_in_the_push(self):
        self.commit("engine/audio/src/lib.rs")
        head = self.commit("engine/sandbox/tests/cli.rs")
        self.assertEqual(changed_paths(self.root, "push", {"before": self.base, "after": head}),
                         ["engine/audio/src/lib.rs", "engine/sandbox/tests/cli.rs"])

    def test_rename_reports_old_and_new_owner_and_preserves_unusual_filenames(self):
        self.git("mv", "engine/core/src/lib.rs", "engine/core/src/new\nname.rs")
        self.git("commit", "-m", "rename")
        paths = changed_paths(self.root, "push", {"before": self.base, "after": "HEAD"})
        self.assertEqual(paths, ["engine/core/src/lib.rs", "engine/core/src/new\nname.rs"])

    def test_deleted_files_are_included(self):
        self.git("rm", "engine/core/src/lib.rs")
        self.git("commit", "-m", "delete")
        self.assertEqual(changed_paths(self.root, "push", {"before": self.base, "after": "HEAD"}),
                         ["engine/core/src/lib.rs"])

    def test_new_branch_unavailable_push_base_and_manual_run_request_full_checks(self):
        for before in ["0" * 40, "1" * 40]:
            with self.subTest(before=before):
                self.assertIsNone(changed_paths(self.root, "push", {"before": before, "after": "HEAD"}))
        self.assertIsNone(changed_paths(self.root, "workflow_dispatch", {}))

    def test_invalid_pr_comparison_fails_instead_of_skipping_checks(self):
        event = {"pull_request": {"base": {"sha": "1" * 40}, "head": {"sha": "HEAD"}}}
        with self.assertRaises(subprocess.CalledProcessError):
            changed_paths(self.root, "pull_request", event)


if __name__ == "__main__":
    unittest.main()
