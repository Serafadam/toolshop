use std::{collections::BTreeSet, fmt, time::Duration};

use color_eyre::{
    Result,
    eyre::{WrapErr, bail, eyre},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    sync::mpsc::Sender,
    task::{self, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use crate::{
    catalog::{
        InstallOverride, InstallSpec, Package, effective_version, expand_script, resolve_install,
    },
    distro::{Distro, DistroFamily, PackageManager},
    engine,
    manifest::Manifest,
};

/// Containerfile written before invoking the build.
pub const CONTAINER_FILE: &str = "container";

/// How often the child process is polled while waiting for it to finish.
const BUILD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long to wait for the build's output pipes to close after it exits.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Default name of the unprivileged user images are set up for.
pub const DEFAULT_DOCKER_USER: &str = "ubuntu";

/// Everything the script installs put on the image lives here, outside the user's home
/// (so toolbox mounting the host home does not hide it) and grouped by package name.
const TOOLBOX_PREFIX: &str = "/opt/toolbox";

/// Toolchain directories the generated image exposes to its user, mirroring the
/// hand-written Containerfiles this tool replaces.
const CARGO_HOME: &str = "/opt/toolbox/cargo";
const RUSTUP_HOME: &str = "/opt/toolbox/rust";
const FZF_PREFIX: &str = "/opt/toolbox/fzf";
const NVM_DIR: &str = "/opt/toolbox/nvm";

/// Debian/Ubuntu images cannot resolve their own hostname (toolbox names the container,
/// so `sudo` greets you with `unable to resolve host toolbx`). `nss-myhostname` answers for
/// the local hostname without needing an `/etc/hosts` entry that a build cannot know.
const HOSTNAME_PACKAGE: &str = "libnss-myhostname";

/// Enables the module in `nsswitch.conf` and proves it works at build time.
const HOSTNAME_STEP: &str = r#"
RUN sed -i -E 's/^(hosts:[[:space:]]+)/\1myhostname /' /etc/nsswitch.conf \
    && getent hosts "$(hostname)" >/dev/null
"#;

/// Normalises the container user so `toolbox create` can reuse it instead of failing with
/// `useradd: UID 1000 is not unique`.
const USER_STEP: &str = r#"
RUN set -eu; target="${DOCKER_USER}"; \
    if command -v getent >/dev/null; then \
        owner="$(getent passwd 1000 | cut -d: -f1 || true)"; \
        if [ -n "$owner" ] && [ "$owner" != "$target" ]; then \
            groupmod -n "$target" "$(id -gn "$owner")" 2>/dev/null || true; \
            usermod -l "$target" -d "/home/$target" -m "$owner"; \
        fi; \
        if ! getent passwd "$target" >/dev/null; then \
            getent group 1000 >/dev/null || groupadd -g 1000 "$target"; \
            useradd -u 1000 -g 1000 -m -s /bin/bash "$target"; \
        fi; \
        getent passwd "$target" >/dev/null || { echo "cannot prepare user $target for uid 1000" >&2; exit 1; }; \
    fi
"#;

/// Packages that only make sense once the Ubuntu man pages are restored.
const MAN_PACKAGES: [&str; 2] = ["man", "manpages"];

/// A closed channel only means the UI is shutting down, so send errors are ignored.
async fn send_log(tx: &Sender<String>, message: impl Into<String>) {
    let _ = tx.send(message.into()).await;
}

fn spawn_pipe_reader(
    pipe: Option<impl AsyncRead + Unpin + Send + 'static>,
    tx: Sender<String>,
) -> JoinHandle<()> {
    task::spawn(async move {
        let Some(pipe) = pipe else { return };
        let mut lines = BufReader::new(pipe).lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if tx.send(line).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let _ = tx.send(format!("Error reading build output: {err}")).await;
                    break;
                }
            }
        }
    })
}

/// Turns a distro plus a package selection into a Containerfile, and builds it.
/// One line of the generated Containerfile, plus the packages that produced it.
///
/// Used by the UI to highlight the steps belonging to a package; the file written for
/// the build is the plain joined text, so no styling ever reaches the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerLine {
    pub text: String,
    /// Names of the packages this line installs (empty for the scaffold lines).
    pub packages: Vec<String>,
}

/// What a successful build produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutcome {
    /// Tag the image was built with, e.g. `localhost/toolshop:jazzy-1a2b3c`.
    pub image: String,
    /// Image ID right after the build, when the engine could report it.
    pub image_id: Option<String>,
}

/// Global build options, shown as checkboxes in the Builder tab (all on by default).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildOptions {
    /// Hand uid+gid 1000 to the user and `chown` the toolbox tree, which is what
    /// `toolbox create` expects.
    pub user_setup: bool,
    /// Install `libnss-myhostname` so `sudo` can resolve the container's own hostname.
    pub hostname_fix: bool,
    /// Keep the real account name out of the file: useful when the generated Containerfile
    /// is committed or recorded, since it falls back to the base image's default user.
    pub hide_username: bool,
    /// Explicit user name for `ARG DOCKER_USER`, overriding the host account.
    pub username: Option<String>,
    /// Extra `ENV` variables the user added, emitted after the catalog's own ones so they win.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            user_setup: true,
            hostname_fix: true,
            hide_username: false,
            username: None,
            env: std::collections::BTreeMap::new(),
        }
    }
}

impl BuildOptions {
    /// Rows in the options block: the checkboxes plus the user-name row.
    pub const ROWS: usize = 4;
    /// How many of those rows are checkboxes (the rest is the user-name value row).
    pub const CHECKBOXES: usize = 3;
    /// Index of the user-name row.
    pub const USER_ROW: usize = Self::CHECKBOXES;

    /// `(label, enabled)` for each checkbox, in display order.
    pub fn toggles(&self) -> [(&'static str, bool); Self::CHECKBOXES] {
        [
            ("user setup (uid 1000)", self.user_setup),
            ("hostname fix (nss)", self.hostname_fix),
            ("hide username", self.hide_username),
        ]
    }

    /// Flip the checkbox at `index`.
    pub fn toggle(&mut self, index: usize) {
        match index {
            0 => self.user_setup = !self.user_setup,
            1 => self.hostname_fix = !self.hostname_fix,
            2 => self.hide_username = !self.hide_username,
            _ => {}
        }
    }

    /// Row text for the user-name value row.
    pub fn user_row(&self, host_user: &str, recorded: &str) -> String {
        if self.hide_username {
            format!("user: {recorded} (hidden)")
        } else if self.username.is_some() {
            format!("user: {recorded} (custom)")
        } else {
            format!("user: {host_user}")
        }
    }
}

/// What the selected packages actually need in the prologue, so a bare distro yields a
/// bare Containerfile: toolchain directories only appear when something uses them.
///
/// Each entry keeps the packages that caused it, so the generated lines can be attributed
/// to them and highlighted in the UI like any other package step.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Needs {
    /// `CARGO_HOME` / `RUSTUP_HOME`.
    rust: Vec<String>,
    /// `NVM_DIR`.
    nvm: Vec<String>,
    /// `FZF_PREFIX` on `PATH`.
    fzf: Vec<String>,
    /// Script installs, i.e. `/opt/toolbox` is in use.
    scripts: Vec<String>,
}

