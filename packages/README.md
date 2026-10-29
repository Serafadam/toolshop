# Package catalog

Every file in this directory describes one package. toolshop reads all of them at
startup, so adding a package means dropping a new `.toml` file here — no recompilation.

## Where packages are loaded from

Later directories win; a package with the same `name` replaces the earlier definition.

1. the catalog baked into the binary (this directory, via `include_dir!`)
2. `./packages/` relative to the working directory
3. `$XDG_CONFIG_HOME/toolshop/packages/` (usually `~/.config/toolshop/packages/`)
4. `$TOOLSHOP_PACKAGES_DIR`

Malformed files in 2–4 are reported in the log pane and skipped; a malformed file in
the bundled catalog is a startup error.

## Schema

```toml
name = "clangd"           # required, unique, used as the package id
category = "C/C++"        # optional, grouping in the package list (default "Other")
version = ""              # optional, informational
summary = "..."           # optional, one-liner shown in the info window
requires = ["curl"]       # optional, other package names; selected automatically

# Required: at least one install entry. `default` applies to every distro that has
# no entry of its own.
[install.default]
pkg = "clangd"

# Per-distro entries override `default` and may switch install strategy.
[install.jazzy]
pkg = "clangd-18"

[install.lyrical]
script = '''apt-get install -y clangd-20 && clangd-20 --version'''
```

The lookup for a given distro walks three keys, first match wins:

1. `install["<distro name>"]` — e.g. `jazzy`, `24.04`
2. `install["<family>"]` — `ros`, `ubuntu`, `debian` or `fedora`
3. `install["default"]`

So a package can differ per distro, per family, or not at all.

Python packages installed into the image's interpreter need
`uv pip install --system --break-system-packages` (or `pip3 install --break-system-packages`):
Debian and Ubuntu mark `/usr` as externally managed under PEP 668 and refuse the install
otherwise. Prefer a venv under `/opt/toolbox/<package>` when the tool pins its Python
version, as `isaac-sim` does. Some wheels are not on PyPI at all: Isaac Sim's OpenCV fork
only exists on `https://pypi.nvidia.com`, so its recipe adds that index with
`--index-strategy unsafe-best-match` (and `--prerelease=allow` for pinned pre-releases).

Because such an install moves gigabytes, the recipe also exports
`UV_HTTP_TIMEOUT=120 UV_HTTP_RETRIES=10 UV_CONCURRENT_DOWNLOADS=4`. uv's defaults (a 30 s
stall limit, 3 retries, 16 parallel downloads) turn a busy link into
`Failed to download ... operation timed out`, which is a network budget problem rather than a
broken URL. `UV_HTTP_TIMEOUT` is an idle timeout, not a total one, so a big wheel that keeps
streaming is fine.

A package can also export environment variables for the image:

```toml
[env]
UV_BREAK_SYSTEM_PACKAGES = "1"  # `ENV UV_BREAK_SYSTEM_PACKAGES=1`
```

Keys must be valid shell identifiers and values must not contain whitespace; the `ENV`
lines are attributed to the package that declares them, so they highlight with the rest of
its steps. If two packages set the same key the first one wins.

`script` values may use two placeholders:

| placeholder | expands to |
| --- | --- |
| `{{version}}` | the catalog `version`, or the version chosen in the UI (`v` in the info popup) / with `--package-version NAME=VERSION`. A script using it **must** declare a `version` to fall back on. |
| `{{arch:x86_64,arm64}}` | a shell expression that resolves to the first value on x86_64 and the second on arm64, so one recipe works on both (`$(uname -m | grep -qE 'aarch64|arm64' && echo arm64 || echo x86_64)`). Exactly two comma separated values are required. |

Use them for upstream tarballs, which usually differ per architecture and version.

Each `install.<key>` table takes exactly one of:

| key      | meaning                                                     |
| -------- | ----------------------------------------------------------- |
| `pkg`    | package name handed to the distro's package manager (`apt-get`) |
| `script` | shell command run with `RUN` in the image                   |

`pkg` entries are collected into a single `apt-get install` layer; `script` entries run
afterwards, in catalog order, so script installs can rely on the packages they declare
in `requires`.

Make scripts tolerate a tool that is already present in the base image — clearing the
target first (`rm -rf /opt/toolbox/<name>`) or passing the tool's own override (rustup needs
`RUSTUP_INIT_SKIP_PATH_CHECK=yes` when `cargo`/`rustc` are already on `PATH`, otherwise it
aborts with *cannot install while Rust is installed*). A final `--version` check is worth
adding so a broken install fails the build rather than the first session.

Script installs should land under `/opt/toolbox/<package>`. toolshop watches the generated
scripts and only emits the matching `ENV`/`mkdir`/`PATH` lines: a script that mentions
`CARGO_HOME`/`RUSTUP_HOME`, `NVM_DIR` or `/opt/toolbox/fzf` triggers exactly those
directories (`/opt/toolbox` itself is created for any script install and chowned to the
image user when the *user setup* option is on). A distro with nothing selected therefore
produces a bare Containerfile. Keeping tools there rather than in `$HOME` means they
survive toolbox mounting the host home over it. Make scripts **idempotent** (`rm -rf` the
target first) so an image that already contains the tool can still be rebuilt on top of.

Categories are shown as headings in the package list and sorted in this order:
`Core`, `Shell`, `Files`, `Network`, `Git`, `Editors`, `C/C++`, `Rust`, `Python`, `Go`,
`Simulation`.
Anything else is grouped under its own heading after those, alphabetically.

Distro keys match the distro name shown in the UI: the predefined distros
(`jazzy`, `kilted`, `lyrical`, `24.04`, `22.04`, `41`) or the name derived from a local
image reference (for `localhost/ros:jazzy-dev` that is `jazzy`). Keys containing dots
need quoting in TOML (`[install."24.04"]`).

Family keys describe the distro *type*:

| key      | distros                                            | package manager |
| -------- | -------------------------------------------------- | --------------- |
| `ros`    | ROS images (`ros:jazzy-ros-base`, …)                | `apt-get`       |
| `ubuntu` | Ubuntu base images (`ubuntu:24.04`, …)              | `apt-get`       |
| `debian` | Debian base images (`debian:12`, …)                 | `apt-get`       |
| `fedora` | Fedora base images (`fedora:41`, …)                 | `dnf`           |
| `other`  | anything else, including images added by hand       | `apt-get` (assumed) |

A package with only a `default` entry is selectable on every distro.
