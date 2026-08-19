import hashlib
import json
import os
import shlex
import signal
import subprocess
import threading
import time
import zipfile
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest
from inline_snapshot import snapshot

from .common import CONDA_FORGE_CHANNEL, CURRENT_PLATFORM, ExitCode, verify_cli_command

SCRIPT_RESOLUTION_STATE = "script-resolution-v1.json"


def assert_no_workspace_state_created(workspace: Path) -> None:
    assert {path.name for path in (workspace / ".pixi").iterdir()} == {"config.toml"}


def only_script_cache(exec_cache: Path) -> Path:
    entries = [path for path in exec_cache.iterdir() if path.is_dir()]
    assert len(entries) == 1
    return entries[0]


def script_resolution(cache: Path) -> str:
    stored = json.loads((cache / SCRIPT_RESOLUTION_STATE).read_text())
    assert stored["version"] == 1
    return stored["lock_file"]


def installed_conda_records(cache: Path, package: str) -> list[Path]:
    return list((cache / "envs" / "default" / "conda-meta").glob(f"{package}-*.json"))


def script_resolution_lock(script: Path) -> Path:
    import pwd

    identity = script.parent.resolve() / f"{script.name}.pixi.lock"
    digest = hashlib.sha256(os.fsencode(identity)).hexdigest()[:16]
    home = Path(pwd.getpwuid(os.geteuid()).pw_dir)
    return home / ".pixi" / "script-locks" / f"pixi-script-{digest}.lock"


@contextmanager
def unavailable_script_resolution_lock(script: Path) -> Iterator[None]:
    lock = script_resolution_lock(script)
    assert lock.is_file()
    lock.unlink()
    lock.mkdir()
    try:
        yield
    finally:
        lock.rmdir()


@contextmanager
def remote_script_server(source: str) -> Iterator[tuple[str, list[str]]]:
    requests: list[str] = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            requests.append(self.path)
            if self.path == "/redirect":
                self.send_response(302)
                self.send_header(
                    "Location",
                    f"http://127.0.0.1:{server.server_port}/extensionless",
                )
                self.end_headers()
                return
            if self.path == "/extensionless":
                body = source.encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            self.send_response(404)
            self.end_headers()

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def write_test_wheel(directory: Path, name: str, version: str) -> Path:
    wheel = directory / f"{name}-{version}-py3-none-any.whl"
    dist_info = f"{name}-{version}.dist-info"
    entries = {
        f"{name}/__init__.py": f'__version__ = "{version}"\n',
        f"{dist_info}/METADATA": (f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"),
        f"{dist_info}/WHEEL": (
            "Wheel-Version: 1.0\nGenerator: pixi-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
        ),
    }
    entries[f"{dist_info}/RECORD"] = "".join(f"{path},,\n" for path in entries) + (
        f"{dist_info}/RECORD,,\n"
    )
    with zipfile.ZipFile(wheel, "w") as archive:
        for path, contents in entries.items():
            archive.writestr(path, contents)
    return wheel


@contextmanager
def simple_pypi_server(root: Path) -> Iterator[str]:
    class Handler(SimpleHTTPRequestHandler):
        def __init__(self, *args: object, **kwargs: object) -> None:
            super().__init__(*args, directory=str(root), **kwargs)

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/simple"
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def test_pixi_init_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "scripts" / "example.py"
    script.parent.mkdir()
    script.write_text("#!/usr/bin/env python\nprint('hello')\n")

    verify_cli_command([pixi, "init", "--script", script, "--channel", "testing"])

    assert (
        script.read_text()
        == """#!/usr/bin/env python
#
# /// script
# requires-python = ">=3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["testing"]
# ///

print('hello')
"""
    )
    assert not (tmp_pixi_workspace / "pixi.toml").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)

    verify_cli_command(
        [pixi, "init", "--script", script],
        ExitCode.FAILURE,
        stderr_contains="already a PEP 723 script",
    )


def test_pixi_run_script_requires_inline_metadata(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text("print('hello')\n")

    verify_cli_command(
        [pixi, "run", "--script", script],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not contain a PEP 723 metadata block",
            "pixi init --script",
        ],
    )
    assert script.read_text() == "print('hello')\n"


@pytest.mark.slow
def test_pixi_run_remote_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    source = f'''# /// script
# requires-python = ">=3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# ///
import json
import os
import sys

print(json.dumps({{
    "argv": sys.argv[1:],
    "cwd": os.getcwd(),
    "file": __file__,
}}))
'''
    with remote_script_server(source) as (base_url, requests):
        output = verify_cli_command(
            [
                pixi,
                "run",
                "--script",
                f"{base_url}/redirect",
                "first",
                "--second",
            ],
            cwd=tmp_pixi_workspace,
        )

    payload = json.loads(next(line for line in output.stdout.splitlines() if line.startswith("{")))
    assert payload["argv"] == ["first", "--second"]
    assert payload["cwd"] == str(tmp_pixi_workspace)
    assert payload["file"].endswith(".py")
    assert not Path(payload["file"]).exists()
    assert requests == ["/redirect", "/extensionless"]
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_run_remote_script_reports_http_errors_and_rejects_locks(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    with remote_script_server("") as (base_url, requests):
        verify_cli_command(
            [pixi, "run", "--script", f"{base_url}/missing"],
            ExitCode.FAILURE,
            stderr_contains=["server returned 404", f"{base_url}/missing"],
        )
        verify_cli_command(
            [pixi, "run", "--frozen", "--script", f"{base_url}/extensionless"],
            ExitCode.FAILURE,
            stderr_contains=[
                "transient scripts cannot be run with `--frozen`",
                "do not have an adjacent lock file",
            ],
        )
    assert requests == ["/missing"]


@pytest.mark.slow
def test_pixi_run_stdin_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    exec_cache = tmp_pixi_workspace / "stdin-exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    source = f'''# /// script
# requires-python = ">=3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# ///
import json
import os
import sys

print(json.dumps({{
    "argv": sys.argv,
    "cwd": os.getcwd(),
    "has_file": "__file__" in globals(),
    "manifest": os.environ["PIXI_PROJECT_MANIFEST"],
    "remaining_stdin": sys.stdin.read(),
}}))
'''
    output = verify_cli_command(
        [pixi, "run", "--script", "-", "first", "--second"],
        cwd=tmp_pixi_workspace,
        env=env,
        stdin=source,
    )

    payload = json.loads(next(line for line in output.stdout.splitlines() if line.startswith("{")))
    assert payload == {
        "argv": ["-c", "first", "--second"],
        "cwd": str(tmp_pixi_workspace),
        "has_file": False,
        "manifest": "<stdin>",
        "remaining_stdin": "",
    }
    first_cache = only_script_cache(exec_cache)
    assert (first_cache / SCRIPT_RESOLUTION_STATE).is_file()

    body_changed = source.replace("import json", "# changed body\nimport json")
    verify_cli_command(
        [pixi, "run", "--script", "-", "first", "--second"],
        cwd=tmp_pixi_workspace,
        env=env,
        stdin=body_changed,
    )
    assert len(list(exec_cache.iterdir())) == 1
    assert only_script_cache(exec_cache) == first_cache
    assert (first_cache / SCRIPT_RESOLUTION_STATE).is_file()

    metadata_changed = source.replace(
        "# ///\nimport json", "# # identity change\n# ///\nimport json"
    )
    verify_cli_command(
        [pixi, "run", "--script", "-", "first", "--second"],
        cwd=tmp_pixi_workspace,
        env=env,
        stdin=metadata_changed,
    )
    assert len(list(exec_cache.iterdir())) == 2

    # The same transient metadata can refer to different relative artifacts
    # when invoked from another directory, so its semantic root is part of the
    # environment identity.
    other_root = tmp_pixi_workspace / "other-root"
    other_root.mkdir()
    verify_cli_command(
        [pixi, "run", "--script", "-", "first", "--second"],
        cwd=other_root,
        env=env,
        stdin=source,
    )
    assert len(list(exec_cache.iterdir())) == 3
    assert all(
        (cache / SCRIPT_RESOLUTION_STATE).is_file()
        for cache in exec_cache.iterdir()
        if cache.is_dir()
    )
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_run_stdin_script_errors_and_dry_run_are_source_safe(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    verify_cli_command(
        [pixi, "run", "--script", "-"],
        ExitCode.FAILURE,
        stderr_contains="stdin does not contain a PEP 723 metadata block",
        stdin="print('missing metadata')\n",
    )
    verify_cli_command(
        [pixi, "run", "--script", "-"],
        ExitCode.FAILURE,
        stderr_contains="<stdin>",
        stderr_excludes="stdin.py",
        stdin="# /// script\n# dependencies = [\n# ///\n",
    )

    secret_marker = "stdin-body-must-not-appear"
    source = f'''# /// script
# dependencies = []
# ///
print("{secret_marker}")
    '''
    verify_cli_command(
        [pixi, "-vvv", "run", "--dry-run", "--script", "-"],
        cwd=tmp_pixi_workspace,
        stdin=source,
        stdout_excludes=secret_marker,
        stderr_contains="python -c <stdin>",
        stderr_excludes=secret_marker,
    )
    verify_cli_command(
        [pixi, "run", "--locked", "--script", "-"],
        ExitCode.FAILURE,
        stderr_contains=[
            "transient scripts cannot be run with `--locked`",
            "do not have an adjacent lock file",
        ],
        stdin=secret_marker,
        stderr_excludes=secret_marker,
    )


def test_pixi_run_script_rejects_workspace_only_options(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        """# /// script
# dependencies = []
# ///
print("hello")
"""
    )
    original_script = script.read_text()

    for option in (["--environment", "test"], ["--skip-deps"]):
        verify_cli_command(
            [pixi, "run", "--script", script, *option],
            ExitCode.FAILURE,
            stderr_contains=[
                f"does not support {option[0]}",
                "one implicit default run environment and no Pixi task graph",
            ],
        )

    assert script.read_text() == original_script
    assert not script.with_name("example.py.pixi.lock").exists()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_lock_script_requires_inline_metadata(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text("print('hello')\n")

    verify_cli_command(
        [pixi, "lock", "--script", script],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not contain a PEP 723 metadata block",
            "pixi init --script",
        ],
    )

    assert script.read_text() == "print('hello')\n"
    assert not script.with_name("example.py.pixi.lock").exists()


@pytest.mark.slow
def test_pixi_run_script_is_isolated_and_does_not_create_a_lock(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    (tmp_pixi_workspace / "pixi.toml").write_text(
        f'''[workspace]
name = "enclosing"
channels = []
platforms = ["{CURRENT_PLATFORM}"]
'''
    )
    script = tmp_pixi_workspace / "scripts" / "example.py"
    script.parent.mkdir()
    script.write_text(
        """# /// script
# requires-python = ">=3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
#
# [tool.pixi.dependencies]
# ///
import json
import os
import sys

print(json.dumps({
    "argv": sys.argv[1:],
    "cwd": os.getcwd(),
    "manifest": os.environ["PIXI_PROJECT_MANIFEST"],
}))
"""
    )

    verify_cli_command(
        [pixi, "run", "--script", script, "first", "--second"],
        cwd=tmp_pixi_workspace,
        env={
            "PIXI_PROJECT_ROOT": str(tmp_pixi_workspace),
            "PIXI_ENVIRONMENT_NAME": "ignored",
        },
        stdout_contains=json.dumps(
            {
                "argv": ["first", "--second"],
                "cwd": str(tmp_pixi_workspace),
                "manifest": str(script),
            }
        ),
    )

    assert not script.with_name("example.py.pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


@pytest.mark.skipif(
    not CURRENT_PLATFORM.startswith("linux"),
    reason="the virtual-package fixture builds its CUDA package for Linux",
)
def test_pixi_run_script_explicit_platform_overrides_host_runnability(
    pixi: Path, tmp_pixi_workspace: Path, virtual_packages_channel: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {
        "CONDA_OVERRIDE_CUDA": "10",
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
    }
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{virtual_packages_channel}", "{CONDA_FORGE_CHANNEL}"]
# platforms = [{{ name = "gpu", platform = "{CURRENT_PLATFORM}", cuda = "13" }}]
#
# [tool.pixi.dependencies]
# cuda = "*"
# ///
print("ran explicit platform")
'''
    )

    verify_cli_command(
        [pixi, "run", "--script", script, "--platform", "gpu"],
        env=env,
        stdout_contains="ran explicit platform",
    )

    cache = only_script_cache(exec_cache)
    assert "name: gpu" in script_resolution(cache)
    assert installed_conda_records(cache, "cuda")