impl Needs {
    /// Whether anything lives under [`TOOLBOX_PREFIX`].
    fn toolbox(&self) -> bool {
        !self.rust.is_empty()
            || !self.nvm.is_empty()
            || !self.fzf.is_empty()
            || !self.scripts.is_empty()
    }

    /// Every package that puts something under [`TOOLBOX_PREFIX`], each listed once (a
    /// package can need a toolchain *and* be a script install).
    fn toolbox_owners(&self) -> Vec<String> {
        unique_owners([&self.rust, &self.nvm, &self.fzf, &self.scripts])
    }

    /// Packages behind the `PATH` entries.
    fn path_owners(&self) -> Vec<String> {
        unique_owners([&self.rust, &self.nvm, &self.fzf])
    }
}

/// Script packages in dependency order: a package comes after the selected packages it
/// requires, so a script can call a tool another script installs. Cycles are tolerated (the
/// catalog keeps the first-seen order) and catalog order breaks ties.
fn script_order<'a>(resolved: &[(&'a Package, InstallSpec)]) -> Vec<&'a Package> {
    let scripts: Vec<&Package> = resolved
        .iter()
        .filter(|(_, spec)| matches!(spec, InstallSpec::Script { .. }))
        .map(|(package, _)| *package)
        .collect();

    fn visit<'a>(
        package: &'a Package,
        scripts: &[&'a Package],
        ordered: &mut Vec<&'a Package>,
        visiting: &mut Vec<String>,
    ) {
        if ordered.iter().any(|done| done.name == package.name)
            || visiting.iter().any(|seen| seen == &package.name)
        {
            return;
        }

        visiting.push(package.name.clone());
        for requirement in &package.requires {
            if let Some(dependency) = scripts.iter().find(|known| &known.name == requirement) {
                visit(dependency, scripts, ordered, visiting);
            }
        }
        visiting.pop();

        if !ordered.iter().any(|done| done.name == package.name) {
            ordered.push(package);
        }
    }

    let mut ordered = Vec::with_capacity(scripts.len());
    let mut visiting = Vec::new();
    for package in &scripts {
        visit(package, &scripts, &mut ordered, &mut visiting);
    }
    ordered
}

/// Merge owner lists, keeping first-seen order and no duplicates.
fn unique_owners<'a>(lists: impl IntoIterator<Item = &'a Vec<String>>) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    for list in lists {
        for name in list {
            if !owners.contains(name) {
                owners.push(name.clone());
            }
        }
    }
    owners
}

#[derive(Debug, Clone)]
pub struct ContainerBuilder {
    distro: Option<Distro>,
    packages: Vec<Package>,
    container_file: String,
    /// The same file, split into lines annotated with the package they come from.
    lines: Vec<ContainerLine>,
    image_tag: Option<String>,
    /// Host user the image is prepared for (`--build-arg DOCKER_USER`).
    docker_user: String,
    /// Per-package install instructions that replace the catalog entries.
    overrides: std::collections::BTreeMap<String, InstallOverride>,
    /// Per-package version choices (`{{version}}` in script installs).
    versions: std::collections::BTreeMap<String, String>,
    /// Global build options (checkboxes in the UI).
    options: BuildOptions,
}

impl Default for ContainerBuilder {
    fn default() -> Self {
        Self {
            distro: None,
            packages: vec![],
            container_file: String::new(),
            image_tag: None,
            lines: Vec::new(),
            docker_user: std::env::var("USER").unwrap_or_else(|_| DEFAULT_DOCKER_USER.to_string()),
            overrides: std::collections::BTreeMap::new(),
            versions: std::collections::BTreeMap::new(),
            options: BuildOptions::default(),
        }
    }
}

impl ContainerBuilder {
    pub fn set_distro(&mut self, distro: Distro) {
        self.distro = Some(distro);
    }

    pub fn set_packages(&mut self, packages: Vec<Package>) {
        self.packages = packages;
    }

    pub fn set_overrides(
        &mut self,
        overrides: std::collections::BTreeMap<String, InstallOverride>,
    ) {
        self.overrides = overrides;
    }

    /// Per-package version choices, substituted for `{{version}}` in script installs.
    pub fn set_versions(&mut self, versions: std::collections::BTreeMap<String, String>) {
        self.versions = versions;
    }

    /// Global build options (user setup, hostname resolution, …).
    pub fn set_options(&mut self, options: BuildOptions) {
        self.options = options;
    }

    pub fn options(&self) -> &BuildOptions {
        &self.options
    }

    /// Version a package installs with: the user's choice or the catalog entry.
    pub fn version_of(&self, package: &Package) -> String {
        effective_version(package, &self.versions).to_string()
    }

    /// Clear the selected distro (e.g. after removing a user-added image).
    pub fn set_distro_option(&mut self, distro: Option<Distro>) {
        self.distro = distro;
    }

    pub fn distro(&self) -> Option<&Distro> {
        self.distro.as_ref()
    }

    /// Which parts of the prologue the selected packages need, and who needs them.
    fn needs(&self, resolved: &[(&Package, InstallSpec)]) -> Needs {
        let mut needs = Needs::default();
        for (package, spec) in resolved {
            let InstallSpec::Script { script } = spec else {
                continue;
            };
            let name = package.name.clone();
            needs.scripts.push(name.clone());
            let script = expand_script(script, effective_version(package, &self.versions));
            if script.contains("CARGO_HOME") || script.contains("RUSTUP_HOME") {
                needs.rust.push(name.clone());
            }
            if script.contains("NVM_DIR") {
                needs.nvm.push(name.clone());
            }
            if script.contains(FZF_PREFIX) {
                needs.fzf.push(name.clone());
            }
        }
        needs
    }

    /// User name recorded in `ARG DOCKER_USER`: the host account unless hidden or overridden.
    fn recorded_user(&self) -> String {
        self.recorded_user_for(&self.options)
    }

    pub fn get_container_file(&self) -> &str {
        &self.container_file
    }

    /// The Containerfile split into lines, each tagged with the packages it installs.
    pub fn container_lines(&self) -> &[ContainerLine] {
        &self.lines
    }

    /// Tag the image will be built with, once a distro is selected.
    pub fn image_tag(&self) -> Option<&str> {
        self.image_tag.as_deref()
    }

    /// Reference the engine reports for the built image.
    pub fn image_reference(&self) -> Option<String> {
        self.image_tag
            .as_ref()
            .map(|tag| format!("localhost/{tag}"))
    }

    /// Host user the generated image renames the default `ubuntu` user to.
    pub fn docker_user(&self) -> &str {
        &self.docker_user
    }

    /// The host account name, for display (`$USER` unless overridden).
    pub fn docker_user_name(&self) -> &str {
        if self.docker_user.trim().is_empty() {
            DEFAULT_DOCKER_USER
        } else {
            self.docker_user.trim()
        }
    }

