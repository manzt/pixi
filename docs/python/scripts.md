# Running Python scripts

Pixi can run a local Python script using dependency metadata embedded in the
file. It creates an isolated environment for the script, independent of an
enclosing Pixi workspace or active Pixi environment.

## Inline script metadata

Pixi uses [PEP 723](https://peps.python.org/pep-0723/), the Python standard for
[inline script
metadata](https://packaging.python.org/en/latest/specifications/inline-script-metadata/).
PEP 723 stores TOML in a comment block so the file remains valid Python. The
block can contain a Python requirement, Python dependencies, and namespaced
tool configuration.

Pixi applies `pyproject.toml` semantics to the fields it supports. Standard
Python metadata stays at the root of the block, while Pixi configuration lives
under `tool.pixi`.

Initialize a new script, or add metadata to an existing one, with:

```console
$ pixi init --script example.py
```

Pixi preserves an existing shebang and Python body. The generated block records
the configured default channels, the current platform, and the same minimum
Python version used by a new `pyproject.toml`:

```python title="example.py"
# /// script
# requires-python = ">= 3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
# ///

print("Hello world")
```

The platform written by `pixi init --script` matches the machine on which the
command runs. The `dependencies` field is present even when it is empty.

## Declaring dependencies

PEP 723's root `dependencies` array contains standard Python package
requirements. Add a PyPI dependency with `--pypi`:

```console
$ pixi add --script example.py --pypi "requests>=2"
```

Pixi records the requirement in `dependencies`:

```python
# /// script
# requires-python = ">= 3.11"
# dependencies = ["requests>=2"]
# ///
```

### Pixi-specific metadata

PEP 723 allows tools to store their own configuration in the `tool` table. Pixi
reads `tool.pixi`. These fields retain the same meaning they have in the
`tool.pixi` section of a `pyproject.toml`.

For example, conda dependencies are a Pixi capability and are stored under
`tool.pixi.dependencies`:

```console
$ pixi add --script example.py "rich>=14,<15"
```

```python
# /// script
# requires-python = ">= 3.11"
# dependencies = ["requests>=2"]
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
#
# [tool.pixi.dependencies]
# rich = ">=14.0,<15"
# ///
```

Remove dependencies through the same distinction:

```console
$ pixi remove --script example.py rich
$ pixi remove --script example.py --pypi requests
```

`pixi add --script` initializes the metadata block when it is absent. `run`,
`lock`, and `remove` require an existing block and suggest `pixi init --script`
when it is missing.

### Standard and tool-specific fields

`requires-python` and the root `dependencies` array are defined by PEP 723.
Other tools that implement PEP 723 can read these fields, although their
environment and command behavior may differ from Pixi.

`tool.pixi` is interpreted only by Pixi. A script that relies on conda packages
or other Pixi configuration requires Pixi to reproduce the same environment.
Pixi does not interpret configuration in other `tool` tables, but preserves it
when editing the metadata block.

## Running a script

Script mode is always explicit:

```console
$ pixi run --script example.py
Hello world
```

Arguments after the script path are passed to Python:

```console
$ pixi run --script example.py first --second
```

The script is independent from any enclosing Pixi workspace and from an active
Pixi environment. Pixi installs its environment in the execution cache.
Relative paths in the metadata are resolved from the script's directory, while
the Python process runs in the directory from which `pixi run` was invoked.

## Locking dependencies

Scripts must be locked explicitly:

```console
$ pixi lock --script example.py
```

This writes `example.py.pixi.lock` next to the script. Once the lock exists,
`run`, `add`, and `remove` reuse it and update it when necessary.

Without an adjacent lock, `run` and `add` resolve in memory without creating
one, and `remove` only edits the metadata.

## Supported Pixi configuration

A script is modeled as a restricted `pyproject.toml` with one implicit default
environment:

- `requires-python` and `dependencies` use standard Python semantics.
- `tool.pixi.workspace`, conda dependencies, activation, system requirements,
  PyPI options, and target-specific configuration retain their Pixi semantics.
- Tasks, named features, environments, solve groups, and package or build
  configuration are rejected.
- Unknown `tool` tables are ignored and preserved.

Only local script paths are currently supported. Reading a script from standard
input or a remote URL is not supported.
