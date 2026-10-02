import unittest
from pathlib import PurePosixPath

from affected_packages import Dependency, Manifest, Metadata, parse_packages, plan

ROOT = "/work/zed"


def manifest(name: str, directory: str, *dependencies: str) -> Manifest:
    return {
        "id": f"path+file://{ROOT}/{directory}#{name}",
        "name": name,
        "manifest_path": f"{ROOT}/{directory}/Cargo.toml",
        "dependencies": [dependency(path) for path in dependencies],
    }


def dependency(path: str) -> Dependency:
    return {"name": PurePosixPath(path).name, "path": f"{ROOT}/{path}"}


def workspace(*manifests: Manifest) -> Metadata:
    return {
        "workspace_root": ROOT,
        "workspace_members": [manifest["id"] for manifest in manifests],
        "packages": list(manifests),
    }


PACKAGES = parse_packages(
    workspace(
        manifest("gpui", "crates/gpui"),
        manifest("assets", "crates/assets"),
        manifest("settings", "crates/settings", "crates/assets"),
        manifest("editor", "crates/editor", "crates/gpui", "crates/settings"),
        manifest("vim", "crates/vim", "crates/editor"),
        manifest("fs", "crates/fs"),
        manifest("fs_macros", "crates/fs/macros"),
        manifest("xtask", "tooling/xtask"),
        manifest("zed", "crates/zed", "crates/vim", "crates/fs", "crates/fs/macros"),
    )
)


class PlanTest(unittest.TestCase):
    def test_changed_crate_includes_every_reverse_dependency(self):
        result = plan(["crates/editor/src/editor.rs"], PACKAGES)
        self.assertEqual(result.scope, "packages")
        self.assertEqual(result.packages, ("editor", "vim", "zed"))

    def test_nested_package_owns_its_files(self):
        result = plan(["crates/fs/macros/src/lib.rs"], PACKAGES)
        self.assertEqual(result.packages, ("fs_macros", "zed"))

    def test_leaf_crate_tests_only_itself(self):
        result = plan(["tooling/xtask/src/main.rs"], PACKAGES)
        self.assertEqual(result.packages, ("xtask",))

    def test_assets_affect_their_embedding_crates(self):
        result = plan(["assets/keymaps/default-linux.json"], PACKAGES)
        self.assertEqual(
            result.packages, ("assets", "editor", "settings", "vim", "zed")
        )

    def test_files_outside_packages_affect_nothing(self):
        result = plan(
            ["docs/src/ai/skills.md", ".github/workflows/release.yml"], PACKAGES
        )
        self.assertEqual(result.scope, "none")
        self.assertEqual(result.outputs(), "scope=none\npackages=\npackage_args=\n")

    def test_workspace_wide_inputs_run_everything(self):
        for path in [
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
            ".mise.toml",
            ".miserc.toml",
            ".mise.ci-linux.toml",
            ".cargo/config.toml",
            "script/clippy",
            "script/fork-ci/affected_packages.py",
            ".github/workflows/fork_ci.yaml",
        ]:
            with self.subTest(path=path):
                self.assertEqual(plan([path], PACKAGES).scope, "all")

    def test_file_names_cannot_inject_output_lines(self):
        result = plan(["Cargo.lock\nscope=none"], PACKAGES)
        self.assertEqual(result.scope, "none")
        self.assertNotIn("\n", result.reason)

    def test_crate_manifest_is_not_workspace_wide(self):
        self.assertEqual(
            plan(["crates/vim/Cargo.toml"], PACKAGES).packages, ("vim", "zed")
        )

    def test_missing_base_runs_everything(self):
        self.assertEqual(plan(None, PACKAGES).scope, "all")

    def test_closure_covering_the_workspace_runs_everything(self):
        packages = parse_packages(
            workspace(
                manifest("core", "crates/core"),
                manifest("app", "crates/app", "crates/core"),
            )
        )
        self.assertEqual(plan(["crates/core/src/lib.rs"], packages).scope, "all")

    def test_outputs_are_cargo_arguments(self):
        self.assertEqual(
            plan(["crates/vim/src/vim.rs"], PACKAGES).outputs(),
            "scope=packages\npackages=vim zed\npackage_args=-p vim -p zed\n",
        )

    def test_dependencies_outside_the_workspace_are_ignored(self):
        packages = parse_packages({
            "workspace_root": ROOT,
            "workspace_members": [f"path+file://{ROOT}/crates/app#app"],
            "packages": [
                {
                    "id": f"path+file://{ROOT}/crates/app#app",
                    "name": "app",
                    "manifest_path": f"{ROOT}/crates/app/Cargo.toml",
                    "dependencies": [
                        {"name": "serde"},
                        {"name": "vendored", "path": "/elsewhere/vendored"},
                    ],
                }
            ],
        })
        self.assertEqual(packages[0].dependencies, frozenset())


if __name__ == "__main__":
    unittest.main()