    /// The name that ends up in `ARG DOCKER_USER` for the given options.
    pub fn recorded_user_for(&self, options: &BuildOptions) -> String {
        if options.hide_username {
            return DEFAULT_DOCKER_USER.to_string();
        }
        if let Some(name) = options.username.as_deref() {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
        self.docker_user_name().to_string()
    }

    pub fn set_docker_user(&mut self, user: impl Into<String>) {
        let user = user.into();
        if !user.trim().is_empty() {
            self.docker_user = user;
        }
    }

    /// Regenerate the Containerfile from the current distro and package selection.
    pub fn create_container_file(&mut self) {
        let Some(distro) = &self.distro else {
            self.container_file.clear();
            self.lines.clear();
            self.image_tag = None;
            return;
        };

        // Resolve every selected package once, keeping the package it came from so each
        // generated line can be attributed for the UI.
        let resolved: Vec<(&Package, InstallSpec)> = self
            .packages
            .iter()
            .filter_map(|package| {
                Some((package, resolve_install(package, distro, &self.overrides)?))
            })
            .collect();

        let mut lines: Vec<ContainerLine> = Vec::new();
        let no_owner: [String; 0] = [];
        let needs = self.needs(&resolved);
        let apt = distro.family.package_manager() == PackageManager::Apt;

        push_lines(&mut lines, &format!("FROM {}\n", distro.image), &no_owner);
        if self.options.user_setup {
            // Default the argument to the recorded user, so a plain
            // `podman build -f Containerfile .` produces the right account too.
            let docker_user = self.recorded_user();
            push_lines(
                &mut lines,
                &format!("ARG DOCKER_USER={docker_user}\n"),
                &no_owner,
            );
        }
        push_lines(&mut lines, "SHELL [\"/bin/bash\", \"-c\"]\n", &no_owner);

        // Package-manager installs are grouped into a single cache-friendly layer.
        let manager_packages: Vec<(&Package, &str)> = resolved
            .iter()
            .filter_map(|(package, spec)| match spec {
                InstallSpec::PackageManager { pkg } => Some((*package, pkg.as_str())),
                InstallSpec::Script { .. } => None,
            })
            .collect();

        // Debian/Ubuntu bases do not resolve their own hostname; Fedora images already ship
        // `nss-myhostname`. Whether to fix it is one of the build options.
        let hostname_fix = apt && self.options.hostname_fix;

        if apt && (!manager_packages.is_empty() || hostname_fix || !needs.scripts.is_empty()) {
            push_lines(
                &mut lines,
                "ENV DEBIAN_FRONTEND=noninteractive\n",
                &no_owner,
            );
        }
        // Environment variables the selected packages ask for (e.g. Isaac Sim's EULA
        // acceptance), each attributed to the package that needs it.
        let mut emitted: BTreeSet<String> = BTreeSet::new();
        for (package, _) in &resolved {
            for (key, value) in &package.env {
                if !emitted.insert(key.clone()) {
                    continue;
                }
                push_lines(
                    &mut lines,
                    &format!("{}\n", env_line(key, value)),
                    std::slice::from_ref(&package.name),
                );
            }
        }

        // The user's own variables come after the packages' ones: Docker applies the last
        // `ENV` for a key, so an explicit choice always wins.
        for (key, value) in &self.options.env {
            push_lines(
                &mut lines,
                &format!("{}\n", env_line(key, value)),
                &no_owner,
            );
        }

        // Every line below is attributed to the package(s) that need it, so the UI can
        // highlight them exactly like a package's own install steps.
        if needs.toolbox() {
            push_lines(
                &mut lines,
                &format!("ENV TOOLBOX_PREFIX={TOOLBOX_PREFIX}\n"),
                &needs.toolbox_owners(),
            );
        }

        // Toolchains and caches live outside the home directory, so toolbox can mount the
        // host home over it without hiding them — but only for the tools that are selected.
        if !needs.rust.is_empty() {
            push_lines(
                &mut lines,
                &format!("ENV CARGO_HOME={CARGO_HOME}\n"),
                &needs.rust,
            );
            push_lines(
                &mut lines,
                &format!("ENV RUSTUP_HOME={RUSTUP_HOME}\n"),
                &needs.rust,
            );
        }
        if !needs.nvm.is_empty() {
            push_lines(&mut lines, &format!("ENV NVM_DIR={NVM_DIR}\n"), &needs.nvm);
        }

        let mut path_segments: Vec<String> = Vec::new();
        if !needs.rust.is_empty() {
            path_segments.push("${CARGO_HOME}/bin".to_string());
            path_segments.push("${RUSTUP_HOME}/bin".to_string());
        }
        if !needs.nvm.is_empty() {
            path_segments.push("${NVM_DIR}/node/bin".to_string());
        }
        if !needs.fzf.is_empty() {
            path_segments.push(format!("{FZF_PREFIX}/bin"));
        }
        if !path_segments.is_empty() {
            push_lines(
                &mut lines,
                &format!("ENV PATH=\"{}:${{PATH}}\"\n", path_segments.join(":")),
                &needs.path_owners(),
            );
        }

        let mut directories: Vec<String> = Vec::new();
        if !needs.rust.is_empty() {
            directories.push("${CARGO_HOME}".to_string());
            directories.push("${RUSTUP_HOME}".to_string());
        }
        if !needs.nvm.is_empty() {
            directories.push("${NVM_DIR}".to_string());
        }
        if !needs.fzf.is_empty() {
            directories.push(format!("{FZF_PREFIX}/bin"));
        }
        if !needs.scripts.is_empty() {
            directories.push(TOOLBOX_PREFIX.to_string());
        }
        if !directories.is_empty() {
            push_lines(
                &mut lines,
                &format!("RUN mkdir -p {}\n", directories.join(" ")),
                &needs.toolbox_owners(),
            );
        }

        if !manager_packages.is_empty() || hostname_fix {
            let (header, trailer) = install_layer_parts(distro.family.package_manager());
            push_lines(&mut lines, header, &no_owner);
            if hostname_fix {
                push_lines(
                    &mut lines,
                    &format!("    {HOSTNAME_PACKAGE} \\\n"),
                    &no_owner,
                );
            }
            for (package, pkg) in &manager_packages {
                push_lines(
                    &mut lines,
                    &format!("    {pkg} \\\n"),
                    std::slice::from_ref(&package.name),
                );
            }
            push_lines(&mut lines, trailer, &no_owner);
        }

        if hostname_fix {
            push_lines(&mut lines, HOSTNAME_STEP, &no_owner);
        }

        // Ubuntu strips man pages from its base images; restore them when asked for.
        if apt
            && self
                .packages
                .iter()
                .any(|package| MAN_PACKAGES.contains(&package.name.as_str()))
        {
            push_lines(&mut lines, "RUN yes | unminimize\n", &no_owner);
        }

        // Scripts run afterwards, so they can rely on the packages installed above. They are
        // emitted in dependency order: `requires` first, so e.g. `uv` exists before Isaac Sim
        // calls it (catalog order alone would put it after).
        let scripts = script_order(&resolved);
        for package in scripts {
            let Some(InstallSpec::Script { script }) =
                resolve_install(package, distro, &self.overrides)
            else {
                continue;
            };
            let owner = std::slice::from_ref(&package.name);
            push_lines(
                &mut lines,
                &format!("\n# --- install {} ---\n", package.name),
                owner,
            );
            let script = expand_script(&script, effective_version(package, &self.versions));
            push_lines(&mut lines, &format!("RUN {script}\n"), owner);
        }

        if self.options.user_setup {
            // Hand the toolbox tree to the user; nothing to do when nothing lives there.
            if needs.toolbox() {
                push_lines(
                    &mut lines,
                    &format!("\nRUN chown -R 1000:1000 {TOOLBOX_PREFIX}\n"),
                    &needs.toolbox_owners(),
                );
            }
            // Make uid/gid 1000 belong to $DOCKER_USER, whatever the base image ships:
            // `toolbox create` runs `useradd --uid 1000 <host user>` inside the container and
            // fails with "UID 1000 is not unique" when the image already owns that id under
            // another name (ubuntu images ship an `ubuntu` user on 1000). Renaming it keeps
            // the account (home, shell) reusable, and creating it covers images without one.
            if apt || matches!(distro.family, DistroFamily::Fedora) {
                push_lines(&mut lines, USER_STEP, &no_owner);
            }
        }

        // The file handed to podman is the plain joined text: styling stays in the UI.
        let mut file = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !file.is_empty() {
            file.push('\n');
        }

        self.container_file = file;
        self.lines = lines;
        self.image_tag = Some(Manifest::image_tag(distro, &self.container_file));
    }

    /// Build the image, tagging it so later runs can be recognised.
    ///
    /// Returns `Ok(None)` when the build was cancelled.
    pub async fn run_build(
        &self,
        tx: Sender<String>,
        token: CancellationToken,
    ) -> Result<Option<BuildOutcome>> {
        if self.container_file.trim().is_empty() {
            bail!("Containerfile is empty");
        }
        let Some(image) = self.image_reference() else {
            bail!("no distro selected");
        };

        let base_image = self
            .distro
            .as_ref()
            .map(|distro| distro.image.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        send_log(&tx, format!("Building {image} from {base_image}")).await;

        tokio::fs::write(CONTAINER_FILE, self.container_file.as_bytes())
            .await
            .wrap_err_with(|| format!("failed to write {CONTAINER_FILE}"))?;

        let mut child = build_command(&image, &self.docker_user)
            .spawn()
            .wrap_err("failed to spawn `podman` (is it installed and on PATH?)")?;

        let stdout_reader = spawn_pipe_reader(child.stdout.take(), tx.clone());
        let stderr_reader = spawn_pipe_reader(child.stderr.take(), tx.clone());

        // The child is polled instead of awaited so cancellation can still take `&mut child`.
        let mut cancel_requested = false;
        let status = loop {
            if !cancel_requested && token.is_cancelled() {
                cancel_requested = true;
                send_log(&tx, "Cancelling build...").await;
                if let Err(err) = child.start_kill() {
                    send_log(&tx, format!("Failed to kill podman: {err}")).await;
                }
            }

            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => tokio::time::sleep(BUILD_POLL_INTERVAL).await,
                Err(err) => return Err(eyre!("failed to poll podman: {err}")),
            }
        };

        // The readers finish once the pipes close; do not hang on a stuck pipe forever.
        let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
        })
        .await;
        if drained.is_err() {
            send_log(&tx, "Timed out waiting for podman output to close.").await;
        }