def test_pixi_run_script_reuses_a_sufficient_cached_resolution(
    pixi: Path, tmp_pixi_workspace: Path, dummy_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")

    def write_script(channel: str, dependency: str | None) -> None:
        dependency_table = (
            f'# [tool.pixi.dependencies]\n# {dependency} = "*"\n' if dependency is not None else ""
        )
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{channel}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
{dependency_table}# ///
print("ran")
'''
        )

    write_script(dummy_channel_1, "dummy-a")
    verify_cli_command(
        [pixi, "run", "--script", script],
        cwd=tmp_pixi_workspace,
        env=env,
        stdout_contains="ran",
    )

    cache = only_script_cache(exec_cache)
    initial_resolution = script_resolution(cache)
    assert "dummy-a-0.1.0-" in initial_resolution
    assert "dummy-c-0.1.0-" in initial_resolution
    assert installed_conda_records(cache, "dummy-a")
    assert installed_conda_records(cache, "dummy-c")
    assert not script_lock.exists()

    verify_cli_command(
        [pixi, "run", "--locked", "--script", script],
        ExitCode.FAILURE,
        cwd=tmp_pixi_workspace,
        env=env,
        stderr_contains="no lock file exists for the script",
    )

    # Resolution policy changed and the direct dependency was removed. The
    # installed superset is still usable, so an offline run must not resolve.
    write_script((tmp_pixi_workspace / "missing-channel").as_uri(), None)
    verify_cli_command(
        [pixi, "run", "--offline", "--script", script],
        cwd=tmp_pixi_workspace,
        env=env,
        stdout_contains="ran",
    )
    assert script_resolution(cache) == initial_resolution
    assert installed_conda_records(cache, "dummy-a")
    assert installed_conda_records(cache, "dummy-c")
    assert not script_lock.exists()

    # The same permissive metadata cannot repair an incomplete prefix under
    # acquisition policy that no longer permits its artifacts. uv only takes
    # this fast path after checking the installed environment itself.
    environment_file = cache / "envs" / "default" / "conda-meta" / "pixi"
    environment_file.unlink()
    write_script((tmp_pixi_workspace / "missing-channel").as_uri(), "dummy-a")
    verify_cli_command(
        [pixi, "run", "--offline", "--script", script],
        ExitCode.FAILURE,
        cwd=tmp_pixi_workspace,
        env=env,
    )
    assert not environment_file.exists()

    write_script(dummy_channel_1, "dummy-a")
    verify_cli_command(
        [pixi, "run", "--script", script],
        cwd=tmp_pixi_workspace,
        env=env,
        stdout_contains="ran",
    )
    assert environment_file.is_file()

    # Disposable state must recover as a cache miss, without user cleanup.
    (cache / SCRIPT_RESOLUTION_STATE).write_text("not json")
    write_script(dummy_channel_1, "dummy-a")
    verify_cli_command(
        [pixi, "run", "--script", script],
        cwd=tmp_pixi_workspace,
        env=env,
        stdout_contains="ran",
    )
    assert "dummy-a-0.1.0-" in script_resolution(cache)
    assert not script_lock.exists()


def test_pixi_run_script_releases_resolution_lock_before_user_code(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    handshake = tmp_pixi_workspace / "handshake"
    handshake.mkdir()
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        """# /// script
# dependencies = []
# ///
from pathlib import Path
import sys
import time

directory = Path(sys.argv[1])
role = sys.argv[2]
if role == "first":
    (directory / "first-started").write_text("")
    deadline = time.monotonic() + 10
    while not (directory / "second-started").exists() and time.monotonic() < deadline:
        time.sleep(0.05)
    if not (directory / "second-started").exists():
        raise RuntimeError("second invocation remained blocked")
else:
    (directory / "second-started").write_text("")
"""
    )
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    first = subprocess.Popen(
        [pixi, "run", "--script", script, handshake, "first"],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    second: subprocess.Popen[str] | None = None
    try:
        deadline = time.monotonic() + 10
        while not (handshake / "first-started").exists() and time.monotonic() < deadline:
            if first.poll() is not None:
                break
            time.sleep(0.05)
        assert (handshake / "first-started").exists(), first.communicate(timeout=1)

        second = subprocess.Popen(
            [pixi, "run", "--script", script, handshake, "second"],
            cwd=tmp_pixi_workspace,
            env=process_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        second_stdout, second_stderr = second.communicate(timeout=15)
        first_stdout, first_stderr = first.communicate(timeout=15)
        assert second.returncode == 0, (second_stdout, second_stderr)
        assert first.returncode == 0, (first_stdout, first_stderr)
        assert (handshake / "second-started").exists()
    finally:
        for process in (second, first):
            if process is not None and process.poll() is None:
                process.terminate()
                process.communicate(timeout=5)


@pytest.mark.skipif(os.name == "nt", reason="PEP 723 activation test uses a shell script")
@pytest.mark.slow
def test_pixi_run_script_releases_resolution_lock_before_activation(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    marker = tmp_pixi_workspace / "nested-finished"
    script = tmp_pixi_workspace / "example.py"
    activation = tmp_pixi_workspace / "activation.sh"
    activation.write_text(
        f"""if [ "${{PIXI_ACTIVATION_CHILD:-}}" != "1" ]; then
  PIXI_ACTIVATION_CHILD=1 {shlex.quote(str(pixi))} run --script {shlex.quote(str(script))} nested
  touch {shlex.quote(str(marker))}
fi
"""
    )
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
#
# [tool.pixi.activation]
# scripts = ["activation.sh"]
# ///
import sys
print(sys.argv[1])
'''
    )
    process_env = dict(os.environ) | {
        "PIXI_ACTIVATION_CHILD": "1",
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }

    # Warm the prefix so the timed invocation isolates lock lifetime from
    # dependency installation and network latency.
    verify_cli_command(
        [pixi, "run", "--script", script, "warmup"],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout_contains="warmup",
    )
    process_env.pop("PIXI_ACTIVATION_CHILD")

    process = subprocess.Popen(
        [pixi, "run", "--script", script, "outer"],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=20)
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate(timeout=5)
        pytest.fail(f"recursive activation remained blocked\nstdout:\n{stdout}\nstderr:\n{stderr}")

    assert process.returncode == 0, (stdout, stderr)
    assert "outer" in stdout
    assert marker.is_file()


