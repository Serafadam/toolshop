# toolshop

A terminal UI for assembling ROS development containers. Pick a distro, tick the tools
you want, and toolshop writes a Containerfile and runs `podman build` for you while
streaming the output in a log pane. Every build records a manifest, so the **Toolboxes**
tab can show what you built, what is inside it, and whether the image is still there.

```
┌Distro────────┐┌Packages (3/18)─┐┌Containerfile — docker.io/library/ros:jazzy-ros-base┐┌Logs─────────┐
│> jazzy       ││> clangd (clan… ││FROM docker.io/library/ros:jazzy-ros-base         ││Building from… │
│  kilted      ││  clang-tidy (… ││ENV DEBIAN_FRONTEND=noninteractive                ││STEP 1/3: …    │
│  lyrical     ││  cmake         ││RUN apt-get update && apt-get install -y --no-…   ││               │
└──────────────┘└────────────────┘└──────────────────────────────────────────────────┘└───────────────┘
```

## Running

```sh
cargo run            # the TUI
cargo run -- --help  # the headless CLI
```

## CLI

Everything the TUI does is also scriptable — no terminal needed, which is what CI uses:

```sh
toolshop --print -d jazzy -p htop,cppcheck,fzf --user ci > Containerfile
toolshop --print -d ros:kilted-ros-base -a            # every package, by image name
toolshop --list-packages | grep -i clang
toolshop --list-distros                               # predefined + local images
toolshop --theme macchiato                            # or latte / frappe / mocha
```

| option | meaning |
| --- | --- |
| `--print` | write the generated Containerfile to stdout and exit (warnings go to stderr) |
| `-d`, `--distro <NAME>` | distro for `--print`: name (`jazzy`) or image (`ros:kilted-ros-base`) |
| `-p`, `--package <NAME>` | package to install; repeatable, comma separated allowed |
| `-a`, `--all` | select every package the distro offers |
| `-u`, `--user <NAME>` | user the image is prepared for (lands in `ARG DOCKER_USER` and the build arg) |
| `--package-version <PKG=VERSION>` | pin a package version, repeatable, e.g. `--package-version neovim=0.11.2` |
| `--env <NAME=VALUE>` | extra `ENV` line in the generated file, repeatable |
| `--list-packages`, `--list-distros` | plain-text listings |
| `--theme <FLAVOUR>`, `--no-config` | pick a flavour / ignore the saved theme and configuration (`--no-theme-config` still works) |
| `-h/--help`, `-V/--version` | usage and version |

`--print` output is self-contained: `ARG DOCKER_USER` defaults to the selected user, so
`toolshop --print -p htop > Containerfile && podman build -f Containerfile .` builds the
same image the app would.

### Build options

The block above the distro list holds the checkboxes that control the infrastructure the
generated file adds. They are on by default and toggle with `Enter` (or a mouse click),
which makes the Containerfile minimal:

