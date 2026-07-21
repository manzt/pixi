import tomllib
from pathlib import Path

import pytest
from dirty_equals import IsPartialDict
from inline_snapshot import snapshot

from .common import CURRENT_PLATFORM, ExitCode, verify_cli_command


def test_pixi_init_cwd(pixi: Path, tmp_pixi_workspace: Path) -> None:
    # Create a new project
    verify_cli_command([pixi, "init", "."], cwd=tmp_pixi_workspace)

    # Verify that the manifest file is created
    manifest_path = tmp_pixi_workspace / "pixi.toml"
    assert manifest_path.exists()

    # Verify that the manifest file contains expected content
    manifest_content = manifest_path.read_text()
    assert "[workspace]" in manifest_content


def test_pixi_init_non_existing_dir(pixi: Path, tmp_pixi_workspace: Path) -> None:
    # Specify project dir
    project_dir = tmp_pixi_workspace / "project_dir"

    # Create a new project
    verify_cli_command([pixi, "init", project_dir])

    # Verify that the manifest file is created
    manifest_path = project_dir / "pixi.toml"
    assert manifest_path.exists()

    # Verify that the manifest file contains expected content
    manifest_content = manifest_path.read_text()
    assert "[workspace]" in manifest_content


def test_pixi_init_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "scripts" / "example.py"
    script.parent.mkdir()
    script.write_text("#!/usr/bin/env python\nprint('hello')\n")

    verify_cli_command(
        [
            pixi,
            "init",
            "--script",
            script,
            "--channel",
            "testing",
            "--platform",
            CURRENT_PLATFORM,
        ]
    )

    assert script.read_text() == f'''#!/usr/bin/env python
#
# /// script
# requires-python = ">= 3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["testing"]
# platforms = ["{CURRENT_PLATFORM}"]
# ///

print('hello')
'''
    assert not (tmp_pixi_workspace / "pixi.toml").exists()
    assert not (tmp_pixi_workspace / ".pixi").exists()

    verify_cli_command(
        [pixi, "init", "--script", script],
        ExitCode.FAILURE,
        stderr_contains="already a PEP 723 script",
    )


def test_pixi_init_pixi_home_parent(pixi: Path, tmp_pixi_workspace: Path) -> None:
    pixi_home = tmp_pixi_workspace / ".pixi"
    pixi_home.mkdir(exist_ok=True)

    verify_cli_command(
        [pixi, "init", pixi_home.parent],
        ExitCode.FAILURE,
        # Test that we print a helpful error message
        stderr_contains="pixi init",
        env={"PIXI_HOME": str(pixi_home)},
    )


def test_pixi_init_import_environment_empty_pip(pixi: Path, tmp_pixi_workspace: Path) -> None:
    environment_file = tmp_pixi_workspace / "environment.yml"
    environment_file.write_text(
        """name: test
channels:
  - conda-forge
dependencies:
  - python=3.13
  - pip
  - pip:
"""
    )

    verify_cli_command(
        [pixi, "init", "--import", "environment.yml"],
        cwd=tmp_pixi_workspace,
    )

    manifest = tmp_pixi_workspace.joinpath("pixi.toml")

    assert manifest.is_file()

    assert tomllib.loads(manifest.read_text()) == snapshot(
        {
            "workspace": IsPartialDict,
            "tasks": {},
            "dependencies": {"python": "3.13.*", "pip": "*"},
        }
    )


@pytest.mark.slow
def test_pixi_init_pyproject(pixi: Path, tmp_pixi_workspace: Path) -> None:
    manifest_path = tmp_pixi_workspace / "pyproject.toml"
    # Create a new project
    verify_cli_command([pixi, "init", tmp_pixi_workspace, "--format", "pyproject"])
    # Verify that install works
    verify_cli_command([pixi, "install", "--manifest-path", manifest_path])