@pytest.mark.skipif(os.name == "nt", reason="read-only cache permissions are Unix-specific")
@pytest.mark.slow
def test_pixi_run_script_allows_a_read_only_cache_and_unavailable_lock(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    lock_dir = tmp_pixi_workspace / "locks"
    lock_dir.mkdir()
    env = {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "TMPDIR": str(lock_dir),
    }
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("ran")
'''
    )

    verify_cli_command([pixi, "run", "--script", script], cwd=tmp_pixi_workspace, env=env)
    cache = only_script_cache(exec_cache)
    legacy_lock = cache / ".script-resolution.lock"
    cache.chmod(0o555)
    if legacy_lock.exists():
        legacy_lock.chmod(0o444)
    alternate_tmp = tmp_pixi_workspace / "alternate-tmp"
    alternate_tmp.mkdir()
    alternate_home = tmp_pixi_workspace / "alternate-home"
    alternate_home.mkdir()
    unavailable_env = env | {"HOME": str(alternate_home), "TMPDIR": str(alternate_tmp)}
    try:
        with unavailable_script_resolution_lock(script):
            verify_cli_command(
                [pixi, "run", "--no-install", "--script", script],
                cwd=tmp_pixi_workspace,
                env=unavailable_env,
                stdout_contains="ran",
                stderr_contains="failed to coordinate the cached script environment",
            )
            verify_cli_command(
                [pixi, "run", "--script", script],
                cwd=tmp_pixi_workspace,
                env=unavailable_env,
                stdout_contains="ran",
                stderr_contains="failed to coordinate the cached script environment",
            )
    finally:
        cache.chmod(0o755)
        if legacy_lock.exists():
            legacy_lock.chmod(0o644)


@pytest.mark.slow
def test_pixi_lock_script_writes_only_the_adjacent_lock(
    pixi: Path, tmp_pixi_workspace: Path, dummy_channel_1: str
) -> None:
    (tmp_pixi_workspace / "pixi.toml").write_text(
        f'''[workspace]
name = "enclosing"
channels = []
platforms = ["{CURRENT_PLATFORM}"]
'''
    )
    script = tmp_pixi_workspace / "scripts" / "example.py"
    script.parent.mkdir()
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{dummy_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# dummy-a = "*"
# ///
print("hello")
'''
    )
    original_script = script.read_text()
    script_lock = script.with_name("example.py.pixi.lock")
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}

    verify_cli_command(
        [pixi, "lock", "--script", script, "--dry-run"],
        cwd=tmp_pixi_workspace,
        env=env,
    )
    assert script.read_text() == original_script
    assert not script_lock.exists()
    assert not exec_cache.exists()

    verify_cli_command([pixi, "lock", "--script", script], cwd=tmp_pixi_workspace, env=env)
    assert script.read_text() == original_script
    assert script_lock.exists()
    assert not exec_cache.exists()

    verify_cli_command(
        [pixi, "run", "--locked", "--script", script],
        cwd=tmp_pixi_workspace,
        env=env,
        stdout_contains="hello",
    )
    assert (only_script_cache(exec_cache) / SCRIPT_RESOLUTION_STATE).is_file()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_update_script_is_exact_and_eager(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    other_platform = "linux-64" if CURRENT_PLATFORM != "linux-64" else "osx-64"

    def write_script(package: str | None, package2: str | None) -> None:
        dependencies = "".join(
            f'# {name} = "{spec}"\n'
            for name, spec in (("package", package), ("package2", package2))
            if spec is not None
        )
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}", "{other_platform}"]
#
# [tool.pixi.dependencies]
{dependencies}# ///
print("hello")
'''
        )

    write_script("==0.1.0", "==0.1.0")
    verify_cli_command([pixi, "update", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    assert "package-0.1.0-" in script_resolution(cache)
    assert "package2-0.1.0-" in script_resolution(cache)
    assert installed_conda_records(cache, "package")
    assert "package-0.1.0-" in installed_conda_records(cache, "package")[0].name
    assert "package2-0.1.0-" in installed_conda_records(cache, "package2")[0].name
    assert not script_lock.exists()

    write_script("*", "*")
    verify_cli_command(
        [
            pixi,
            "update",
            "--script",
            script,
            "--platform",
            CURRENT_PLATFORM,
            "package",
        ],
        env=env,
    )
    resolution = script_resolution(cache)
    assert "package-0.2.0-" in resolution
    assert "package-0.1.0-" in resolution
    assert "package2-0.1.0-" in script_resolution(cache)
    assert "package2-0.2.0-" not in script_resolution(cache)
    records = installed_conda_records(cache, "package")
    assert len(records) == 1
    assert "package-0.2.0-" in records[0].name
    records = installed_conda_records(cache, "package2")
    assert len(records) == 1
    assert "package2-0.1.0-" in records[0].name
    assert not script_lock.exists()

    write_script(None, None)
    verify_cli_command([pixi, "update", "--script", script], env=env)
    assert "package-" not in script_resolution(cache)
    assert "package2-" not in script_resolution(cache)
    assert installed_conda_records(cache, "package") == []
    assert installed_conda_records(cache, "package2") == []
    assert not script_lock.exists()


@pytest.mark.slow
def test_pixi_update_script_selectively_updates_pypi_package(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    index_root = tmp_pixi_workspace / "index"
    packages = index_root / "packages"
    packages.mkdir(parents=True)
    for name in ("demo", "stable"):
        project = index_root / "simple" / name
        project.mkdir(parents=True)
        wheels = [write_test_wheel(packages, name, version) for version in ("0.1.0", "0.2.0")]
        project.joinpath("index.html").write_text(
            "".join(f'<a href="../../packages/{wheel.name}">{wheel.name}</a>\n' for wheel in wheels)
        )

    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"

    def write_script(demo: str, stable: str, index_url: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# python = "3.12.*"
#
# [tool.pixi.pypi-dependencies]
# demo = {{ version = "{demo}", index = "{index_url}" }}
# stable = {{ version = "{stable}", index = "{index_url}" }}
# ///
print("hello")
'''
        )

    with simple_pypi_server(index_root) as index_url:
        write_script("==0.1.0", "==0.1.0", index_url)
        verify_cli_command([pixi, "update", "--script", script], env=env)
        cache = only_script_cache(exec_cache)
        assert "demo-0.1.0-py3-none-any.whl" in script_resolution(cache)
        assert "stable-0.1.0-py3-none-any.whl" in script_resolution(cache)

        write_script("*", "*", index_url)
        verify_cli_command([pixi, "update", "--script", script, "demo"], env=env)

    resolution = script_resolution(cache)
    assert "demo-0.2.0-py3-none-any.whl" in resolution
    assert "stable-0.1.0-py3-none-any.whl" in resolution
    assert "stable-0.2.0-py3-none-any.whl" not in resolution
    prefix = cache / "envs" / "default"
    assert list(prefix.rglob("demo-0.2.0.dist-info"))
    assert list(prefix.rglob("stable-0.1.0.dist-info"))