| option | what it adds | off means |
| --- | --- | --- |
| `user setup (uid 1000)` | `ARG DOCKER_USER`, `chown -R 1000:1000 /opt/toolbox`, and the step that renames/creates uid+gid 1000 as your user (`toolbox create` needs it) | no user handover — handy for a plain image you run as root |
| `hostname fix (nss)` | `libnss-myhostname` plus the one-line `nsswitch.conf` edit, so `sudo` stops saying *unable to resolve host* (apt families only) | no extra package; `sudo` may warn inside the container |
| `hide username` | records the base image's `ubuntu` placeholder instead of your account name | the real account goes into `ARG DOCKER_USER` (and the build arg) |
| `user: <name>` | `Enter` opens a prompt to record another name; an empty value returns to the host account | — |
| `toolbox: <name>` | `Enter` names the toolbox container created after the next successful build (same prompt as `n`); shows `(auto)` when toolbox picks the name | — |
| `local images` | lists the images found on this machine under their distro family (marked `(local)`) | only the predefined distros and your own added images are listed; a selected local image hands the builder back to the first remaining distro |
| `env: …` | `Enter` opens a prompt for extra environment variables: `NAME=VALUE` adds or replaces one, a bare `NAME` removes it, `-` clears them all | — |
| `reset last configuration …` | `Enter` deletes the saved state (see [The last configuration](#the-last-configuration)) and clears the selection | — |

`hide username` / `user: …` exist for recording: a Containerfile you commit, paste into a
ticket or screenshot carries no personal account name. `--hide-username` does the same for
`--print`, and the real name is still passed at build time via `--build-arg`, so the image
behaves identically.

Everything else is derived from the selection rather than configured: package-manager
packages go into one layer, script packages into their own `RUN` under `/opt/toolbox`, and
the `ENV`/`mkdir`/`PATH`/`chown` lines appear only for the toolchains you picked — and they
are attributed to those packages, so with the cursor on `rust` the Containerfile highlights
`ENV CARGO_HOME`, `ENV RUSTUP_HOME`, their `PATH` entry, their directories and the `chown`
exactly like the install step (shared lines such as `ENV PATH` are owned by every package
behind them).

### Extra environment variables

The `env:` row in the options block adds `ENV` lines of your own — handy for things that are
not package business, like `CUDA_VISIBLE_DEVICES`, `HTTP_PROXY` or a prompt. `Enter` opens a
prompt whose title spells the syntax out:

* `NAME=VALUE` adds the variable, or replaces it when the name is already there;
* a bare `NAME` removes it;
* `-` clears every variable added this way.

```
ENV FOO=bar
ENV GREETING="hello world"
ENV DOLLAR="\$HOME"
```

Values are written verbatim except for what the Containerfile format would misinterpret:
anything with spaces, quotes, `$`, `#`, `;`, `&` or redirections is wrapped in double quotes
with `"`, `\` and `$` escaped, so `$HOME` stays literal. Names are validated
(`[A-Za-z_][A-Za-z0-9_]*`) and values must fit on one line, since a newline would end the
instruction.

Your lines are emitted **after** the packages' own `[env]` tables, so an explicit value wins
over a package's default (`ENV UV_BREAK_SYSTEM_PACKAGES=0` beats the one `uv` exports). They
belong to no package, so nothing is highlighted for them, and they are part of the saved
configuration, so they survive a restart and go away with `reset last configuration …`.

`--env NAME=VALUE` does the same for `--print`:

```console
$ toolshop --print -d jazzy -p htop --env CUDA_VISIBLE_DEVICES=0 | grep CUDA
ENV CUDA_VISIBLE_DEVICES=0
```

### The last configuration

Everything that describes *what you picked* — the distro, the selected packages, the versions
chosen with `v`, the install commands you replaced, the build options and the folded
categories — is written to

```
$XDG_CONFIG_HOME/toolshop/state.toml      # ~/.config/toolshop/state.toml
```

after every change (and skipped when nothing changed, so the file is not rewritten on every
keystroke). The next start continues where you left off:

```
restored the last configuration: distro docker.io/library/ubuntu:24.04, 4 packages, 1 version choice(s)
dropped from the saved configuration: neovim-removed
```

Anything the catalog no longer knows — a package that was renamed, a version for a package
that was dropped, an image that is not on this machine — is skipped and named in the log
instead of breaking the start. The filter is a view, not part of the configuration, and is
not saved.

`reset last configuration …` in the Options block (below `local images`) deletes that file and
puts everything back to defaults: no distro, no packages, no versions, no overrides, the
default build options. `--no-config` disables reading and writing it for one run.

### Filtering packages

`/` opens a search box; the list narrows as you type, matching package names, their categories
and their summaries (case-insensitive), and the pane title shows what is shown and why:

```
╭ Packages (2/73 · "rust") — Esc clears ─╮
```

Categories whose packages all filter out disappear with their heading, so a search never
leaves empty groups behind; folding, the cursor, `Enter` and `Ctrl+A` all act on what is
visible. `Esc` inside the box throws the search away (the list comes back whole), `Enter`
keeps it, and `Esc` in the builder clears an active filter before it quits.

### Simulators

`gazebo` (gz-sim, formerly Ignition), `isaac-sim`, `mujoco` and `genesis` live in the
**Simulation** category. `vulkan-tools`, `nvidia-vulkan-icd` and `zenity` live in
**Graphics**: the first two make a containerised GPU usable for Vulkan, the last is what Isaac
Sim shows its dialogs with. Recipes come from upstream and are pinned:

| package | source | notes |
| --- | --- | --- |
| `ignition-gazebo` | `ros-<distro>-ros-gz`, else OSRF's repository | ROS, Ubuntu and "other" apt images |
| `mujoco` | release tarball + PyPI | checksum verified, `simulate` viewer on `PATH` |
| `isaac-sim` | `isaacsim[all]==6.2.0.0` from PyPI | several GB, own Python 3.12 venv, x86_64/aarch64, needs a GPU. Sets `OMNI_KIT_ACCEPT_EULA=YES` — Isaac Sim refuses to run until the Omniverse EULA is accepted |
| `isaac-sim-cache` | `isaacsim[extscache]==6.2.0.0` | optional 7.4 GB extension cache so the image runs offline instead of downloading it on first launch; requires `isaac-sim` |
| `genesis` | `genesis-world` from PyPI | pure-Python wheel, installs into the system interpreter (`--break-system-packages`, see PEP 668) |

Isaac Sim resolves through NVIDIA's extra index (its OpenCV fork is not published on PyPI)
with pre-releases allowed, and the install widens uv's HTTP budgets
(`UV_HTTP_TIMEOUT=120 UV_HTTP_RETRIES=10 UV_CONCURRENT_DOWNLOADS=4`) — with the defaults a
slow or busy link fails after a 30 s stall, which is what "Failed to download
`isaacsim-ros1`" looks like.

`--print` (and the TUI) resolve a package's `requires`, so picking `isaac-sim` also installs
`uv`, and scripts are emitted in dependency order — `uv` is installed before the simulators
that call it.

### GPU access

Simulators need the host GPU. Toolbox containers get it from the container runtime
configured on the host, not from flags: toolshop cannot pass any — every `toolbox create`
release parses its first positional argument as the container name, so
`toolbox create -- --device …` dies with

```
Error: invalid argument for 'CONTAINER'
Container names must match '[a-zA-Z0-9][a-zA-Z0-9_.-]*'
```

(the name it validates is `--device`). GPU access therefore comes from the host's
`nvidia-container-toolkit` / runtime hook, which mounts `/dev/nvidia*` and copies the
driver's user-space libraries into the container on every start. Check it with
`nvidia-smi` inside the container.

Those copied libraries cover CUDA but not Vulkan: the manifest in `/usr/share/vulkan/icd.d`
is what tells the loader where the driver is, and it is not copied. That is the whole story
behind

```
VkResult: ERROR_INCOMPATIBLE_DRIVER
vkCreateInstance failed. Vulkan 1.1 is not supported, or your driver requires an update.
Failed to create any GPU devices, including an attempt with compatibility mode.
[omni.kit.renderer.plugin] GPU Foundation is not initialized!
```

— the GPU is right there (CUDA and `nvidia-smi` work), but Vulkan cannot find it. Three
packages fix the image side, and `vulkan-tools` proves it:

| package | what it adds |
| --- | --- |
| `vulkan-tools` | `vulkaninfo` plus `libvulkan1`, the loader itself |
| `nvidia-vulkan-icd` | `/usr/share/vulkan/icd.d/nvidia_icd.json`, pointing at the driver library the runtime injects |
| `zenity` | the dialogs Isaac Sim shows (`sh: 1: zenity: not found`) |

```console
$ vulkaninfo --summary | head -20        # "deviceName = NVIDIA …" means Vulkan sees the GPU
```

One runtime pitfall needs the container to be **recreated**, not restarted:

* A toolbox created before a driver update keeps the old driver copies: the container here
  held `libnvidia-glvkspirv.so.595.80` next to `libcuda.so.615.78.08`, and the ICD refused to
  initialise (`vk_icdNegotiateLoaderICDInterfaceVersion` → `VK_ERROR_INITIALIZATION_FAILED`).
  Remove the toolbox and create it again.

If the image a toolbox was created from contains `isaac-sim` or `ignition-gazebo` without
`nvidia-vulkan-icd`, toolshop says so right after `toolbox create` — the driver is there, the
manifest that points the loader at it is not.

For plain `podman` (outside toolbox) the device has to be named explicitly, which is what
CDI is for:

```console
$ sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
$ podman run --device nvidia.com/gpu=all --env NVIDIA_DRIVER_CAPABILITIES=all \
      --rm ubuntu24 nvidia-smi
```

### Package versions

Packages whose install command can take a version (currently the script-installed ones such
as `neovim`) expose `{{version}}` in their recipe. Pick one with `v` in the info popup or
with `--package-version NAME=VERSION`; it changes the generated command, the recorded
manifest (`version` per package) *and* the image tag, so two versions never overwrite each
other:

```console
$ toolshop --print -p neovim --package-version neovim=0.11.2 | grep download/v
    && wget -qO- https://github.com/neovim/neovim/releases/download/v0.11.2/nvim-linux-$(uname -m | grep -qE 'aarch64|arm64' && echo arm64 || echo x86_64).tar.gz \
```

### arm64

Recipes that fetch upstream tarballs use `{{arch:x86_64,arm64}}`, which resolves the asset
name at build time from `uname -m` — so the same package builds on x86_64 and arm64 hosts
(`neovim`, `lazygit`, `mujoco`). MuJoCo also fetches the release's published `.sha256`
next to the tarball, so the checksum matches whichever architecture was downloaded. Where
an upstream project only ships x86_64 (Isaac Sim), the recipe fails fast with an explicit
message instead of downloading the wrong thing.

## Tabs

* **Builder** — the options block (build checkboxes, user name, toolbox name), distro
  list, package list and the generated Containerfile. The Containerfile is generated from what you actually selected: a bare
  distro yields a bare file, and toolchain directories (`CARGO_HOME`, `RUSTUP_HOME`,
  `NVM_DIR`, `/opt/toolbox/fzf/bin`) only appear when the package that uses them is added. With the
  cursor on a *selected* package, the lines installing it are highlighted in the
  Containerfile (mauve band) together with the lines of everything it requires (peach),
  so you can see what a package actually costs. This is display-only: the file written for
  the build is plain text with no styling.
* **Toolboxes** — toolshop-built images, what they contain, and whether they still exist
  locally.
* **Logs** — the build log, full window (it no longer shares space with the builder):
  `Tab` cycles to it, or press `L` to jump there from the Builder and `L` again to come
  back. `↑`/`↓` (or `j`/`k`) scroll it; it still follows the tail while scrolled to the
  bottom.

## Keys

| key            | action                                            |
| -------------- | ------------------------------------------------- |
| `Tab` / `Shift+Tab` | next / previous tab                          |
| `L`            | jump to the full-window Logs tab, and back        |
| `t`            | cycle the Catppuccin flavour (Latte → Frappé → Macchiato → Mocha) |
| `←`/`→`, `h`/`l` | move focus between panes, following the layout (options → distro list → packages → Containerfile and back) |
| `↑`/`↓`, `k`/`j` | move the cursor inside the focused pane (scrolls the log when focused) |
| `Enter`        | select the distro / toggle the package under the cursor / fold-unfold a package category |
| `z`            | fold every package category at once — press again to unfold them all |
| `Ctrl+A`       | select every package the distro offers (press again to clear) |
| `i`            | open the package info window (`Esc` closes it, then quits) |
| `m` / `e` / `r` (info open) | switch install method / edit the install command / reset both the override and the chosen version |
| `v` (info open) | choose the version of the package (scripts that take one, e.g. neovim) |
| `/`          | filter the package list by name, category or summary — matches as you type |
| `Esc`        | in the builder: clear an active filter first, then quit |
| `Enter` on the Options block | toggle the checkbox under the cursor, including `local images`, or run `reset last configuration …` (mouse click works too) |
| `n`            | name the toolbox the next build also creates (Builder) |
| `a`            | add your own base image (Builder)                 |
| `d`            | remove a user-added image (Builder) / forget a manifest (Toolboxes) |
| `Backspace`    | start a build, or cancel a running one            |
| `R`            | rescan local images (Builder) / toolboxes (Toolboxes) |
| `PgUp`/`PgDn`  | scroll the toolbox details                        |
| `c`            | create a toolbox container from the selected image — asks for the container name (Toolboxes) |
| `x`            | remove that image's toolbox containers (Toolboxes) |
| `d`            | forget the manifest of the selected toolbox (Toolboxes) |
| `q`, `Esc`, `Ctrl+C` | quit                                        |

### Mouse

* click a tab in the top bar to switch view;
* click a list row to focus that pane and move the cursor there; clicking the row the
  cursor is already on activates it (select the distro, toggle the package). Category and
  family headings are not clickable;
* the wheel scrolls whatever is under the pointer: list cursors in the Builder, the
  Containerfile pane (its title then reads `… · line 7`), the toolbox list or its details,
  and the log in the Logs tab;
* clicking outside the info popup closes it; while a prompt is open the mouse is ignored.

The renderer records the pane/tab rects in `App::hit` each frame, so hit testing always
matches what is on screen (including list scroll offsets).

Packages whose requirements are pulled in are selected automatically, and dropped
again when nothing else needs them. `Ctrl+A` selects every package available on the
selected distro (requirements included); pressing it again clears the selection, which
also drops the install overrides of the removed packages.

### Install overrides

Every package installs with what its catalog entry says, but any package can be overridden
per build. With the info window open (`i`):

* `m` flips between **package manager** and **script**. Switching to a script seeds it with
  a ready-to-run command (`apt-get update && apt-get install -y --no-install-recommends …`
  or the `dnf` equivalent); switching back keeps the package name.
* `e` opens an input window to type the command (pre-filled with the current one).
* `r` drops the override and falls back to the catalog entry.

Overridden packages are highlighted in the info window, appear in the generated
Containerfile as a `RUN` step instead of the shared package layer, and the manifest records
the command that actually ran.

The info window shows the command that will *run*, not the recipe: `{{version}}` and
`{{arch:…}}` are expanded, so picking a version (`v`) visibly rewrites the line, and a chosen
version is marked `(chosen, r resets)` — `r` puts both the install override and the version
back to the catalog entry. The input overlays these keys open are drawn **on top** of the
info window, and the footer switches to `Enter confirm · Esc cancel · Ctrl+U clear` while one
is open.

The package pane is grouped by category and every category can be folded shut (they start
expanded): `Enter` on a heading — or a click on it — hides its packages and shows
`▸ C/C++ (9)` instead of `▾ C/C++ (9)`, the count always reporting the packages the distro
offers. `↑`/`↓` walk headings too, so a folded category can always be reopened, and `z`
folds *every* category at once (pressing it again unfolds them all) — handy once a catalog
grows past a screenful. The fold state is kept per category name, so it survives switching
distro or rescanning images.

The package pane is grouped by category (`Core`, `Shell`, `Files`, `Network`, `Git`,
`Editors`, `C/C++`, `Rust`, `Python`, `Go`, `Graphics`, `Simulation`). `i` opens a
small description window for the package under the cursor: summary, category, version,
how it installs on the selected distro (`apt-get: htop`, `dnf: …`, `shell script`),
requirements and whether it is currently selected.

The chrome is a tab bar on top and a footer with context-dependent key hints plus the
build state.

### Build progress

`App::build` (`BuildProgress`) parses podman's output as it arrives, so the build is
visible without reading the log:

* **tab bar** — the Logs tab turns into `Logs 75%` while a build runs;
* **footer** — a `Gauge` with `STEP 2/5 · 12s · 1 warning` as its label, plus one glyph
  per fact: `●` running, `▲` warnings, `✖` errors, `⚑` a toolbox name is pending;
* **log title** — `Logs — STEP 2/5 · 12s · 1 warning (0 up)`;
* when the build ends the gauge is replaced by a summary: `done · 12s`, `failed · 8s ·
  1 error` (red) or `cancelled · 3s` (yellow), and the failure keeps the error marker.

Step counts come from `STEP n/m:` lines, so the bar is a real percentage rather than a
guess; before the first step (and when a step count is unavailable) it shows a minimal
filled sliver. The parser only counts warnings/errors while a build is active, so
unrelated log lines never skew the indicators.

**Failures are made obvious**, in three layers:

* log lines are coloured by severity — `STEP …` in sapphire, `warning` lines yellow,
  `Error:`/`level=error` red and bold, `Build finished` green;
* the `Build failed: …` line itself is drawn as a red-on-`surface0` banner so it stands
  out from the surrounding output;
* when a build fails the app switches to the **Logs** tab automatically and scrolls back to
  the tail, so the failure is on screen without pressing anything. A successful build
  never changes the view.

The footer keeps the red `failed · 8s · 1 error` summary and the `✖` marker afterwards.


## What the generated Containerfile does

| line | why |
| --- | --- |
| `ARG DOCKER_USER` + a `getent`/`usermod`/`useradd` step | uid **and** gid 1000 end up owned by *your* user: renamed if the base image owns it (`ubuntu` images do), created if it does not. `toolbox create` runs `useradd --uid 1000 <host user>` and fails with `UID 1000 is not unique` when the image keeps 1000 under another name |
| `ENV TOOLBOX_PREFIX=/opt/toolbox` + `CARGO_HOME`/`RUSTUP_HOME`/`NVM_DIR` below it | everything script-installed lives outside `$HOME` (toolbox mounts the host home over it) and is grouped by package name: `/opt/toolbox/{cargo,rust,nvm,fzf}` |
| `ENV PATH=…:${CARGO_HOME}/bin:${RUSTUP_HOME}/bin:${NVM_DIR}/node/bin:/opt/toolbox/fzf/bin:$PATH` | the toolchains are usable without sourcing anything |
| `ENV UV_BREAK_SYSTEM_PACKAGES=1` | lets `uv` install into the externally managed system interpreter (PEP 668) |
| `RUN mkdir -p ${CARGO_HOME} ${RUSTUP_HOME} ${NVM_DIR} /opt/toolbox/fzf/bin` | the install scripts write there |
| package `[env]` tables | a package can export environment variables for the image (`uv` → `UV_BREAK_SYSTEM_PACKAGES=1`, `isaac-sim` → `OMNI_KIT_ACCEPT_EULA=YES`) |
| `libnss-myhostname` + a one-line `nsswitch.conf` edit | Debian/Ubuntu bases could not resolve their own hostname, so `sudo` greeted you with `unable to resolve host toolbx`. The module answers for whatever hostname toolbox gives the container, and the build proves it with `getent hosts "$(hostname)"`. Fedora ships it already, so this step is apt-only |
| `RUN chown -R 1000:1000 /opt/toolbox` | the whole tree is writable for the user |

The script recipes are re-runnable: each clears its target under `/opt/toolbox/<package>`
before installing, and rustup additionally runs with `RUSTUP_INIT_SKIP_PATH_CHECK=yes` so a
base image that already ships a Rust (e.g. an older image with `/opt/cargo/bin`) does not
abort the build with *cannot install while Rust is installed*.
| `SHELL ["/bin/bash", "-c"]` | install scripts may use bash syntax |
| `RUN yes \| unminimize` | restores man pages, only when `man`/`manpages` are selected (Ubuntu/ROS) |


`--build-arg DOCKER_USER=$USER` is passed automatically at build time.

## Packages

Packages live in TOML files, **not** in the source. The catalog in
[`packages/`](packages/) is compiled into the binary, and can be extended or overridden
without rebuilding — see [`packages/README.md`](packages/README.md) for the schema.

Load order (later wins):

1. the bundled catalog (`packages/` in this repo)
2. `./packages/`
3. `~/.config/toolshop/packages/`
4. `$TOOLSHOP_PACKAGES_DIR`

Each package declares how to install itself per distro, so the same package can use a
different package-manager name or even switch between `apt-get` and a shell script on
different distros:

```toml
name = "clangd"
category = "C/C++"
summary = "C/C++ language server"

[install.default]
pkg = "clangd"

[install.jazzy]
pkg = "clangd-18"

[install.lyrical]
pkg = "clangd-20"
```

## Distros

Distros are grouped by family, and the family decides how packages are installed:

| family   | predefined                     | install layer                      |
| -------- | ------------------------------ | ---------------------------------- |
| `ROS`    | `jazzy`, `kilted`, `lyrical`   | `apt-get update && apt-get install` |
| `Ubuntu` | `24.04`, `22.04`               | `apt-get update && apt-get install` |
| `Debian` | —                              | `apt-get update && apt-get install` |
| `Fedora` | `41`                           | `dnf install`                       |

On startup (and on `R`) toolshop asks the local container engine (`podman`, then `docker`)
for every image it has — `podman images --format '{{.Repository}}:{{.Tag}}'` — and lists
them under their family as `<image> (local)`. Nothing is hidden: images whose family cannot
be detected (Alpine, Arch, your own images, …) are listed under **`Other`**, which assumes
Debian-style `apt-get`; dangling `<none>:<none>` entries are skipped.

Press `a` to add an image by hand: type the reference, pick the type with `Tab`/`←→`
(`ROS`, `Ubuntu`, `Debian`, `Fedora`, `Other`), `Enter`. User images are marked
`(custom)`, become the current selection immediately, and are persisted in
`~/.config/toolshop/images.toml` (`$TOOLSHOP_CONFIG_DIR` overrides the directory), so they
survive restarts. `d` in the distro pane removes the selected user image.

```toml
[[image]]
image = "localhost/ubuntu24"
family = "ubuntu"
name = "dev"
```

The distro *name* used for package lookups comes from the image: the ROS codename
(`ros:jazzy-ros-base` → `jazzy`), the version tag (`ubuntu:24.04` → `24.04`).
Package install specs are looked up by distro name, then by family, then `default`.

## Toolboxes and manifests

Each build is tagged `toolshop:<distro>-<hash>` (the hash covers the base image and the
resolved package set, so the same selection always rebuilds the same tag) and recorded in
the cache:

```
$XDG_CACHE_HOME/toolshop/manifests/      # ~/.cache/toolshop/manifests
└── toolshop-jazzy-799fcc.toml
```

A manifest holds the image reference, the image ID at build time, the distro, the
resolved install for every package (`pkg = "clangd-18"` or the script that ran), the
build time and the toolshop version:

```toml
version = 1
image = "localhost/toolshop:jazzy-799fcc"
image_id = "sha256:deadbeefcafe1234"
created_at = 1792760508
tool_version = "0.1.0"

[distro]
name = "jazzy"
image = "docker.io/library/ros:jazzy-ros-base"
family = "ros"

[[packages]]
name = "clang-tidy"
pkg = "clang-tidy-18"
```

The tab lists two kinds of entries:

* **toolshop builds** — every manifest in the cache, newest first.
* **external toolboxes** — images that have toolbox containers but no manifest, i.e.
  things this tool did not create (your old `create_toolbox_ws.sh` images, for example).
  They are marked `external` in the list and their details pane says the package list is
  unknown, but you can still `c`reate and `x`-remove their containers.

The Toolboxes tab compares each manifest against what the engine currently reports
(`podman images`), which yields three states:

| state      | meaning                                                     |
| ---------- | ----------------------------------------------------------- |
| `present`  | the image is still there and its ID matches the manifest    |
| `modified` | the tag exists but its ID changed — rebuilt outside toolshop |
| `missing`  | the image is gone; the manifest still documents the build   |

`unknown` is shown when the engine cannot be queried at all. `d` deletes only the
manifest (use `podman rmi` to remove the image itself) — if the image still has toolbox
containers it stays in the list as an external entry.

Containers are created with the host GPU when a CDI spec exists (see
[GPU access](#gpu-access)); the log line says `with GPU access`, or explains how to enable it
when the host has an NVIDIA device but no spec yet.

The tab also runs `toolbox list --containers` (when `toolbox` is installed) and shows
which toolbox containers were created from each image, so the
`podman build` → `toolbox create --image …` workflow is visible in one place — Two ways to name the final toolbox:

* **Builder tab** — press `n` and type the name. It is shown in the footer (`toolbox: dev`),
  and the next successful `Backspace` build then runs
  `toolbox create --container <name> --image <just-built image>` for you. The name is used
  once (a failed build keeps it for the next attempt), and the Toolboxes tab refreshes.
* **Toolboxes tab** — `c` opens the same prompt for an image that already exists and
  creates the container immediately.

Both prompts are pre-filled with a suggestion derived from the image; it is selected, so the
first keypress replaces it and `Ctrl+U` clears the field. `Enter` confirms, `Esc` cancels., `x` runs `toolbox rm --force …` on the
selected image's containers:

```
Image:      localhost/toolshop:jazzy-799fcc
Distro:     jazzy (ros) from docker.io/library/ros:jazzy-ros-base
Created:    12m ago (1792760508)
Status:     present — image matches the manifest
Containers: dev, scratch
Manifest:   /home/you/.cache/toolshop/manifests/toolshop-jazzy-799fcc.toml

Packages (3):
  clang-tidy          pkg: clang-tidy-18
  htop                pkg: htop
  rust                script
```

Set `TOOLSHOP_CACHE_DIR` to keep manifests somewhere else.

## Layout

| file                      | purpose                                                    |
| ------------------------- | ---------------------------------------------------------- |
| `src/main.rs`             | startup, the single `tokio::select!` event loop             |
| `src/app.rs`              | application state and state transitions                     |
| `src/catalog.rs`          | TOML package catalog: loading, validation, per-distro specs |
| `src/manifest.rs`         | build manifests: schema, cache directory, hashing           |
| `src/toolbox.rs`          | manifests matched against the engine's current images        |
| `src/engine.rs`           | podman/docker queries (images, image IDs)                   |
| `src/distro.rs`           | predefined distros + local image discovery                  |
| `src/container_builder.rs`| Containerfile generation and the podman build task          |
| `src/event.rs`            | terminal event reader thread (crossterm → async channel)    |
| `src/ui.rs`               | rendering                                                   |
| `src/update.rs`           | key bindings                                                |
| `src/tui.rs`              | terminal setup/teardown                                     |

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt
```

