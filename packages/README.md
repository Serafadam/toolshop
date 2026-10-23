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

Script installs should land under `/opt/toolbox/<package>` — toolshop pre-creates the
`suggested` directories (`CARGO_HOME=/opt/toolbox/cargo`, `RUSTUP_HOME=/opt/toolbox/rust`,
`NVM_DIR=/opt/toolbox/nvm`, `/opt/toolbox/fzf/bin`, all chowned to the image user) and adds
the relevant `bin` paths to `PATH`. Keeping tools there rather than in `$HOME` means they
survive toolbox mounting the host home over it. Make scripts **idempotent** (`rm -rf` the
target first) so an image that already contains the tool can still be rebuilt on top of.

Categories are shown as headings in the package list and sorted in this order:
`Core`, `Shell`, `Files`, `Network`, `Git`, `Editors`, `C/C++`, `Rust`, `Python`, `Go`.
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
