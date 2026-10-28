use std::{fmt, time::Duration};

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
    catalog::{InstallOverride, InstallSpec, Package, resolve_install},
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

    /// Clear the selected distro (e.g. after removing a user-added image).
    pub fn set_distro_option(&mut self, distro: Option<Distro>) {
        self.distro = distro;
    }

    pub fn distro(&self) -> Option<&Distro> {
        self.distro.as_ref()
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

        push_lines(&mut lines, &format!("FROM {}\n", distro.image), &no_owner);
        // Default the argument to the configured user, so a plain
        // `podman build -f Containerfile .` produces the right account too.
        let docker_user = if self.docker_user.trim().is_empty() {
            DEFAULT_DOCKER_USER
        } else {
            self.docker_user.trim()
        };
        push_lines(
            &mut lines,
            &format!("ARG DOCKER_USER={docker_user}\n"),
            &no_owner,
        );
        push_lines(&mut lines, "SHELL [\"/bin/bash\", \"-c\"]\n", &no_owner);

        if distro.family.package_manager() == PackageManager::Apt {
            push_lines(
                &mut lines,
                "ENV DEBIAN_FRONTEND=noninteractive\n",
                &no_owner,
            );
        }

        // Toolchains and caches live outside the home directory, so toolbox can mount the
        // host home over it without hiding them.
        push_lines(&mut lines, "ENV UV_SYSTEM_PYTHON=1\n", &no_owner);
        push_lines(
            &mut lines,
            &format!("ENV TOOLBOX_PREFIX={TOOLBOX_PREFIX}\n"),
            &no_owner,
        );
        push_lines(
            &mut lines,
            &format!("ENV CARGO_HOME={CARGO_HOME}\n"),
            &no_owner,
        );
        push_lines(
            &mut lines,
            &format!("ENV RUSTUP_HOME={RUSTUP_HOME}\n"),
            &no_owner,
        );
        push_lines(&mut lines, &format!("ENV NVM_DIR={NVM_DIR}\n"), &no_owner);
        push_lines(
            &mut lines,
            &format!(
                "ENV PATH=\"${{CARGO_HOME}}/bin:${{RUSTUP_HOME}}/bin:${{NVM_DIR}}/node/bin:{FZF_PREFIX}/bin:${{PATH}}\"\n"
            ),
            &no_owner,
        );
        push_lines(
            &mut lines,
            &format!(
                "RUN mkdir -p ${{CARGO_HOME}} ${{RUSTUP_HOME}} ${{NVM_DIR}} {FZF_PREFIX}/bin\n"
            ),
            &no_owner,
        );

        // Package-manager installs are grouped into a single cache-friendly layer.
        let manager_packages: Vec<(&Package, &str)> = resolved
            .iter()
            .filter_map(|(package, spec)| match spec {
                InstallSpec::PackageManager { pkg } => Some((*package, pkg.as_str())),
                InstallSpec::Script { .. } => None,
            })
            .collect();

        // Debian/Ubuntu bases do not resolve their own hostname; Fedora images already
        // ship `nss-myhostname`, so this is only needed for apt families.
        let hostname_fix = distro.family.package_manager() == PackageManager::Apt;

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
        if distro.family.package_manager() == PackageManager::Apt
            && self
                .packages
                .iter()
                .any(|package| MAN_PACKAGES.contains(&package.name.as_str()))
        {
            push_lines(&mut lines, "RUN yes | unminimize\n", &no_owner);
        }

        // Scripts run afterwards, so they can rely on the packages installed above.
        for (package, spec) in &resolved {
            let InstallSpec::Script { script } = spec else {
                continue;
            };
            let owner = std::slice::from_ref(&package.name);
            push_lines(
                &mut lines,
                &format!("\n# --- install {} ---\n", package.name),
                owner,
            );
            push_lines(&mut lines, &format!("RUN {script}\n"), owner);
        }

        // Hand the image to the host user, as the toolbox workflow expects: the
        // `ubuntu` account in the base image is renamed and the toolchain is its own.
        push_lines(
            &mut lines,
            &format!("\nRUN chown -R 1000:1000 {TOOLBOX_PREFIX}\n"),
            &no_owner,
        );
        // Make uid/gid 1000 belong to $DOCKER_USER, whatever the base image ships:
        // `toolbox create` runs `useradd --uid 1000 <host user>` inside the container and
        // fails with "UID 1000 is not unique" when the image already owns that id under
        // another name (ubuntu images ship an `ubuntu` user on 1000). Renaming it keeps the
        // account (home, shell) reusable, and creating it covers images without one.
        if distro.family.package_manager() == PackageManager::Apt
            || matches!(distro.family, DistroFamily::Fedora)
        {
            push_lines(&mut lines, USER_STEP, &no_owner);
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
        self.image_tag = Some(Manifest::image_tag(distro, &self.packages));
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

        let mut child = tokio::process::Command::new("podman")
            .args([
                "build",
                "-t",
                &image,
                "--build-arg",
                &format!("DOCKER_USER={}", self.docker_user),
                "-f",
                CONTAINER_FILE,
                ".",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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

        let expected = "\
FROM docker.io/library/ros:jazzy-ros-base
ARG DOCKER_USER=ubuntu
SHELL [\"/bin/bash\", \"-c\"]
ENV DEBIAN_FRONTEND=noninteractive
ENV UV_SYSTEM_PYTHON=1
ENV TOOLBOX_PREFIX=/opt/toolbox
ENV CARGO_HOME=/opt/toolbox/cargo
ENV RUSTUP_HOME=/opt/toolbox/rust
ENV NVM_DIR=/opt/toolbox/nvm
ENV PATH=\"${CARGO_HOME}/bin:${RUSTUP_HOME}/bin:${NVM_DIR}/node/bin:/opt/toolbox/fzf/bin:${PATH}\"
RUN mkdir -p ${CARGO_HOME} ${RUSTUP_HOME} ${NVM_DIR} /opt/toolbox/fzf/bin
RUN apt-get update && apt-get install -y --no-install-recommends \\
    libnss-myhostname \\
    htop \\
    iputils-ping \\
 && rm -rf /var/lib/apt/lists/*

RUN sed -i -E 's/^(hosts:[[:space:]]+)/\\1myhostname /' /etc/nsswitch.conf \\
    && getent hosts \"$(hostname)\" >/dev/null

RUN chown -R 1000:1000 /opt/toolbox

RUN set -eu; target=\"${DOCKER_USER}\"; \\
    if command -v getent >/dev/null; then \\
        owner=\"$(getent passwd 1000 | cut -d: -f1 || true)\"; \\
        if [ -n \"$owner\" ] && [ \"$owner\" != \"$target\" ]; then \\
            groupmod -n \"$target\" \"$(id -gn \"$owner\")\" 2>/dev/null || true; \\
            usermod -l \"$target\" -d \"/home/$target\" -m \"$owner\"; \\
        fi; \\
        if ! getent passwd \"$target\" >/dev/null; then \\
            getent group 1000 >/dev/null || groupadd -g 1000 \"$target\"; \\
            useradd -u 1000 -g 1000 -m -s /bin/bash \"$target\"; \\
        fi; \\
        getent passwd \"$target\" >/dev/null || { echo \"cannot prepare user $target for uid 1000\" >&2; exit 1; }; \\
    fi
";
        assert_eq!(builder.get_container_file(), expected);
    }

    #[test]
    fn fedora_uses_dnf() {
        let mut builder = test_builder();
        builder.set_distro(fedora());
        builder.set_packages(vec![package("htop", &[(DEFAULT_INSTALL_KEY, pkg("htop"))])]);
        builder.create_container_file();

        let expected = "\
FROM docker.io/library/fedora:41
ARG DOCKER_USER=ubuntu
SHELL [\"/bin/bash\", \"-c\"]
ENV UV_SYSTEM_PYTHON=1
ENV TOOLBOX_PREFIX=/opt/toolbox
ENV CARGO_HOME=/opt/toolbox/cargo
ENV RUSTUP_HOME=/opt/toolbox/rust
ENV NVM_DIR=/opt/toolbox/nvm
ENV PATH=\"${CARGO_HOME}/bin:${RUSTUP_HOME}/bin:${NVM_DIR}/node/bin:/opt/toolbox/fzf/bin:${PATH}\"
RUN mkdir -p ${CARGO_HOME} ${RUSTUP_HOME} ${NVM_DIR} /opt/toolbox/fzf/bin
RUN dnf install -y \\
    htop \\
 && dnf clean all

RUN chown -R 1000:1000 /opt/toolbox

RUN set -eu; target=\"${DOCKER_USER}\"; \\
    if command -v getent >/dev/null; then \\
        owner=\"$(getent passwd 1000 | cut -d: -f1 || true)\"; \\
        if [ -n \"$owner\" ] && [ \"$owner\" != \"$target\" ]; then \\
            groupmod -n \"$target\" \"$(id -gn \"$owner\")\" 2>/dev/null || true; \\
            usermod -l \"$target\" -d \"/home/$target\" -m \"$owner\"; \\
        fi; \\
        if ! getent passwd \"$target\" >/dev/null; then \\
            getent group 1000 >/dev/null || groupadd -g 1000 \"$target\"; \\
            useradd -u 1000 -g 1000 -m -s /bin/bash \"$target\"; \\
        fi; \\
        getent passwd \"$target\" >/dev/null || { echo \"cannot prepare user $target for uid 1000\" >&2; exit 1; }; \\
    fi
";
        assert_eq!(builder.get_container_file(), expected);
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
        builder.set_docker_user("adaser");
        builder.set_distro(ros("jazzy"));
        builder.create_container_file();
        let file = builder.get_container_file();

        // The generated file defaults the argument to the configured user, so a plain
        // `podman build -f container .` produces the right account without build args.
        assert!(file.contains("ARG DOCKER_USER=adaser"), "{file}");
        assert!(builder.docker_user() == "adaser");

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
        builder.create_container_file();
        let file = builder.get_container_file();

        assert!(file.contains("ENV CARGO_HOME=/opt/toolbox/cargo"));
        assert!(file.contains("ENV RUSTUP_HOME=/opt/toolbox/rust"));
        assert!(file.contains("ENV NVM_DIR=/opt/toolbox/nvm"));
        assert!(file.contains("ENV UV_SYSTEM_PYTHON=1"));
        assert!(file.contains("/opt/toolbox/fzf/bin"));
        assert!(file.contains("chown -R 1000:1000"));
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

        // Scaffold lines belong to nobody.
        assert!(owner_of("FROM ").is_empty());
        assert!(owner_of("ENV PATH=").is_empty());
        assert!(owner_of("chown -R 1000:1000").is_empty());

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