        if cancel_requested {
            send_log(&tx, format!("Build cancelled ({status}).")).await;
            return Ok(None);
        }

        if !status.success() {
            bail!("podman exited with {status}");
        }

        let image_id = match engine::image_id(&image).await {
            Ok(id) => id,
            Err(err) => {
                send_log(&tx, format!("warning: cannot read the image id: {err}")).await;
                None
            }
        };

        send_log(&tx, format!("Build finished ({status}): {image}")).await;
        Ok(Some(BuildOutcome { image, image_id }))
    }
}

/// `podman build` invocation for a tagged image.
///
/// `--format docker` matters: with the default OCI format podman ignores the Containerfile's
/// `SHELL` instruction and runs every `RUN` with `/bin/sh`, which breaks the bash-dependent
/// install scripts (e.g. `nvm.sh`).
fn build_command(image: &str, docker_user: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("podman");
    command
        .args([
            "build",
            "--format",
            "docker",
            "-t",
            image,
            "--build-arg",
            &format!("DOCKER_USER={docker_user}"),
            "-f",
            CONTAINER_FILE,
            ".",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

/// Render an `ENV` instruction, quoting the value when it would otherwise be misparsed.
///
/// `ENV KEY=value` takes the rest of the line verbatim but treats `$` as expansion and
/// whitespace at the edges as nothing, so anything with spaces, quotes, `$` or a `#` is
/// wrapped in double quotes with the sensitive characters escaped.
fn env_line(key: &str, value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '\\' | '$' | '#' | ';' | '&' | '|' | '<' | '>'
                )
        });
    if !needs_quotes {
        return format!("ENV {key}={value}");
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$");
    format!("ENV {key}=\"{escaped}\"")
}

/// Opening and closing lines of the single `RUN` layer that installs package-manager
/// packages; the package lines go in between, one per package.
fn install_layer_parts(manager: PackageManager) -> (&'static str, &'static str) {
    match manager {
        PackageManager::Apt => (
            "RUN apt-get update && apt-get install -y --no-install-recommends \\\n",
            " && rm -rf /var/lib/apt/lists/*\n",
        ),
        PackageManager::Dnf => ("RUN dnf install -y \\\n", " && dnf clean all\n"),
    }
}

/// Append `chunk` as one or more [`ContainerLine`]s owned by `packages`.
fn push_lines(lines: &mut Vec<ContainerLine>, chunk: &str, packages: &[String]) {
    let mut parts: Vec<&str> = chunk.split('\n').collect();
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    for part in parts {
        lines.push(ContainerLine {
            text: part.to_string(),
            packages: packages.to_vec(),
        });
    }
}

impl fmt::Display for ContainerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({})",
            self.distro
                .as_ref()
                .map(|distro| distro.name.as_str())
                .unwrap_or("<none>"),
            self.packages.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::{Catalog, DEFAULT_INSTALL_KEY},
        distro::{DistroFamily, DistroSource},
    };
    use std::collections::BTreeMap;

    fn package(name: &str, install: &[(&str, InstallSpec)]) -> Package {
        Package {
            name: name.to_string(),
            version: String::new(),
            summary: String::new(),
            category: "Test".to_string(),
            requires: vec![],
            install: install
                .iter()
                .map(|(distro, spec)| ((*distro).to_string(), spec.clone()))
                .collect::<BTreeMap<_, _>>(),
            env: BTreeMap::new(),
        }
    }

    fn pkg(pkg: &str) -> InstallSpec {
        InstallSpec::PackageManager {
            pkg: pkg.to_string(),
        }
    }

    fn script(script: &str) -> InstallSpec {
        InstallSpec::Script {
            script: script.to_string(),
        }
    }

    /// A package from the bundled catalog.
    fn catalog_package(name: &str) -> Package {
        crate::catalog::Catalog::load_from_dirs(&[])
            .expect("bundled catalog")
            .get(name)
            .unwrap_or_else(|| panic!("{name} is in the catalog"))
            .clone()
    }

    /// Builder with a predictable `DOCKER_USER`, so expectations do not depend on `$USER`.
    fn test_builder() -> ContainerBuilder {
        let mut builder = ContainerBuilder::default();
        builder.set_docker_user(DEFAULT_DOCKER_USER);
        builder
    }

    fn ros(name: &str) -> Distro {
        Distro::new(
            DistroFamily::Ros,
            name,
            format!("docker.io/library/ros:{name}-ros-base"),
            DistroSource::Predefined,
        )
    }

    fn fedora() -> Distro {
        Distro::new(
            DistroFamily::Fedora,
            "41",
            "docker.io/library/fedora:41",
            DistroSource::Predefined,
        )
    }

    #[test]
    fn container_file_is_empty_without_a_distro() {
        let mut builder = test_builder();
        builder.set_packages(vec![package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))])]);
        builder.create_container_file();
        assert!(builder.get_container_file().is_empty());
    }

    #[test]
    fn package_manager_packages_share_one_layer() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![
            package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))]),
            package("ping", &[(DEFAULT_INSTALL_KEY, pkg("iputils-ping"))]),
        ]);
        builder.create_container_file();
        let file = builder.get_container_file();

        // One layer, one `apt-get update`, packages in selection order.
        assert_eq!(file.matches("apt-get install").count(), 1, "{file}");
        assert_eq!(file.matches("apt-get update").count(), 1, "{file}");
        let header = file.find("RUN apt-get update").unwrap();
        let htop = file.find("    htop \\").unwrap();
        let ping = file.find("    iputils-ping \\").unwrap();
        let trailer = file.find(" && rm -rf /var/lib/apt/lists/*").unwrap();
        assert!(header < htop && htop < ping && ping < trailer, "{file}");
        assert!(
            file[header..trailer].contains("libnss-myhostname"),
            "the hostname fix rides in the same layer: {file}"
        );
    }

    #[test]
    fn fedora_uses_dnf() {
        let mut builder = test_builder();
        builder.set_distro(fedora());
        builder.set_packages(vec![package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))])]);
        builder.create_container_file();
        let file = builder.get_container_file();

        assert!(
            file.starts_with("FROM docker.io/library/fedora:41\n"),
            "{file}"
        );
        assert!(
            file.contains("RUN dnf install -y \\\n    htop \\\n && dnf clean all"),
            "{file}"
        );
        assert!(!file.contains("apt-get"), "{file}");
        // Fedora ships nss-myhostname with systemd, so neither the package nor the
        // nsswitch edit is needed.
        assert!(!file.contains("DEBIAN_FRONTEND"), "{file}");
        assert!(!file.contains("libnss-myhostname"), "{file}");
        assert!(!file.contains("nsswitch.conf"), "{file}");
    }

    #[test]
    fn a_bare_distro_generates_a_bare_containerfile() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![]);
        builder.create_container_file();
        let file = builder.get_container_file();

        // Nothing selected: no toolchain directories, PATH, ownership or uv environment.
        for absent in [
            "ENV PATH=",
            "mkdir",
            "/opt/toolbox",
            "chown",
            "CARGO_HOME",
            "NVM_DIR",
            "UV_BREAK_SYSTEM_PACKAGES",
        ] {
            assert!(
                !file.contains(absent),
                "{absent} should not appear:\n{file}"
            );
        }
        assert!(
            file.starts_with("FROM docker.io/library/ros:jazzy-ros-base\n"),
            "{file}"
        );

        // Both options off leaves just the base image and the shell.
        builder.set_options(BuildOptions {
            user_setup: false,
            hostname_fix: false,
            ..BuildOptions::default()
        });
        builder.create_container_file();
        assert_eq!(
            builder.get_container_file(),
            "FROM docker.io/library/ros:jazzy-ros-base\nSHELL [\"/bin/bash\", \"-c\"]\n"
        );
    }

    #[test]
    fn toolchain_directories_only_appear_for_selected_packages() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));

        builder.set_packages(vec![catalog_package("rust")]);
        builder.create_container_file();
        let rust = builder.get_container_file();
        assert!(rust.contains("ENV CARGO_HOME=/opt/toolbox/cargo"), "{rust}");
        assert!(rust.contains("ENV RUSTUP_HOME=/opt/toolbox/rust"), "{rust}");
        assert!(
            rust.contains("ENV PATH=\"${CARGO_HOME}/bin:${RUSTUP_HOME}/bin:${PATH}\""),
            "{rust}"
        );
        assert!(!rust.contains("NVM_DIR"), "{rust}");
        assert!(!rust.contains("/opt/toolbox/fzf"), "{rust}");

        builder.set_packages(vec![catalog_package("nvm")]);
        builder.create_container_file();
        let nvm = builder.get_container_file();
        assert!(nvm.contains("ENV NVM_DIR=/opt/toolbox/nvm"), "{nvm}");
        assert!(nvm.contains("${NVM_DIR}/node/bin"), "{nvm}");
        assert!(!nvm.contains("CARGO_HOME"), "{nvm}");

        builder.set_packages(vec![catalog_package("fzf")]);
        builder.create_container_file();
        let fzf = builder.get_container_file();
        assert!(
            fzf.contains("ENV PATH=\"/opt/toolbox/fzf/bin:${PATH}\""),
            "{fzf}"
        );
        assert!(
            !fzf.contains("CARGO_HOME") && !fzf.contains("NVM_DIR"),
            "{fzf}"
        );

        builder.set_packages(vec![catalog_package("htop")]);
        builder.create_container_file();
        let htop = builder.get_container_file();
        assert!(!htop.contains("ENV PATH="), "{htop}");
        assert!(!htop.contains("/opt/toolbox"), "{htop}");
    }

    #[test]
    fn user_environment_variables_are_emitted_after_the_packages() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![catalog_package("uv")]);
        builder.set_options(BuildOptions {
            env: std::collections::BTreeMap::from([
                ("FOO".to_string(), "bar".to_string()),
                ("GREETING".to_string(), "hello world".to_string()),
                ("DOLLAR".to_string(), "$HOME".to_string()),
                ("QUOTED".to_string(), "say \"hi\"".to_string()),
                // Wins over the same key in a package's own `[env]` table.
                ("UV_BREAK_SYSTEM_PACKAGES".to_string(), "0".to_string()),
            ]),
            ..BuildOptions::default()
        });
        builder.create_container_file();
        let file = builder.get_container_file();

        assert!(file.contains("ENV FOO=bar"), "{file}");
        assert!(file.contains("ENV GREETING=\"hello world\""), "{file}");
        // `$` and quotes are escaped so Docker does not expand or misparse them.
        assert!(file.contains("ENV DOLLAR=\"\\$HOME\""), "{file}");
        assert!(
            file.contains(&format!("ENV QUOTED=\"say {}\"", "\\\"hi\\\"")),
            "{file}"
        );
        // The user's line comes last, so it is the one in effect.
        assert!(file.contains("ENV UV_BREAK_SYSTEM_PACKAGES=0"), "{file}");
        let uv_package = file.find("ENV UV_BREAK_SYSTEM_PACKAGES=1").unwrap();
        let user_line = file.find("ENV UV_BREAK_SYSTEM_PACKAGES=0").unwrap();
        assert!(
            uv_package < user_line,
            "the user's value is applied last:\n{file}"
        );

        // They belong to no package, so nothing is highlighted for them.
        assert!(
            builder
                .container_lines()
                .iter()
                .any(|line| line.text.contains("ENV FOO=bar") && line.packages.is_empty())
        );
    }

    #[test]
    fn package_environment_variables_are_emitted_and_owned() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![catalog_package("uv"), catalog_package("isaac-sim")]);
        builder.create_container_file();
        let file = builder.get_container_file();

        assert!(file.contains("ENV UV_BREAK_SYSTEM_PACKAGES=1"), "{file}");
        assert!(file.contains("ENV OMNI_KIT_ACCEPT_EULA=YES"), "{file}");

        let owner_of = |needle: &str| -> Vec<String> {
            builder
                .container_lines()
                .iter()
                .find(|line| line.text.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}"))
                .packages
                .clone()
        };
        assert_eq!(
            owner_of("ENV UV_BREAK_SYSTEM_PACKAGES"),
            vec!["uv".to_string()]
        );
        assert_eq!(
            owner_of("ENV OMNI_KIT_ACCEPT_EULA"),
            vec!["isaac-sim".to_string()]
        );

        // Without those packages the image stays free of their environment.
        builder.set_packages(vec![catalog_package("htop")]);
        builder.create_container_file();
        let bare = builder.get_container_file();
        assert!(!bare.contains("UV_BREAK_SYSTEM_PACKAGES"), "{bare}");
        assert!(!bare.contains("OMNI_KIT_ACCEPT_EULA"), "{bare}");
    }

    #[test]
    fn scripts_are_installed_in_dependency_order() {
        // `isaac-sim` requires `uv`, but catalog order puts it first alphabetically.
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![catalog_package("isaac-sim"), catalog_package("uv")]);
        builder.create_container_file();
        let file = builder.get_container_file();

        let uv = file.find("# --- install uv ---").expect("uv block");
        let isaac = file
            .find("# --- install isaac-sim ---")
            .expect("isaac block");
        assert!(uv < isaac, "uv must be installed before Isaac Sim:\n{file}");

        // Genesis and MuJoCo are independent: catalog order is kept.
        builder.set_packages(vec![catalog_package("uv"), catalog_package("genesis")]);
        builder.create_container_file();
        let file = builder.get_container_file();
        assert!(
            file.find("# --- install uv ---").unwrap()
                < file.find("# --- install genesis ---").unwrap()
        );
    }

    #[test]
    fn the_build_runs_podman_with_the_docker_format() {
        let command = build_command("localhost/toolshop:jazzy-1a2b3c", "dev");
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        // Without the docker format podman ignores `SHELL` and runs `RUN` with /bin/sh.
        let format = args
            .iter()
            .position(|arg| arg == "--format")
            .expect("--format");
        assert_eq!(args.get(format + 1).map(String::as_str), Some("docker"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-t", "localhost/toolshop:jazzy-1a2b3c"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--build-arg", "DOCKER_USER=dev"])
        );
        assert!(args.windows(2).any(|pair| pair == ["-f", CONTAINER_FILE]));
        assert_eq!(args.last().map(String::as_str), Some("."));
    }

    #[test]
    fn build_options_control_the_infrastructure_steps() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![catalog_package("rust")]);

        builder.set_options(BuildOptions::default());
        builder.create_container_file();
        let both = builder.get_container_file();
        assert!(both.contains("ARG DOCKER_USER="), "{both}");
        assert!(both.contains("chown -R 1000:1000 /opt/toolbox"), "{both}");
        assert!(both.contains("usermod -l"), "{both}");
        assert!(both.contains("libnss-myhostname"), "{both}");

        builder.set_options(BuildOptions {
            user_setup: false,
            hostname_fix: false,
            ..BuildOptions::default()
        });
        builder.create_container_file();
        let neither = builder.get_container_file();
        for absent in [
            "ARG DOCKER_USER",
            "chown",
            "usermod",
            "libnss-myhostname",
            "nsswitch",
        ] {
            assert!(
                !neither.contains(absent),
                "{absent} should be gone:\n{neither}"
            );
        }
        assert!(
            neither.contains("ENV CARGO_HOME=/opt/toolbox/cargo"),
            "the package itself is still installed: {neither}"
        );

        builder.set_options(BuildOptions {
            user_setup: true,
            hostname_fix: false,
            ..BuildOptions::default()
        });
        builder.create_container_file();
        let user_only = builder.get_container_file();
        assert!(user_only.contains("usermod -l"), "{user_only}");
        assert!(!user_only.contains("libnss-myhostname"), "{user_only}");
    }

    #[test]
    fn prologue_lines_are_attributed_to_the_packages_that_need_them() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![catalog_package("rust"), catalog_package("nvm")]);
        builder.create_container_file();

        let owner_of = |needle: &str| -> Vec<String> {
            builder
                .container_lines()
                .iter()
                .find(|line| line.text.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}"))
                .packages
                .clone()
        };

        // Toolchain lines belong to the toolchain that needs them …
        assert_eq!(owner_of("ENV CARGO_HOME"), vec!["rust".to_string()]);
        assert_eq!(owner_of("ENV RUSTUP_HOME"), vec!["rust".to_string()]);
        assert_eq!(owner_of("ENV NVM_DIR"), vec!["nvm".to_string()]);
        // … shared lines list every package behind them.
        let path = owner_of("ENV PATH=");
        assert!(path.contains(&"rust".to_string()) && path.contains(&"nvm".to_string()));
        let mkdir = owner_of("RUN mkdir -p");
        assert!(mkdir.contains(&"rust".to_string()) && mkdir.contains(&"nvm".to_string()));
        let prefix = owner_of("ENV TOOLBOX_PREFIX");
        assert!(prefix.contains(&"rust".to_string()) && prefix.contains(&"nvm".to_string()));
        let chown = owner_of("chown -R 1000:1000");
        assert!(chown.contains(&"rust".to_string()) && chown.contains(&"nvm".to_string()));

        // Scaffolding that no package caused stays unowned.
        assert!(owner_of("FROM ").is_empty());
        assert!(owner_of("SHELL [").is_empty());
        assert!(owner_of("ARG DOCKER_USER").is_empty());
    }

    #[test]
    fn the_recorded_user_can_be_hidden_or_overridden() {
        let mut builder = test_builder(); // docker_user = "ubuntu"
        builder.set_docker_user("real-person");
        builder.set_distro(ros("jazzy"));
        builder.create_container_file();
        assert!(
            builder
                .get_container_file()
                .contains("ARG DOCKER_USER=real-person")
        );

        // Override with any name.
        builder.set_options(BuildOptions {
            username: Some("dev".to_string()),
            ..BuildOptions::default()
        });
        builder.create_container_file();
        assert!(builder.get_container_file().contains("ARG DOCKER_USER=dev"));

        // Hiding wins over the override and never leaks the host account, so the
        // Containerfile can be recorded or committed.
        builder.set_options(BuildOptions {
            hide_username: true,
            username: Some("dev".to_string()),
            ..BuildOptions::default()
        });
        builder.create_container_file();
        let hidden = builder.get_container_file();
        assert!(hidden.contains("ARG DOCKER_USER=ubuntu"), "{hidden}");
        assert!(!hidden.contains("real-person"), "{hidden}");
        assert!(
            !hidden.lines().any(
                |line| line.starts_with("ARG DOCKER_USER=") && line != "ARG DOCKER_USER=ubuntu"
            ),
            "only the placeholder name is recorded: {hidden}"
        );

        // The build still passes the real name as a build arg, which is not in the file.
        assert_eq!(builder.docker_user(), "real-person");
        assert_eq!(builder.recorded_user_for(builder.options()), "ubuntu");
    }

    #[test]
    fn the_tag_changes_with_the_options() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![catalog_package("htop")]);
        builder.create_container_file();
        let default_tag = builder.image_tag().unwrap().to_string();

        builder.set_options(BuildOptions {
            user_setup: false,
            hostname_fix: true,
            ..BuildOptions::default()
        });
        builder.create_container_file();
        assert_ne!(builder.image_tag().unwrap(), default_tag);

        builder.set_options(BuildOptions::default());
        builder.create_container_file();
        assert_eq!(builder.image_tag().unwrap(), default_tag, "stable again");
    }

    #[test]
    fn distro_specific_package_name_is_used() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![package(
            "clangd",
            &[
                (DEFAULT_INSTALL_KEY, pkg("clangd")),
                ("jazzy", pkg("clangd-18")),
            ],
        )]);
        builder.create_container_file();
        assert!(builder.get_container_file().contains("clangd-18"));
        assert!(!builder.get_container_file().contains("    clangd \\"));
    }

    #[test]
    fn family_specs_are_used_for_matching_distros() {
        let mut builder = test_builder();
        builder.set_distro(fedora());
        builder.set_packages(vec![package(
            "clangd",
            &[
                (DEFAULT_INSTALL_KEY, pkg("clangd")),
                ("fedora", pkg("clang-tools-extra")),
            ],
        )]);
        builder.create_container_file();
        assert!(builder.get_container_file().contains("clang-tools-extra"));
    }

    #[test]
    fn every_continuation_line_ends_with_a_backslash() {
        for distro in [ros("jazzy"), fedora()] {
            let mut builder = test_builder();
            builder.set_distro(distro);
            builder.set_packages(vec![
                package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))]),
                package("tmux", &[(DEFAULT_INSTALL_KEY, pkg("tmux"))]),
            ]);
            builder.create_container_file();

            for line in builder.get_container_file().lines() {
                if line.contains('\\') {
                    assert!(line.ends_with('\\'), "malformed continuation: {line:?}");
                }
            }
        }
    }

    #[test]
    fn script_packages_run_after_manager_packages() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![
            package("fzf", &[(DEFAULT_INSTALL_KEY, script("echo install fzf"))]),
            package("curl", &[(DEFAULT_INSTALL_KEY, pkg("curl"))]),
        ]);
        builder.create_container_file();
        let file = builder.get_container_file();

        let install = file.find("apt-get install").expect("manager layer");
        let script = file.find("RUN echo install fzf").expect("script layer");
        assert!(
            install < script,
            "script package installed before its prerequisite"
        );
        assert!(file.contains("# --- install fzf ---"));
    }

    #[test]
    fn images_are_handed_to_the_host_user() {
        let mut builder = test_builder();
        builder.set_docker_user("dev");
        builder.set_distro(ros("jazzy"));
        builder.create_container_file();
        let file = builder.get_container_file();

        // The generated file defaults the argument to the configured user, so a plain
        // `podman build -f container .` produces the right account without build args.
        assert!(file.contains("ARG DOCKER_USER=dev"), "{file}");
        assert!(builder.docker_user() == "dev");

        // Without an explicit user the documented default is used.
        let mut fallback = test_builder();
        fallback.set_distro(ros("jazzy"));
        fallback.create_container_file();
        assert!(
            fallback
                .get_container_file()
                .contains("ARG DOCKER_USER=ubuntu"),
            "{}",
            fallback.get_container_file()
        );

        // uid/gid 1000 is handed to the user either way: renamed if the base image owns it
        // (ubuntu), created if it does not (fedora).
        for distro in [ros("jazzy"), fedora()] {
            builder.set_distro(distro.clone());
            builder.create_container_file();
            let file = builder.get_container_file();
            assert!(
                file.contains("usermod -l \"$target\""),
                "{distro:?}: {file}"
            );
            assert!(file.contains("useradd -u 1000 -g 1000"), "{distro:?}");
            assert!(
                file.contains("getent passwd 1000"),
                "the step looks at who owns 1000 first: {distro:?}"
            );
            assert!(
                file.contains("cannot prepare user $target for uid 1000"),
                "and fails the build if it cannot arrange it: {distro:?}"
            );
        }
    }

    #[test]
    fn toolchains_live_outside_the_home_directory() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![
            catalog_package("rust"),
            catalog_package("nvm"),
            catalog_package("fzf"),
            catalog_package("uv"),
        ]);
        builder.create_container_file();
        let file = builder.get_container_file();

        // Everything script-installed lives under /opt/toolbox, outside $HOME.
        assert!(file.contains("ENV CARGO_HOME=/opt/toolbox/cargo"), "{file}");
        assert!(file.contains("ENV RUSTUP_HOME=/opt/toolbox/rust"), "{file}");
        assert!(file.contains("ENV NVM_DIR=/opt/toolbox/nvm"), "{file}");
        assert!(file.contains("/opt/toolbox/fzf/bin"), "{file}");
        assert!(file.contains("ENV UV_BREAK_SYSTEM_PACKAGES=1"), "{file}");
        assert!(file.contains("chown -R 1000:1000 /opt/toolbox"), "{file}");
    }

    #[test]
    fn container_lines_are_attributed_to_their_package() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let packages: Vec<Package> = ["htop", "fzf"]
            .iter()
            .filter_map(|name| catalog.get(name).cloned())
            .collect();

        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(packages);
        builder.create_container_file();

        // The file handed to podman is plain text: no escapes, no styling.
        let file = builder.get_container_file();
        assert!(
            !file.contains('\u{1b}'),
            "the containerfile carries no escape codes"
        );
        let rejoined = format!(
            "{}\n",
            builder
                .container_lines()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(rejoined, file);

        let owner_of = |needle: &str| -> Vec<String> {
            builder
                .container_lines()
                .iter()
                .find(|line| line.text.contains(needle))
                .unwrap_or_else(|| panic!("no line containing {needle:?}"))
                .packages
                .clone()
        };

        // Scaffold lines belong to nobody; lines a package needs belong to that package.
        assert!(owner_of("FROM ").is_empty());
        assert_eq!(owner_of("ENV TOOLBOX_PREFIX"), vec!["fzf".to_string()]);
        assert_eq!(owner_of("ENV PATH="), vec!["fzf".to_string()]);
        assert_eq!(owner_of("chown -R 1000:1000"), vec!["fzf".to_string()]);

        // The package-manager layer tags the line each package occupies.
        assert_eq!(
            owner_of("install -y --no-install-recommends"),
            Vec::<String>::new()
        );
        assert_eq!(owner_of("htop \\"), vec!["htop".to_string()]);

        // Script packages own their comment and every line of their script, including
        // the continuation lines of a multi-line command.
        assert_eq!(owner_of("# --- install fzf ---"), vec!["fzf".to_string()]);
        assert_eq!(
            owner_of("RUN rm -rf /opt/toolbox/fzf"),
            vec!["fzf".to_string()]
        );
        assert_eq!(
            owner_of("--no-update-rc"),
            vec!["fzf".to_string()],
            "continuation lines belong to the package too"
        );
    }

    #[test]
    fn versions_and_architectures_are_expanded_in_scripts() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let neovim = catalog.get("neovim").unwrap().clone();

        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![neovim.clone()]);
        builder.create_container_file();
        let default = builder.get_container_file();
        assert!(
            default.contains("nvim-linux-$(uname -m | grep -qE 'aarch64|arm64' && echo arm64 || echo x86_64).tar.gz"),
            "the arch is resolved at build time: {default}"
        );
        assert!(default.contains("/v0.12.1/"), "catalog version: {default}");
        assert!(
            !default.contains("{{"),
            "no placeholders are left: {default}"
        );
        let tag = builder.image_tag().unwrap().to_string();

        // A chosen version changes the command and the tag.
        builder.set_versions(std::collections::BTreeMap::from([(
            "neovim".to_string(),
            "0.11.3".to_string(),
        )]));
        builder.create_container_file();
        let chosen = builder.get_container_file();
        assert!(chosen.contains("/v0.11.3/"), "{chosen}");
        assert!(!chosen.contains("/v0.12.1/"), "{chosen}");
        assert_ne!(builder.image_tag().unwrap(), tag, "tag follows the version");
        assert_eq!(builder.version_of(&neovim), "0.11.3");
    }

    #[test]
    fn script_installs_share_the_toolbox_prefix() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = ros("jazzy");
        let packages: Vec<Package> = ["fzf", "nvm", "rust"]
            .iter()
            .filter_map(|name| catalog.get(name).cloned())
            .collect();

        let mut builder = test_builder();
        builder.set_distro(distro);
        builder.set_packages(packages);
        builder.create_container_file();
        let file = builder.get_container_file();

        // Every toolchain lives under one directory that the image hands to the user.
        for path in [
            "/opt/toolbox/cargo",
            "/opt/toolbox/rust",
            "/opt/toolbox/nvm",
            "/opt/toolbox/fzf",
        ] {
            assert!(file.contains(path), "{path} missing from:\n{file}");
        }
        assert!(file.contains("ENV TOOLBOX_PREFIX=/opt/toolbox"));
        assert!(file.contains("RUN chown -R 1000:1000 /opt/toolbox\n"));

        // The fzf recipe no longer targets the hidden /opt/.fzf directory.
        assert!(!file.contains("/opt/.fzf"), "{file}");
    }

    #[test]
    fn images_can_resolve_their_own_hostname() {
        let mut builder = test_builder();
        builder.set_packages(vec![]);

        // Debian/Ubuntu bases need `nss-myhostname`, otherwise `sudo` complains with
        // "unable to resolve host <container>".
        for distro in [
            ros("jazzy"),
            Distro::new(
                DistroFamily::Ubuntu,
                "24.04",
                "docker.io/library/ubuntu:24.04",
                DistroSource::Predefined,
            ),
        ] {
            builder.set_distro(distro.clone());
            builder.create_container_file();
            let file = builder.get_container_file();
            assert!(
                file.contains("    libnss-myhostname \\"),
                "{distro:?} installs the resolver module:\n{file}"
            );
            assert!(
                file.contains("myhostname /' /etc/nsswitch.conf"),
                "{distro:?} enables it in nsswitch.conf:\n{file}"
            );
            assert!(
                file.contains("getent hosts \"$(hostname)\" >/dev/null"),
                "{distro:?} verifies it at build time:\n{file}"
            );
        }

        // A selection-less image still gets the fix (the layer exists for it).
        assert!(
            builder
                .get_container_file()
                .contains("install -y --no-install-recommends")
        );

        // Fedora images ship `nss-myhostname` with systemd.
        builder.set_distro(fedora());
        builder.create_container_file();
        let file = builder.get_container_file();
        assert!(!file.contains("libnss-myhostname"), "{file}");
        assert!(!file.contains("nsswitch.conf"), "{file}");
    }

    #[test]
    fn man_pages_are_unminimized_only_when_requested() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))])]);
        builder.create_container_file();
        assert!(!builder.get_container_file().contains("unminimize"));

        builder.set_packages(vec![package(
            "manpages",
            &[(DEFAULT_INSTALL_KEY, pkg("manpages"))],
        )]);
        builder.create_container_file();
        assert!(
            builder
                .get_container_file()
                .contains("RUN yes | unminimize")
        );
    }

    #[test]
    fn packages_unavailable_on_the_distro_are_skipped() {
        let mut builder = test_builder();
        builder.set_distro(ros("jazzy"));
        builder.set_packages(vec![
            package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))]),
            package("only-on-lyrical", &[("lyrical", pkg("nope"))]),
        ]);
        builder.create_container_file();
        assert!(builder.get_container_file().contains("htop"));
        assert!(!builder.get_container_file().contains("nope"));
    }

    #[test]
    fn bundled_catalog_renders_for_every_distro() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        for distro in Distro::predefined() {
            let available = catalog.available_on(Some(&distro));
            assert!(!available.is_empty(), "no packages for {distro:?}");

            let mut builder = test_builder();
            builder.set_distro(distro.clone());
            builder.set_packages(available.into_iter().cloned().collect());
            builder.create_container_file();

            let file = builder.get_container_file();
            assert!(file.contains(&distro.image));
            let manager = distro.family.package_manager().command();
            assert!(file.contains(manager), "{manager} missing for {distro:?}");
            for line in file.lines() {
                if line.contains('\\') {
                    assert!(line.ends_with('\\'), "malformed continuation: {line:?}");
                }
            }
        }
    }
}