def test_pixi_update_script_write_modes(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"

    def write_script(spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0")
    verify_cli_command([pixi, "update", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    state_before = (cache / SCRIPT_RESOLUTION_STATE).read_bytes()
    records_before = [record.name for record in installed_conda_records(cache, "package")]
    assert len(records_before) == 1
    assert "package-0.1.0-" in records_before[0]

    write_script("*")
    verify_cli_command([pixi, "update", "--dry-run", "--script", script, "package"], env=env)
    assert (cache / SCRIPT_RESOLUTION_STATE).read_bytes() == state_before
    assert [record.name for record in installed_conda_records(cache, "package")] == records_before

    verify_cli_command([pixi, "update", "--no-install", "--script", script, "package"], env=env)
    assert "package-0.2.0-" in script_resolution(cache)
    assert [record.name for record in installed_conda_records(cache, "package")] == records_before
    assert not script.with_name("example.py.pixi.lock").exists()


@pytest.mark.skipif(os.name == "nt", reason="read-only lock permissions are Unix-specific")
def test_pixi_update_script_lockless_publication_is_required(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    lock_dir = tmp_pixi_workspace / "locks"
    lock_dir.mkdir()
    env = {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "TMPDIR": str(lock_dir),
    }
    script = tmp_pixi_workspace / "example.py"

    def write_script(spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0")
    verify_cli_command([pixi, "update", "--no-install", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    state = cache / SCRIPT_RESOLUTION_STATE
    state_before = state.read_bytes()
    with unavailable_script_resolution_lock(script):
        write_script("*")
        dry_run = verify_cli_command(
            [pixi, "update", "--dry-run", "--json", "--script", script],
            env=env,
        )
        assert "0.1.0" in dry_run.stdout
        assert "0.2.0" in dry_run.stdout
        assert state.read_bytes() == state_before
        verify_cli_command(
            [pixi, "update", "--no-install", "--script", script],
            ExitCode.FAILURE,
            env=env,
            stderr_contains="failed to lock the cached script environment",
        )

    assert state.read_bytes() == state_before
    assert "package-0.1.0-" in script_resolution(cache)
    assert "package-0.2.0-" not in script_resolution(cache)

    state.chmod(0o444)
    cache.chmod(0o555)
    try:
        verify_cli_command(
            [pixi, "update", "--no-install", "--script", script],
            ExitCode.FAILURE,
            env=env,
            stderr_contains="failed to write cached script resolution",
        )
    finally:
        cache.chmod(0o755)
        state.chmod(0o644)

    assert state.read_bytes() == state_before


def test_pixi_update_script_sidecar_refreshes_lockless_baseline(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")

    def write_script(spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0")
    verify_cli_command([pixi, "update", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    verify_cli_command([pixi, "lock", "--script", script], env=env)
    assert script_lock.is_file()

    write_script("*")
    verify_cli_command([pixi, "update", "--script", script, "package"], env=env)
    assert "package-0.2.0-" in script_lock.read_text()
    assert "package-0.2.0-" in script_resolution(cache)

    script_lock.unlink()
    verify_cli_command(
        [pixi, "run", "--script", script],
        env=env,
        stdout_contains="hello",
    )
    assert "package-0.2.0-" in script_resolution(cache)
    records = installed_conda_records(cache, "package")
    assert len(records) == 1
    assert "package-0.2.0-" in records[0].name


def test_pixi_remove_script_refreshes_lockless_baseline(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("hello")
'''
    )

    verify_cli_command([pixi, "run", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    assert "package-0.1.0-" in script_resolution(cache)
    verify_cli_command([pixi, "lock", "--script", script], env=env)

    verify_cli_command([pixi, "remove", "--script", script, "package"], env=env)
    assert "package-0.1.0-" not in script_lock.read_text()
    assert "package-0.1.0-" not in script_resolution(cache)
    assert installed_conda_records(cache, "package") == []

    script_lock.unlink()
    verify_cli_command(
        [pixi, "run", "--script", script],
        env=env,
        stdout_contains="hello",
    )
    assert "package-0.1.0-" not in script_resolution(cache)
    assert installed_conda_records(cache, "package") == []


def test_pixi_remove_from_lockless_script_invalidates_cached_resolution(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("hello")
'''
    )

    verify_cli_command([pixi, "run", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    assert "package-0.1.0-" in script_resolution(cache)

    verify_cli_command([pixi, "remove", "--script", script, "package"], env=env)
    assert not script_lock.exists()
    assert not (cache / SCRIPT_RESOLUTION_STATE).exists()

    verify_cli_command(
        [pixi, "run", "--script", script],
        env=env,
        stdout_contains="hello",
    )
    assert "package-0.1.0-" not in script_resolution(cache)
    assert installed_conda_records(cache, "package") == []


@pytest.mark.skipif(os.name == "nt", reason="fcntl prefix locks are Unix-specific")
def test_pixi_remove_script_does_not_recreate_a_deleted_sidecar(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("hello")
'''
    )

    verify_cli_command([pixi, "run", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    remove: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                remove = subprocess.Popen(
                    [pixi, "remove", "--script", script, "package"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package-0.1.0-" in script_lock.read_text():
                    assert remove.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("remove did not publish before prefix synchronization")
                    time.sleep(0.05)
                script_lock.unlink()
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)

        stdout, stderr = remove.communicate(timeout=60)
        assert remove.returncode == ExitCode.SUCCESS, (stdout, stderr)
    finally:
        if remove is not None and remove.poll() is None:
            remove.terminate()
            try:
                remove.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                remove.kill()
                remove.communicate(timeout=5)

    assert not script_lock.exists()
    assert "package-0.1.0-" not in script_resolution(cache)
    assert installed_conda_records(cache, "package") == []


@pytest.mark.skipif(os.name == "nt", reason="fcntl prefix locks are Unix-specific")
def test_pixi_add_script_accepts_a_deleted_sidecar_with_matching_hidden_state(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("hello")
'''
    )

    verify_cli_command([pixi, "run", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    add: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                add = subprocess.Popen(
                    [pixi, "add", "--script", script, "package2==0.2.0"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package2-0.2.0-" not in script_lock.read_text():
                    assert add.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("add did not publish before prefix synchronization")
                    time.sleep(0.05)
                script_lock.unlink()
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)

        stdout, stderr = add.communicate(timeout=60)
        assert add.returncode == ExitCode.SUCCESS, (stdout, stderr)
    finally:
        if add is not None and add.poll() is None:
            add.terminate()
            try:
                add.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                add.kill()
                add.communicate(timeout=5)

    assert not script_lock.exists()
    assert '# package2 = "==0.2.0"' in script.read_text()
    assert "package2-0.2.0-" in script_resolution(cache)
    records = installed_conda_records(cache, "package2")
    assert len(records) == 1
    assert records[0].name.startswith("package2-0.2.0-")


@pytest.mark.skipif(os.name == "nt", reason="fcntl prefix locks are Unix-specific")
def test_pixi_update_script_reconciles_prefix_after_concurrent_publication(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    script = tmp_pixi_workspace / "example.py"

    def write_script(package: str, package2: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{package}"
# package2 = "{package2}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0", "==0.1.0")
    verify_cli_command([pixi, "update", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    assert marker.is_file()
    write_script("*", "*")
    eager: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                eager = subprocess.Popen(
                    [pixi, "update", "--script", script, "package"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package-0.2.0-" not in script_resolution(cache):
                    assert eager.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("eager update did not publish before prefix synchronization")
                    time.sleep(0.05)

                inner = subprocess.run(
                    [pixi, "update", "--no-install", "--script", script, "package2"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    capture_output=True,
                    text=True,
                    timeout=5,
                    check=False,
                )
                assert inner.returncode == ExitCode.SUCCESS, (inner.stdout, inner.stderr)
                assert "package2-0.2.0-" in script_resolution(cache)
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)

        stdout, stderr = eager.communicate(timeout=60)
        assert eager.returncode == ExitCode.SUCCESS, (stdout, stderr)
    finally:
        if eager is not None and eager.poll() is None:
            eager.terminate()
            try:
                eager.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                eager.kill()
                eager.communicate(timeout=5)

    assert "package-0.2.0-" in script_resolution(cache)
    assert "package2-0.2.0-" in script_resolution(cache)
    for package in ("package", "package2"):
        records = installed_conda_records(cache, package)
        assert len(records) == 1
        assert f"{package}-0.2.0-" in records[0].name


@pytest.mark.skipif(os.name == "nt", reason="fcntl prefix locks are Unix-specific")
def test_pixi_update_script_converges_to_different_hidden_winner(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    script = tmp_pixi_workspace / "example.py"
    sidecar = script.with_name("example.py.pixi.lock")

    def write_script(package: str, package2: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{package}"
# package2 = "{package2}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0", "==0.1.0")
    verify_cli_command([pixi, "update", "--script", script], env=process_env)
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    write_script("*", "*")

    eager: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                eager = subprocess.Popen(
                    [pixi, "update", "--script", script, "package"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package-0.2.0-" not in sidecar.read_text():
                    assert eager.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("sidecar update did not publish before prefix synchronization")
                    time.sleep(0.05)

                sidecar.unlink()
                inner = subprocess.run(
                    [pixi, "update", "--no-install", "--script", script, "package2"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    capture_output=True,
                    text=True,
                    timeout=5,
                    check=False,
                )
                assert inner.returncode == ExitCode.SUCCESS, (inner.stdout, inner.stderr)
                assert "package2-0.2.0-" in script_resolution(cache)
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)

        stdout, stderr = eager.communicate(timeout=60)
        assert eager.returncode == ExitCode.SUCCESS, (stdout, stderr)
    finally:
        if eager is not None and eager.poll() is None:
            eager.terminate()
            try:
                eager.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                eager.kill()
                eager.communicate(timeout=5)

    assert not sidecar.exists()
    assert "package-0.2.0-" in script_resolution(cache)
    assert "package2-0.2.0-" in script_resolution(cache)
    for package in ("package", "package2"):
        records = installed_conda_records(cache, package)
        assert len(records) == 1
        assert f"{package}-0.2.0-" in records[0].name


@pytest.mark.skipif(os.name == "nt", reason="fcntl prefix locks are Unix-specific")
def test_pixi_run_script_reconciles_prefix_after_concurrent_publication(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    script = tmp_pixi_workspace / "example.py"

    def write_script(package: str, package2: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{package}"
# package2 = "{package2}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0", "==0.1.0")
    verify_cli_command([pixi, "run", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    assert marker.is_file()

    write_script("==0.2.0", "*")
    run_process: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                run_process = subprocess.Popen(
                    [pixi, "run", "--script", script],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package-0.2.0-" not in script_resolution(cache):
                    assert run_process.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("run did not publish before prefix synchronization")
                    time.sleep(0.05)

                inner = subprocess.run(
                    [pixi, "update", "--no-install", "--script", script, "package2"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    capture_output=True,
                    text=True,
                    timeout=5,
                    check=False,
                )
                assert inner.returncode == ExitCode.SUCCESS, (inner.stdout, inner.stderr)
                assert "package2-0.2.0-" in script_resolution(cache)
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)

        stdout, stderr = run_process.communicate(timeout=60)
        assert run_process.returncode == ExitCode.SUCCESS, (stdout, stderr)
        assert "hello" in stdout
    finally:
        if run_process is not None and run_process.poll() is None:
            run_process.terminate()
            try:
                run_process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                run_process.kill()
                run_process.communicate(timeout=5)

    assert "package-0.2.0-" in script_resolution(cache)
    assert "package2-0.2.0-" in script_resolution(cache)
    for package in ("package", "package2"):
        records = installed_conda_records(cache, package)
        assert len(records) == 1
        assert f"{package}-0.2.0-" in records[0].name


@pytest.mark.skipif(os.name == "nt", reason="Unix permissions and prefix locks are required")
def test_pixi_run_script_rejects_a_stale_sidecar_without_coordination(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    lock_dir = tmp_pixi_workspace / "locks"
    lock_dir.mkdir()
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
        "TMPDIR": str(lock_dir),
    }
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")

    def write_script(spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0")
    verify_cli_command([pixi, "run", "--script", script], env=process_env)
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    external_lock = script_resolution_lock(script)
    assert external_lock.is_file()
    external_lock.unlink()
    external_lock.mkdir()

    write_script("==0.2.0")
    run_process: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                run_process = subprocess.Popen(
                    [pixi, "run", "--script", script],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package-0.2.0-" not in script_lock.read_text():
                    assert run_process.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("run did not publish before prefix synchronization")
                    time.sleep(0.05)

                # Publish a different sidecar through the supported fail-open
                # update path, then restore the outer run's manifest snapshot.
                # The outer run must not accept its now-stale installed candidate.
                write_script("==0.1.0")
                inner = subprocess.run(
                    [pixi, "update", "--no-install", "--script", script],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                assert inner.returncode == ExitCode.SUCCESS, (inner.stdout, inner.stderr)
                assert "package-0.1.0-" in script_lock.read_text()
                write_script("==0.2.0")
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)

        stdout, stderr = run_process.communicate(timeout=60)
        assert run_process.returncode == ExitCode.SUCCESS, (stdout, stderr)
        assert "hello" in stdout
    finally:
        if run_process is not None and run_process.poll() is None:
            run_process.terminate()
            try:
                run_process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                run_process.kill()
                run_process.communicate(timeout=5)
        external_lock.rmdir()

    assert "package-0.2.0-" in script_lock.read_text()
    records = installed_conda_records(cache, "package")
    assert len(records) == 1
    assert records[0].name.startswith("package-0.2.0-")


def test_pixi_update_script_filtered_requires_prior_state(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    other_platform = "linux-64" if CURRENT_PLATFORM != "linux-64" else "osx-64"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}", "{other_platform}"]
#
# [tool.pixi.dependencies]
# package = "*"
# ///
print("hello")
'''
    )

    verify_cli_command(
        [pixi, "update", "--script", script, "--environment", "test"],
        ExitCode.FAILURE,
        env=env,
        stderr_contains=["does not support --environment", "one implicit default environment"],
    )

    selective_error = "cannot selectively update a script without a prior cached resolution"
    verify_cli_command(
        [pixi, "update", "--script", script, "package"],
        ExitCode.FAILURE,
        env=env,
        stderr_contains=[selective_error, "Run an unfiltered", "first"],
    )
    verify_cli_command(
        [pixi, "update", "--script", script, "--platform", CURRENT_PLATFORM],
        ExitCode.FAILURE,
        env=env,
        stderr_contains=selective_error,
    )
    assert not exec_cache.exists()

    verify_cli_command([pixi, "update", "--no-install", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    state = cache / SCRIPT_RESOLUTION_STATE
    state.write_text("not json")
    verify_cli_command(
        [pixi, "update", "--script", script, "--platform", CURRENT_PLATFORM],
        ExitCode.FAILURE,
        env=env,
        stderr_contains=selective_error,
    )
    assert state.read_text() == "not json"
    assert not script.with_name("example.py.pixi.lock").exists()


@pytest.mark.skipif(os.name == "nt", reason="read-only lock permissions are Unix-specific")
def test_pixi_update_script_sidecar_publication_requires_coordination(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    lock_dir = tmp_pixi_workspace / "locks"
    lock_dir.mkdir()
    env = {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "TMPDIR": str(lock_dir),
    }
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")

    def write_script(spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script("==0.1.0")
    verify_cli_command([pixi, "lock", "--script", script], env=env)
    verify_cli_command([pixi, "update", "--no-install", "--script", script], env=env)
    assert "package-0.1.0-" in script_lock.read_text()
    assert not exec_cache.exists()

    with unavailable_script_resolution_lock(script):
        write_script("*")
        verify_cli_command(
            [pixi, "update", "--no-install", "--script", script, "package"],
            ExitCode.FAILURE,
            env=env,
            stderr_contains="failed to lock the cached script environment",
        )

    assert "package-0.1.0-" in script_lock.read_text()
    assert "package-0.2.0-" not in script_lock.read_text()
    assert not exec_cache.exists()


@pytest.mark.slow
def test_pixi_update_script_dry_run_resolves_dynamic_pypi_source(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    source = tmp_pixi_workspace / "dynamic-dep"
    source.mkdir()
    source.joinpath("pyproject.toml").write_text(
        """[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"

[project]
name = "dynamic-dep"
dynamic = ["version"]
"""
    )
    source.joinpath("setup.py").write_text(
        'from setuptools import setup\nsetup(version="42.0.0")\n'
    )
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# python = "3.11.*"
#
# [tool.pixi.pypi-dependencies]
# dynamic-dep = {{ path = "./dynamic-dep" }}
# ///
print("hello")
'''
    )
    original_script = script.read_text()

    verify_cli_command([pixi, "update", "--dry-run", "--script", script], env=env)

    assert script.read_text() == original_script
    assert not script.with_name("example.py.pixi.lock").exists()
    assert not exec_cache.exists()


@pytest.mark.slow
@pytest.mark.parametrize("command", ["run", "update", "lock"])
def test_pixi_script_failed_dynamic_solve_does_not_touch_real_prefix(
    pixi: Path, tmp_pixi_workspace: Path, command: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    source = tmp_pixi_workspace / "broken-dynamic-dep"
    source.mkdir()
    source.joinpath("pyproject.toml").write_text(
        """[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"

[project]
name = "broken-dynamic-dep"
dynamic = ["version"]
"""
    )
    source.joinpath("setup.py").write_text('raise RuntimeError("metadata failed")\n')
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# python = "3.11.*"
#
# [tool.pixi.pypi-dependencies]
# broken-dynamic-dep = {{ path = "./broken-dynamic-dep" }}
# ///
print("hello")
'''
    )

    verify_cli_command(
        [pixi, command, "--script", script],
        ExitCode.FAILURE,
        env=env,
        stderr_contains="metadata failed",
    )

    assert not script.with_name("example.py.pixi.lock").exists()
    if exec_cache.exists():
        cache = only_script_cache(exec_cache)
        assert not (cache / "envs" / "default").exists()
        assert not (cache / SCRIPT_RESOLUTION_STATE).exists()


@pytest.mark.slow
def test_pixi_add_script_failed_dynamic_solve_restores_manifest_and_sidecar(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    source = tmp_pixi_workspace / "broken-dynamic-dep"
    source.mkdir()
    source.joinpath("pyproject.toml").write_text(
        """[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"

[project]
name = "broken-dynamic-dep"
dynamic = ["version"]
"""
    )
    source.joinpath("setup.py").write_text('raise RuntimeError("metadata failed")\n')
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# python = "3.11.*"
# ///
print("hello")
'''
    )
    verify_cli_command([pixi, "lock", "--script", script], env=env)
    source_before = script.read_bytes()
    lock_before = script_lock.read_bytes()

    verify_cli_command(
        [
            pixi,
            "add",
            "--script",
            script,
            "--pypi",
            f"broken-dynamic-dep @ {source.as_uri()}",
        ],
        ExitCode.FAILURE,
        env=env,
        stderr_contains="metadata failed",
    )

    assert script.read_bytes() == source_before
    assert script_lock.read_bytes() == lock_before
    cache = only_script_cache(exec_cache)
    assert not (cache / "envs" / "default").exists()
    assert not (cache / SCRIPT_RESOLUTION_STATE).exists()


@pytest.mark.skipif(
    os.name == "nt" or getattr(os, "geteuid", lambda: 1)() == 0,
    reason="read-only directory permissions require an unprivileged Unix user",
)
def test_pixi_add_script_read_only_parent_preserves_manifest_and_sidecar(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
# ///
print("hello")
'''
    )
    verify_cli_command([pixi, "lock", "--script", script])
    source_before = script.read_bytes()
    lock_before = script_lock.read_bytes()
    original_mode = tmp_pixi_workspace.stat().st_mode
    tmp_pixi_workspace.chmod(0o555)
    try:
        verify_cli_command(
            [pixi, "add", "--script", script, "package==0.2.0"],
            ExitCode.FAILURE,
        )
    finally:
        tmp_pixi_workspace.chmod(original_mode)

    assert script.read_bytes() == source_before
    assert script_lock.read_bytes() == lock_before


@pytest.mark.skipif(
    os.name == "nt" or getattr(os, "geteuid", lambda: 1)() == 0,
    reason="read-only directory permissions require an unprivileged Unix user",
)
def test_pixi_lockless_edit_failure_preserves_cached_resolution(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    env = {"PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache)}
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("hello")
'''
    )
    verify_cli_command([pixi, "run", "--script", script], env=env)
    cache = only_script_cache(exec_cache)
    source_before = script.read_bytes()
    resolution_before = (cache / SCRIPT_RESOLUTION_STATE).read_bytes()
    records_before = installed_conda_records(cache, "package")
    original_mode = tmp_pixi_workspace.stat().st_mode

    tmp_pixi_workspace.chmod(0o555)
    try:
        verify_cli_command(
            [pixi, "remove", "--script", script, "package"],
            ExitCode.FAILURE,
            env=env,
        )
    finally:
        tmp_pixi_workspace.chmod(original_mode)

    assert script.read_bytes() == source_before
    assert (cache / SCRIPT_RESOLUTION_STATE).read_bytes() == resolution_before
    assert installed_conda_records(cache, "package") == records_before


def test_pixi_update_script_releases_resolution_lock_before_solve(
    pixi: Path, tmp_pixi_workspace: Path, multiple_versions_channel_1: str
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    script = tmp_pixi_workspace / "example.py"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }

    def write_script(channels: list[str], spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = {json.dumps(channels)}
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script([multiple_versions_channel_1, CONDA_FORGE_CHANNEL], "==0.1.0")
    verify_cli_command([pixi, "update", "--no-install", "--script", script], env=process_env)

    request_started = threading.Event()
    release_request = threading.Event()

    class BlockingChannelHandler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            request_started.set()
            release_request.wait(timeout=20)
            self.send_response(404)
            self.end_headers()

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), BlockingChannelHandler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    channel = f"http://127.0.0.1:{server.server_port}"
    write_script([channel, CONDA_FORGE_CHANNEL], "*")
    outer = subprocess.Popen(
        [pixi, "update", "--no-install", "--script", script],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        if not request_started.wait(timeout=10):
            if outer.poll() is not None:
                stdout, stderr = outer.communicate()
                pytest.fail(f"update exited before solving: {stdout}\n{stderr}")
            pytest.fail("update did not start resolving within 10 seconds")
        inner = subprocess.run(
            [pixi, "update", "--no-install", "--script", script, "not-locked"],
            cwd=tmp_pixi_workspace,
            env=process_env,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        assert inner.returncode == ExitCode.FAILURE
        assert "could not find a package named 'not-locked'" in inner.stderr
    finally:
        release_request.set()
        server.shutdown()
        server_thread.join()
        server.server_close()
        if outer.poll() is None:
            outer.terminate()
        outer.communicate(timeout=5)


def test_pixi_run_script_releases_resolution_lock_before_solve(
    pixi: Path,
    tmp_pixi_workspace: Path,
    channels: Path,
    multiple_versions_channel_1: str,
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    script = tmp_pixi_workspace / "example.py"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }

    def write_script(channel: str, spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{channel}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script(multiple_versions_channel_1, "==0.1.0")
    verify_cli_command([pixi, "run", "--script", script], env=process_env)

    request_started = threading.Event()
    release_request = threading.Event()
    channel_directory = channels / "multiple_versions_channel_1"

    class BlockingChannelHandler(SimpleHTTPRequestHandler):
        def __init__(self, *args: object, **kwargs: object) -> None:
            super().__init__(*args, directory=str(channel_directory), **kwargs)

        def do_GET(self) -> None:
            request_started.set()
            release_request.wait(timeout=20)
            super().do_GET()

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), BlockingChannelHandler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    channel = f"http://127.0.0.1:{server.server_port}"
    write_script(channel, "==0.2.0")
    outer = subprocess.Popen(
        [pixi, "run", "--script", script],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert request_started.wait(timeout=10)
        inner = subprocess.run(
            [pixi, "update", "--no-install", "--script", script, "not-locked"],
            cwd=tmp_pixi_workspace,
            env=process_env,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        assert inner.returncode == ExitCode.FAILURE
        assert "could not find a package named 'not-locked'" in inner.stderr
        release_request.set()
        stdout, stderr = outer.communicate(timeout=30)
        assert outer.returncode == ExitCode.SUCCESS, (stdout, stderr)
        assert "hello" in stdout
    finally:
        release_request.set()
        server.shutdown()
        server_thread.join()
        server.server_close()
        if outer.poll() is None:
            outer.terminate()
            try:
                outer.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                outer.kill()
                outer.communicate(timeout=5)


@pytest.mark.parametrize("command", ["run", "update"])
@pytest.mark.parametrize("metadata_change", [False, True], ids=["python-body", "metadata"])
def test_pixi_script_validates_environment_changes_during_solve(
    pixi: Path,
    tmp_pixi_workspace: Path,
    channels: Path,
    multiple_versions_channel_1: str,
    command: str,
    metadata_change: bool,
) -> None:
    exec_cache = tmp_pixi_workspace / "exec-cache"
    script = tmp_pixi_workspace / "example.py"
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }

    def write_script(channel: str, spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{channel}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script(multiple_versions_channel_1, "==0.1.0")
    verify_cli_command([pixi, "update", "--no-install", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    state_before = (cache / SCRIPT_RESOLUTION_STATE).read_bytes()

    request_started = threading.Event()
    release_request = threading.Event()
    channel_directory = channels / "multiple_versions_channel_1"

    class BlockingChannelHandler(SimpleHTTPRequestHandler):
        def __init__(self, *args: object, **kwargs: object) -> None:
            super().__init__(*args, directory=str(channel_directory), **kwargs)

        def do_GET(self) -> None:
            request_started.set()
            release_request.wait(timeout=20)
            super().do_GET()

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), BlockingChannelHandler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    channel = f"http://127.0.0.1:{server.server_port}"
    write_script(channel, "*")
    command_args = (
        [pixi, "update", "--no-install", "--script", script]
        if command == "update"
        else [pixi, "run", "--script", script]
    )
    outer = subprocess.Popen(
        command_args,
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert request_started.wait(timeout=10)
        if metadata_change:
            script.write_text(
                script.read_text().replace('# package = "*"', '# package = "==0.1.0"')
            )
        else:
            script.write_text(script.read_text().replace('print("hello")', 'print("edited body")'))
        release_request.set()
        stdout, stderr = outer.communicate(timeout=30)
        expected = ExitCode.FAILURE if metadata_change else ExitCode.SUCCESS
        assert outer.returncode == expected, (stdout, stderr)
        if metadata_change:
            assert "the script environment changed while it was being updated" in stderr
    finally:
        release_request.set()
        server.shutdown()
        server_thread.join()
        server.server_close()
        if outer.poll() is None:
            outer.terminate()
            try:
                outer.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                outer.kill()
                outer.communicate(timeout=5)

    if metadata_change:
        assert (cache / SCRIPT_RESOLUTION_STATE).read_bytes() == state_before
        if command == "run":
            assert "hello" not in stdout
            assert not (cache / "envs" / "default").exists()
    else:
        assert 'print("edited body")' in script.read_text()
        assert "package-0.2.0-" in script_resolution(cache)
        if command == "run":
            assert "edited body" in stdout


def test_pixi_lock_script_rejects_stale_manifest_publication(
    pixi: Path,
    tmp_pixi_workspace: Path,
    channels: Path,
    multiple_versions_channel_1: str,
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    process_env = dict(os.environ) | {"PIXI_NO_WRAP": "1"}

    def write_script(channel: str, spec: str) -> None:
        script.write_text(
            f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{channel}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "{spec}"
# ///
print("hello")
'''
        )

    write_script(multiple_versions_channel_1, "==0.1.0")
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    lock_before = script_lock.read_bytes()

    request_started = threading.Event()
    release_request = threading.Event()
    channel_directory = channels / "multiple_versions_channel_1"

    class BlockingChannelHandler(SimpleHTTPRequestHandler):
        def __init__(self, *args: object, **kwargs: object) -> None:
            super().__init__(*args, directory=str(channel_directory), **kwargs)

        def do_GET(self) -> None:
            request_started.set()
            release_request.wait(timeout=20)
            super().do_GET()

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), BlockingChannelHandler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    channel = f"http://127.0.0.1:{server.server_port}"
    write_script(channel, "==0.2.0")
    outer = subprocess.Popen(
        [pixi, "lock", "--script", script],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        assert request_started.wait(timeout=10)
        write_script(multiple_versions_channel_1, "==0.1.0")
        release_request.set()
        stdout, stderr = outer.communicate(timeout=30)
        assert outer.returncode == ExitCode.FAILURE, (stdout, stderr)
        assert "the script environment changed while it was being updated" in stderr
    finally:
        release_request.set()
        server.shutdown()
        server_thread.join()
        server.server_close()
        if outer.poll() is None:
            outer.terminate()
            try:
                outer.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                outer.kill()
                outer.communicate(timeout=5)

    assert script_lock.read_bytes() == lock_before


@pytest.mark.parametrize("metadata_change", [False, True], ids=["python-body", "metadata"])
def test_pixi_add_script_rebases_body_edits_and_rejects_metadata_races(
    pixi: Path,
    tmp_pixi_workspace: Path,
    channels: Path,
    multiple_versions_channel_1: str,
    metadata_change: bool,
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    process_env = dict(os.environ) | {"PIXI_NO_WRAP": "1"}
    request_started = threading.Event()
    release_request = threading.Event()
    channel_directory = channels / "multiple_versions_channel_1"

    class BlockingChannelHandler(SimpleHTTPRequestHandler):
        def __init__(self, *args: object, **kwargs: object) -> None:
            super().__init__(*args, directory=str(channel_directory), **kwargs)

        def do_GET(self) -> None:
            request_started.set()
            release_request.wait(timeout=20)
            super().do_GET()

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), BlockingChannelHandler)
    server_thread = threading.Thread(target=server.serve_forever)
    server_thread.start()
    channel = f"http://127.0.0.1:{server.server_port}"
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
# ///
print("original body")
'''
    )
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    lock_before = script_lock.read_bytes()
    script.write_text(script.read_text().replace(multiple_versions_channel_1, channel))
    outer = subprocess.Popen(
        [pixi, "add", "--script", script, "package==0.2.0"],
        cwd=tmp_pixi_workspace,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        if not request_started.wait(timeout=10):
            if outer.poll() is not None:
                stdout, stderr = outer.communicate()
                pytest.fail(f"add exited before solving: {stdout}\n{stderr}")
            pytest.fail("add did not start resolving within 10 seconds")
        if metadata_change:
            script.write_text(
                script.read_text().replace(
                    "# dependencies = []", '# dependencies = ["requests==2.32.5"]'
                )
            )
        else:
            script.write_text(script.read_text().replace("original body", "externally edited body"))
        release_request.set()
        stdout, stderr = outer.communicate(timeout=45)
        expected = ExitCode.FAILURE if metadata_change else ExitCode.SUCCESS
        assert outer.returncode == expected, (stdout, stderr)
        if metadata_change:
            assert "the script environment changed while it was being updated" in stderr
    finally:
        release_request.set()
        server.shutdown()
        server_thread.join()
        server.server_close()
        if outer.poll() is None:
            outer.terminate()
            try:
                outer.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                outer.kill()
                outer.communicate(timeout=5)

    if metadata_change:
        assert '# dependencies = ["requests==2.32.5"]' in script.read_text()
        assert script_lock.read_bytes() == lock_before
    else:
        assert "externally edited body" in script.read_text()
        assert '# package = "==0.2.0"' in script.read_text()
        assert "package-0.2.0-" in script_lock.read_text()


@pytest.mark.skipif(os.name == "nt", reason="SIGINT delivery differs on Windows")
def test_pixi_add_script_sigint_restores_metadata_and_preserves_body_edits(
    pixi: Path,
    tmp_pixi_workspace: Path,
    multiple_versions_channel_1: str,
) -> None:
    import fcntl

    exec_cache = tmp_pixi_workspace / "exec-cache"
    script = tmp_pixi_workspace / "example.py"
    script_lock = script.with_name("example.py.pixi.lock")
    process_env = dict(os.environ) | {
        "PIXI_CACHE_EXEC_ENVIRONMENTS_DIR": str(exec_cache),
        "PIXI_NO_WRAP": "1",
    }
    script.write_text(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{multiple_versions_channel_1}", "{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# package = "==0.1.0"
# ///
print("original body")
'''
    )
    verify_cli_command([pixi, "run", "--script", script], env=process_env)
    verify_cli_command([pixi, "lock", "--script", script], env=process_env)
    cache = only_script_cache(exec_cache)
    marker = cache / "envs" / "default" / "conda-meta" / ".pixi-environment-fingerprint"
    source_before = script.read_text()
    lock_before = script_lock.read_bytes()
    add: subprocess.Popen[str] | None = None
    try:
        with marker.open("r+b") as marker_file:
            fcntl.flock(marker_file, fcntl.LOCK_EX)
            try:
                add = subprocess.Popen(
                    [pixi, "add", "--script", script, "package2==0.2.0"],
                    cwd=tmp_pixi_workspace,
                    env=process_env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                deadline = time.monotonic() + 30
                while "package2-0.2.0-" not in script_lock.read_text():
                    assert add.poll() is None
                    if time.monotonic() >= deadline:
                        pytest.fail("add did not publish before prefix synchronization")
                    time.sleep(0.05)
                assert '# package2 = "==0.2.0"' in script.read_text()
                script.write_text(
                    script.read_text().replace("original body", "externally edited body")
                )
                add.send_signal(signal.SIGINT)
            finally:
                fcntl.flock(marker_file, fcntl.LOCK_UN)
        stdout, stderr = add.communicate(timeout=10)
        assert add.returncode == 130, (stdout, stderr)
        assert "dependency update cancelled" in stderr
    finally:
        if add is not None and add.poll() is None:
            add.terminate()
            try:
                add.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                add.kill()
                add.communicate(timeout=5)

    assert script.read_text() == source_before.replace("original body", "externally edited body")
    assert script_lock.read_bytes() == lock_before
    assert "package2-0.2.0-" not in script_resolution(cache)
    assert installed_conda_records(cache, "package2") == []


def test_pixi_add_script_requires_inline_metadata(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text("print('hello')\n")

    verify_cli_command(
        [pixi, "add", "--script", script, "rich"],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not contain a PEP 723 metadata block",
            "pixi init --script",
        ],
    )

    assert script.read_text() == "print('hello')\n"
    assert not script.with_name("example.py.pixi.lock").exists()


def test_pixi_dependency_mutations_reject_workspace_only_options(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        """# /// script
# dependencies = []
# ///
print("hello")
"""
    )
    original_script = script.read_text()

    for command, options in [
        ("add", ["--feature", "test", "--host"]),
        ("add", ["--environment", "test"]),
        ("remove", ["--feature", "test", "--build"]),
        ("remove", ["--environment", "test"]),
    ]:
        verify_cli_command(
            [pixi, command, "--script", script, *options, "bzip2"],
            ExitCode.FAILURE,
            stderr_contains=[
                f"`pixi {command} --script` does not support",
                options[0],
                "one implicit default run environment",
            ],
        )

    assert script.read_text() == original_script
    assert not script.with_name("example.py.pixi.lock").exists()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


@pytest.mark.slow
def test_pixi_add_script_writes_conda_and_pypi_dependencies(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# requires-python = ">=3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    script_lock = script.with_name("example.py.pixi.lock")

    verify_cli_command(
        [pixi, "add", "--script", script, "--no-install", "bzip2"],
        cwd=tmp_pixi_workspace,
        stderr_contains="Added bzip2",
    )
    assert not script_lock.exists()

    verify_cli_command([pixi, "lock", "--script", script], cwd=tmp_pixi_workspace)
    original_lock = script_lock.read_text()

    verify_cli_command(
        [
            pixi,
            "add",
            "--script",
            script,
            "--no-install",
            "--pypi",
            "requests==2.32.5",
        ],
        cwd=tmp_pixi_workspace,
        stderr_contains="Added requests==2.32.5",
    )

    assert script.read_text() == snapshot(
        f'''# /// script
# requires-python = ">=3.11"
# dependencies = ["requests==2.32.5"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
#
# [tool.pixi.dependencies]
# bzip2 = "*"
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    assert script_lock.read_text() != original_lock
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


@pytest.mark.slow
def test_pixi_add_script_writes_representable_dependency_options(
    pixi: Path, tmp_pixi_workspace: Path
) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# requires-python = ">=3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    editable = tmp_pixi_workspace / "demo-editable"
    editable.mkdir()
    (editable / "pyproject.toml").write_text(
        """[project]
name = "demo-editable"
version = "0.1.0"
"""
    )
    source = tmp_pixi_workspace / "demo-source"
    source.mkdir()
    (source / "pyproject.toml").write_text(
        """[project]
name = "demo-source"
version = "0.1.0"
"""
    )

    verify_cli_command(
        [
            pixi,
            "add",
            "--script",
            script,
            "--no-install",
            "--platform",
            CURRENT_PLATFORM,
            "zlib",
        ],
        cwd=tmp_pixi_workspace,
        stderr_contains=["Added zlib", f"platform(s): {CURRENT_PLATFORM}"],
    )
    verify_cli_command(
        [
            pixi,
            "add",
            "--script",
            script,
            "--no-install",
            "--pypi",
            "--index",
            "https://pypi.org/simple",
            "requests==2.32.5",
        ],
        cwd=tmp_pixi_workspace,
        stderr_contains="Added requests==2.32.5",
    )
    verify_cli_command(
        [
            pixi,
            "add",
            "--script",
            script,
            "--no-install",
            "--pypi",
            "--editable",
            "demo-editable @ ./demo-editable",
        ],
        cwd=tmp_pixi_workspace,
        stderr_contains="Added demo-editable @ ./demo-editable",
    )
    verify_cli_command(
        [
            pixi,
            "add",
            "--script",
            script,
            "--no-install",
            "--pypi",
            "demo-source @ ./demo-source",
        ],
        cwd=tmp_pixi_workspace,
        stderr_contains="Added demo-source @ ./demo-source",
    )

    assert script.read_text() == snapshot(
        f'''# /// script
# requires-python = ">=3.11"
# dependencies = ["demo-source @ {source.as_uri()}"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.target.{CURRENT_PLATFORM}.dependencies]
# zlib = "*"
#
# [tool.pixi.pypi-dependencies]
# requests = {{ version = "==2.32.5", index = "https://pypi.org/simple" }}
# demo-editable = {{ path = "./demo-editable", editable = true }}
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    verify_cli_command(
        [
            pixi,
            "remove",
            "--script",
            script,
            "--no-install",
            "--platform",
            CURRENT_PLATFORM,
            "zlib",
        ],
        cwd=tmp_pixi_workspace,
        stderr_contains="Removed zlib",
    )
    assert script.read_text() == snapshot(
        f'''# /// script
# requires-python = ">=3.11"
# dependencies = ["demo-source @ {source.as_uri()}"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.pypi-dependencies]
# requests = {{ version = "==2.32.5", index = "https://pypi.org/simple" }}
# demo-editable = {{ path = "./demo-editable", editable = true }}
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    assert not script.with_name("example.py.pixi.lock").exists()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_remove_script_requires_inline_metadata(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text("print('hello')\n")

    verify_cli_command(
        [pixi, "remove", "--script", script, "requests"],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not contain a PEP 723 metadata block",
            "pixi init --script",
        ],
    )
    assert script.read_text() == "print('hello')\n"


def test_pixi_workspace_channel_edits_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
"""
    )
    original_script = script.read_text()

    verify_cli_command(
        [
            pixi,
            "workspace",
            "channel",
            "add",
            "--script",
            script,
            "--feature",
            "test",
            "--no-install",
            "conda-forge",
        ],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not support --feature",
            "one implicit default run environment",
        ],
    )
    assert script.read_text() == original_script

    verify_cli_command(
        [
            pixi,
            "workspace",
            "channel",
            "add",
            "--script",
            script,
            "--no-install",
            "conda-forge",
        ],
        stderr_contains="Added conda-forge",
    )
    assert script.read_text() == snapshot(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# ///
print("hello")
"""
    )
    assert not script.with_name("example.py.pixi.lock").exists()

    verify_cli_command(
        [pixi, "workspace", "channel", "list", "--script", script],
        stdout_contains=["Environment: default", "- conda-forge"],
    )

    verify_cli_command(
        [
            pixi,
            "workspace",
            "channel",
            "remove",
            "--script",
            script,
            "--no-install",
            "conda-forge",
        ],
        stderr_contains="Removed conda-forge",
    )
    assert script.read_text() == snapshot(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# channels = []
# ///
print("hello")
"""
    )
    assert not script.with_name("example.py.pixi.lock").exists()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_workspace_platform_edits_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# platforms = ["linux-aarch64"]
# ///
print("hello")
"""
    )
    original_script = script.read_text()

    verify_cli_command(
        [
            pixi,
            "workspace",
            "platform",
            "add",
            "--script",
            script,
            "--feature",
            "test",
            "--no-install",
            "linux-ci=linux-64",
        ],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not support --feature",
            "one implicit default run environment",
        ],
    )
    assert script.read_text() == original_script

    verify_cli_command(
        [
            pixi,
            "workspace",
            "platform",
            "add",
            "--script",
            script,
            "--no-install",
            "linux-ci=linux-64",
            "mac-ci=osx-64",
        ],
        stderr_contains=["Added linux-ci", "Added mac-ci"],
    )
    assert script.read_text() == snapshot(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# platforms = ["linux-aarch64", { name = "linux-ci", platform = "linux-64" }, { name = "mac-ci", platform = "osx-64" }]
# ///
print("hello")
"""
    )

    verify_cli_command(
        [
            pixi,
            "workspace",
            "platform",
            "edit",
            "--script",
            script,
            "linux-ci",
            "--cuda",
            "12.0",
            "--no-install",
        ],
        stderr_contains="Updated platform linux-ci",
    )
    assert script.read_text() == snapshot(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# platforms = ["linux-aarch64", { name = "linux-ci", platform = "linux-64", cuda = "12.0" }, { name = "mac-ci", platform = "osx-64" }]
# ///
print("hello")
"""
    )

    verify_cli_command(
        [
            pixi,
            "workspace",
            "platform",
            "move",
            "--script",
            script,
            "mac-ci",
            "--to-top",
            "--no-install",
        ],
        stderr_contains="Moved platform mac-ci",
    )
    assert script.read_text() == snapshot(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# platforms = [{ name = "mac-ci", platform = "osx-64" }, "linux-aarch64", { name = "linux-ci", platform = "linux-64", cuda = "12.0" }]
# ///
print("hello")
"""
    )

    verify_cli_command(
        [
            pixi,
            "workspace",
            "platform",
            "list",
            "--script",
            script,
            "--machine-readable",
        ],
        stdout_contains="mac-ci linux-aarch64 linux-ci",
    )

    verify_cli_command(
        [
            pixi,
            "workspace",
            "platform",
            "remove",
            "--script",
            script,
            "--no-install",
            "mac-ci",
            "linux-aarch64",
            "linux-ci",
        ],
        stderr_contains=["Removed mac-ci", "Removed linux-ci"],
    )
    assert script.read_text() == snapshot(
        """# /// script
# dependencies = []
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# platforms = []
# ///
print("hello")
"""
    )
    assert not script.with_name("example.py.pixi.lock").exists()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_tree_reads_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = ["requests==2.32.5"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# bzip2 = "*"
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    original_script = script.read_text()
    script_lock = script.with_name("example.py.pixi.lock")

    verify_cli_command(
        [
            pixi,
            "tree",
            "--script",
            script,
            "--environment",
            "test",
            "--no-install",
        ],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not support --environment",
            "one implicit default run environment",
        ],
    )
    output = verify_cli_command(
        [pixi, "tree", "--script", script, "--no-install"],
        stdout_contains=["bzip2", "requests"],
    )
    assert "default" not in output.stdout
    assert script.read_text() == original_script
    assert not script_lock.exists()

    verify_cli_command([pixi, "lock", "--script", script])
    original_lock = script_lock.read_text()
    verify_cli_command(
        [pixi, "tree", "--script", script, "--locked", "--no-install"],
        stdout_contains=["bzip2", "requests"],
    )

    assert script.read_text() == original_script
    assert script_lock.read_text() == original_lock
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_list_reads_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = ["requests==2.32.5"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# bzip2 = "*"
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    original_script = script.read_text()
    script_lock = script.with_name("example.py.pixi.lock")

    verify_cli_command(
        [
            pixi,
            "list",
            "--script",
            script,
            "--environment",
            "test",
            "--no-install",
        ],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not support --environment",
            "one implicit default run environment",
        ],
    )
    verify_cli_command(
        [pixi, "list", "--script", script, "--no-install"],
        stdout_contains=["bzip2", "requests"],
    )
    assert script.read_text() == original_script
    assert not script_lock.exists()
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


def test_pixi_workspace_export_reads_script(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = ["requests==2.32.5"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
# platforms = ["{CURRENT_PLATFORM}"]
#
# [tool.pixi.dependencies]
# bzip2 = "*"
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    original_script = script.read_text()
    script_lock = script.with_name("example.py.pixi.lock")

    verify_cli_command(
        [
            pixi,
            "workspace",
            "export",
            "conda-environment",
            "--script",
            script,
            "--environment",
            "test",
        ],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not support --environment",
            "one implicit default run environment",
        ],
    )
    environment = verify_cli_command(
        [
            pixi,
            "workspace",
            "export",
            "conda-environment",
            "--script",
            script,
        ],
        stdout_contains=["name: default", "bzip2", "requests==2.32.5"],
    )
    assert environment.stdout == snapshot(
        """\
name: default
channels:
- https://prefix.dev/conda-forge
- nodefaults
dependencies:
- bzip2 *
- python *
- pip
- pip:
  - requests==2.32.5

"""
    )

    export_dir = tmp_pixi_workspace / "explicit"
    export_dir.mkdir()
    verify_cli_command(
        [
            pixi,
            "workspace",
            "export",
            "conda-explicit-spec",
            "--script",
            script,
            "--environment",
            "test",
            "--no-install",
            export_dir,
        ],
        ExitCode.FAILURE,
        stderr_contains=[
            "does not support --environment",
            "one implicit default run environment",
        ],
    )
    verify_cli_command(
        [
            pixi,
            "workspace",
            "export",
            "conda-explicit-spec",
            "--script",
            script,
            "--no-install",
            "--ignore-pypi-errors",
            export_dir,
        ]
    )
    explicit_specs = list(export_dir.glob("*_conda_spec.txt"))
    assert len(explicit_specs) == 1
    assert (
        explicit_specs[0]
        .read_text()
        .startswith(
            f"# Generated by `pixi workspace export`\n# platform: {CURRENT_PLATFORM}\n@EXPLICIT\n"
        )
    )
    assert script.read_text() == original_script
    assert not script_lock.exists()

    verify_cli_command([pixi, "lock", "--script", script])
    original_lock = script_lock.read_text()
    locked_export_dir = tmp_pixi_workspace / "explicit-locked"
    locked_export_dir.mkdir()
    verify_cli_command(
        [
            pixi,
            "workspace",
            "export",
            "conda-explicit-spec",
            "--script",
            script,
            "--locked",
            "--no-install",
            "--ignore-pypi-errors",
            locked_export_dir,
        ]
    )
    assert len(list(locked_export_dir.glob("*_conda_spec.txt"))) == 1
    locked_environment = verify_cli_command(
        [
            pixi,
            "workspace",
            "export",
            "conda-environment",
            "--script",
            script,
            "--from-lock-file",
        ],
        stdout_contains=["bzip2", "requests==2.32.5"],
    )
    assert "name: default" in locked_environment.stdout

    assert script.read_text() == original_script
    assert script_lock.read_text() == original_lock
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)


@pytest.mark.slow
def test_pixi_remove_script_uses_explicit_ecosystem(pixi: Path, tmp_pixi_workspace: Path) -> None:
    script = tmp_pixi_workspace / "example.py"
    script.write_text(
        f'''# /// script
# dependencies = ["requests==2.32.5"]
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
#
# [tool.pixi.dependencies]
# bzip2 = "*"
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    script_lock = script.with_name("example.py.pixi.lock")

    verify_cli_command(
        [pixi, "remove", "--script", script, "--no-install", "bzip2"],
        stderr_contains="Removed bzip2",
    )
    assert not script_lock.exists()

    verify_cli_command([pixi, "lock", "--script", script], cwd=tmp_pixi_workspace)
    original_lock = script_lock.read_text()

    verify_cli_command(
        [pixi, "remove", "--script", script, "--no-install", "--pypi", "requests"],
        stderr_contains="Removed requests",
    )

    assert script.read_text() == snapshot(
        f'''# /// script
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["{CONDA_FORGE_CHANNEL}"]
#
# [tool.uv]
# prerelease = "allow"
# ///
print("hello")
'''
    )
    assert script_lock.read_text() != original_lock
    assert not (tmp_pixi_workspace / "pixi.lock").exists()
    assert_no_workspace_state_created(tmp_pixi_workspace)
