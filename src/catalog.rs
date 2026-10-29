//! Package catalog loaded from TOML files.
//!
//! The catalog is baked into the binary from `packages/` and can be extended or
//! overridden by external directories, so new packages never require a rebuild.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use color_eyre::{
    Result,
    eyre::{WrapErr, bail, eyre},
};
use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};

use crate::distro::Distro;

/// Catalog compiled into the binary, used when no external directory is present.
static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/packages");

/// Install entry used for distros that have no explicit entry.
pub const DEFAULT_INSTALL_KEY: &str = "default";

/// Environment variable pointing at an additional catalog directory.
pub const PACKAGES_DIR_ENV: &str = "TOOLSHOP_PACKAGES_DIR";

/// Placeholder for the effective package version inside a script install.
pub const VERSION_PLACEHOLDER: &str = "{{version}}";

/// Architecture placeholders look like `{{arch:x86_64,arm64}}`: the first value is used on
/// x86_64, the second on arm64.
const ARCH_PREFIX: &str = "{{arch:";
const ARCH_SUFFIX: &str = "}}";

/// Category used for packages that do not declare one.
pub const DEFAULT_CATEGORY: &str = "Other";

/// Display order of the package categories; unknown ones are appended alphabetically.
pub const CATEGORY_ORDER: [&str; 12] = [
    "Core",
    "Shell",
    "Files",
    "Network",
    "Git",
    "Editors",
    "C/C++",
    "Rust",
    "Python",
    "Go",
    "Graphics",
    "Simulation",
];

/// Packages that need a GPU with a working Vulkan driver at run time.
pub const GPU_PACKAGES: [&str; 2] = ["isaac-sim", "ignition-gazebo"];

/// Package shipping the Vulkan ICD manifest a driver inside a container needs.
pub const NVIDIA_VULKAN_ICD: &str = "nvidia-vulkan-icd";

/// Warn when `packages` include something that renders but the image cannot find the driver.
///
/// A container gets the driver's user-space libraries from the runtime, but not the Vulkan
/// ICD manifest that points the loader at them — without it those tools fail with
/// `VkResult: ERROR_INCOMPATIBLE_DRIVER` (Isaac Sim then says "GPU Foundation is not
/// initialized"). The catalog's `nvidia-vulkan-icd` package writes that manifest.
pub fn vulkan_hint<T: AsRef<str>>(packages: &[T]) -> Option<String> {
    let has_gpu_tool = packages
        .iter()
        .any(|name| GPU_PACKAGES.contains(&name.as_ref()));
    let has_icd = packages
        .iter()
        .any(|name| name.as_ref() == NVIDIA_VULKAN_ICD);
    (has_gpu_tool && !has_icd).then(|| {
        format!(
            "tip: this image has no Vulkan driver manifest, so GPU tools will report \
             ERROR_INCOMPATIBLE_DRIVER — add the `{NVIDIA_VULKAN_ICD}` (and `vulkan-tools`) \
             package and rebuild"
        )
    })
}

/// Sort key putting known categories in [`CATEGORY_ORDER`] and the rest last.
pub fn category_rank(category: &str) -> (usize, String) {
    match CATEGORY_ORDER
        .iter()
        .position(|known| known.eq_ignore_ascii_case(category))
    {
        Some(index) => (index, String::new()),
        None => (CATEGORY_ORDER.len(), category.to_ascii_lowercase()),
    }
}

/// How a package is installed on one particular distro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSpec {
    /// Installed through the distro package manager.
    PackageManager { pkg: String },
    /// Installed by running a shell command.
    Script { script: String },
}

impl InstallSpec {
    /// Short description for the UI: the package manager name, or `script`.
    pub fn describe(&self) -> &str {
        match self {
            InstallSpec::PackageManager { pkg } => pkg.as_str(),
            InstallSpec::Script { .. } => "script",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub summary: String,
    /// Grouping shown in the package list, e.g. `Core` or `C/C++`.
    pub category: String,
    /// Names of packages that must be selected alongside this one.
    pub requires: Vec<String>,
    /// Install instructions keyed by distro name, plus [`DEFAULT_INSTALL_KEY`].
    pub install: BTreeMap<String, InstallSpec>,
    /// Environment variables the image needs for this package (emitted as `ENV` lines).
    pub env: BTreeMap<String, String>,
}

impl Package {
    /// Install instructions for `distro`, looked up as: distro name, then distro
    /// family (`ros`, `ubuntu`, `debian`, `fedora`), then `default`.
    pub fn install_for(&self, distro: &Distro) -> Option<&InstallSpec> {
        self.install
            .get(&distro.name)
            .or_else(|| self.install.get(distro.family.key()))
            .or_else(|| self.install.get(DEFAULT_INSTALL_KEY))
    }

    pub fn is_available_on(&self, distro: &Distro) -> bool {
        self.install_for(distro).is_some()
    }

    /// List label: the package name, plus how the distro installs it when that
    /// differs from the name (a different package name, or a script).
    pub fn label_for(&self, distro: &Distro) -> String {
        match self.install_for(distro) {
            Some(InstallSpec::PackageManager { pkg }) if pkg != &self.name => {
                format!("{} ({pkg})", self.name)
            }
            Some(InstallSpec::Script { .. }) => format!("{} (script)", self.name),
            _ => self.name.clone(),
        }
    }
}

/// How a package is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    /// Through the distro package manager.
    PackageManager,
    /// By running a shell command.
    Script,
}

impl InstallMethod {
    pub fn label(self) -> &'static str {
        match self {
            InstallMethod::PackageManager => "package manager",
            InstallMethod::Script => "script",
        }
    }

    /// The other method.
    pub fn flipped(self) -> Self {
        match self {
            InstallMethod::PackageManager => InstallMethod::Script,
            InstallMethod::Script => InstallMethod::PackageManager,
        }
    }
}

/// A user-supplied install instruction that replaces the catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallOverride {
    pub method: InstallMethod,
    /// Package name, or the shell command when `method` is [`InstallMethod::Script`].
    pub value: String,
}

/// Install instruction for `package` on `distro`, honouring a user override.
pub fn resolve_install(
    package: &Package,
    distro: &Distro,
    overrides: &BTreeMap<String, InstallOverride>,
) -> Option<InstallSpec> {
    match overrides.get(&package.name) {
        Some(over) if !over.value.trim().is_empty() => Some(match over.method {
            InstallMethod::PackageManager => InstallSpec::PackageManager {
                pkg: over.value.clone(),
            },
            InstallMethod::Script => InstallSpec::Script {
                script: over.value.clone(),
            },
        }),
        Some(_) => None,
        None => package.install_for(distro).cloned(),
    }
}

/// True for a name that can be used as an environment variable (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn is_valid_env_name(key: &str) -> bool {
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.starts_with(|c: char| c.is_ascii_digit())
}

/// Check a user-supplied `ENV` pair, returning what is wrong with it.
///
/// Values may contain spaces (unlike the catalog's own `[env]` tables, which are single
/// tokens), but never a newline: that would end the `ENV` instruction and split the
/// Containerfile.
pub fn validate_env(key: &str, value: &str) -> Result<(), String> {
    if !is_valid_env_name(key) {
        return Err(format!(
            "{key:?} is not a usable variable name (letters, digits and _, not starting with a digit)"
        ));
    }
    if value.contains(['\n', '\r']) {
        return Err(format!("{key}: the value must stay on one line"));
    }
    Ok(())
}

/// Version to use for `package`: the user's choice when set, else the catalog entry.
pub fn effective_version<'a>(
    package: &'a Package,
    versions: &'a BTreeMap<String, String>,
) -> &'a str {
    versions
        .get(&package.name)
        .map(String::as_str)
        .unwrap_or(&package.version)
}

/// Whether a package's install command can take a user-selected version.
pub fn accepts_version(package: &Package) -> bool {
    package
        .install
        .values()
        .any(|spec| matches!(spec, InstallSpec::Script { script } if script.contains(VERSION_PLACEHOLDER)))
}

impl InstallSpec {
    /// The script with `{{version}}` and `{{arch:…}}` expanded, or `None` for `pkg` installs.
    pub fn script_for(&self, version: &str) -> Option<String> {
        match self {
            InstallSpec::Script { script } => Some(expand_script(script, version)),
            InstallSpec::PackageManager { .. } => None,
        }
    }
}

/// Expand the placeholders a script install may contain.
///
/// `{{version}}` becomes `version`; `{{arch:x86_64,arm64}}` becomes a shell expression that
/// picks the right token for the machine podman builds on, so one recipe covers both
/// architectures.
pub fn expand_script(script: &str, version: &str) -> String {
    let mut expanded = script.replace(VERSION_PLACEHOLDER, version);

    while let Some(start) = expanded.find(ARCH_PREFIX) {
        let Some(rest) = expanded.get(start + ARCH_PREFIX.len()..) else {
            break;
        };
        let Some(end) = rest.find(ARCH_SUFFIX) else {
            break;
        };
        let tokens = &rest[..end];
        let (x86, arm) = tokens.split_once(',').unwrap_or((tokens, tokens));
        let replacement = format!(
            "$(uname -m | grep -qE 'aarch64|arm64' && echo {} || echo {})",
            arm.trim(),
            x86.trim()
        );
        expanded.replace_range(
            start..start + ARCH_PREFIX.len() + end + ARCH_SUFFIX.len(),
            &replacement,
        );
    }

    expanded
}

/// All known packages, sorted by name.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    pub packages: Vec<Package>,
    /// Non-fatal problems (unreadable or invalid files, unknown requirements).
    pub warnings: Vec<String>,
}

impl Catalog {
    /// Load the bundled catalog plus every external catalog directory.
    pub fn load() -> Result<Self> {
        Self::load_from_dirs(&search_dirs())
    }

    /// Load the bundled catalog plus the given directories, in increasing priority.
    pub fn load_from_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut catalog = Catalog::default();
        let mut merged: BTreeMap<String, Package> = BTreeMap::new();

        for file in BUNDLED.files() {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let contents = file
                .contents_utf8()
                .ok_or_else(|| eyre!("bundled package {} is not valid UTF-8", path.display()))?;
            let package = parse_package(contents)
                .wrap_err_with(|| format!("bundled package {} is invalid", path.display()))?;
            merged.insert(package.name.clone(), package);
        }

        for dir in dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    catalog
                        .warnings
                        .push(format!("cannot read {}: {err}", dir.display()));
                    continue;
                }
            };

            let mut files: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            files.sort();

            for path in files {
                let contents = match std::fs::read_to_string(&path) {
                    Ok(contents) => contents,
                    Err(err) => {
                        catalog
                            .warnings
                            .push(format!("cannot read {}: {err}", path.display()));
                        continue;
                    }
                };

                match parse_package(&contents) {
                    Ok(package) => {
                        let name = package.name.clone();
                        if merged
                            .get(&name)
                            .is_some_and(|existing| existing != &package)
                        {
                            catalog
                                .warnings
                                .push(format!("{name} overridden by {}", path.display()));
                        }
                        merged.insert(name, package);
                    }
                    Err(err) => catalog
                        .warnings
                        .push(format!("skipping {}: {err}", path.display())),
                }
            }
        }

        if merged.is_empty() {
            bail!("no packages found in the catalog");
        }

        catalog.packages = merged.into_values().collect();
        catalog.sort_and_validate();
        Ok(catalog)
    }

    fn sort_and_validate(&mut self) {
        self.packages.sort_by(|a, b| a.name.cmp(&b.name));

        let known: Vec<String> = self.packages.iter().map(|p| p.name.clone()).collect();
        for package in &self.packages {
            for requirement in &package.requires {
                if !known.contains(requirement) {
                    self.warnings.push(format!(
                        "{} requires unknown package {requirement}",
                        package.name
                    ));
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|package| package.name == name)
    }

    /// Packages selectable on `distro` (or on any distro when `None`).
    pub fn available_on(&self, distro: Option<&Distro>) -> Vec<&Package> {
        match distro {
            Some(distro) => self
                .packages
                .iter()
                .filter(|package| package.is_available_on(distro))
                .collect(),
            None => self.packages.iter().collect(),
        }
    }
}

/// Catalog directories in increasing priority.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("packages")];

    if let Some(config) = config_dir() {
        dirs.push(config.join("toolshop").join("packages"));
    }

    if let Some(dir) = std::env::var_os(PACKAGES_DIR_ENV) {
        dirs.push(PathBuf::from(dir));
    }

    dirs
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".config"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageFile {
    name: String,
    /// `[env]` table: variables the generated image exports for this package.
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    summary: String,
    #[serde(default = "default_category")]
    category: String,
    #[serde(default)]
    requires: Vec<String>,
    install: BTreeMap<String, RawInstall>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawInstall {
    PackageManager { pkg: String },
    Script { script: String },
}

fn default_category() -> String {
    DEFAULT_CATEGORY.to_string()
}

/// Category of `package`, falling back to [`DEFAULT_CATEGORY`].
pub fn category_of(package: &Package) -> &str {
    if package.category.trim().is_empty() {
        DEFAULT_CATEGORY
    } else {
        &package.category
    }
}

fn parse_package(contents: &str) -> Result<Package> {
    let file: PackageFile = toml::from_str(contents)?;

    let name = file.name.trim().to_string();
    if name.is_empty() {
        bail!("package is missing a `name`");
    }
    if name.contains(char::is_whitespace) {
        bail!("package name {name:?} must not contain whitespace");
    }
    if file.install.is_empty() {
        bail!("{name} has no `[install.<distro>]` entries");
    }

    let mut install = BTreeMap::new();
    for (distro, spec) in file.install {
        let spec = match spec {
            RawInstall::PackageManager { pkg } => {
                if pkg.trim().is_empty() {
                    bail!("{name} has an empty `pkg` for distro {distro}");
                }
                InstallSpec::PackageManager { pkg }
            }
            RawInstall::Script { script } => {
                if script.trim().is_empty() {
                    bail!("{name} has an empty `script` for distro {distro}");
                }
                if script.contains(VERSION_PLACEHOLDER) && file.version.trim().is_empty() {
                    bail!(
                        "{name} uses {VERSION_PLACEHOLDER} in its script but declares no \
                         `version` to fall back on"
                    );
                }
                let mut rest = script.as_str();
                while let Some((_, after)) = rest.split_once(ARCH_PREFIX) {
                    let Some((tokens, tail)) = after.split_once(ARCH_SUFFIX) else {
                        bail!(
                            "{name} has an unterminated {ARCH_PREFIX}…{ARCH_SUFFIX} placeholder \
                             for distro {distro}"
                        );
                    };
                    if !tokens.contains(',') {
                        bail!(
                            "{name} needs two comma separated architectures in \
                             {ARCH_PREFIX}{tokens}{ARCH_SUFFIX}"
                        );
                    }
                    rest = tail;
                }
                InstallSpec::Script { script }
            }
        };
        install.insert(distro, spec);
    }

    for (key, value) in &file.env {
        if !is_valid_env_name(key) {
            bail!("{name} has an invalid env variable name {key:?}");
        }
        if value.trim().is_empty() || value.contains(char::is_whitespace) {
            bail!("{name} has an unusable value for env {key}");
        }
    }

    Ok(Package {
        name,
        version: file.version,
        summary: file.summary,
        category: file.category.trim().to_string(),
        requires: file.requires,
        install,
        env: file.env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distro::{DistroFamily, DistroSource};
    use std::collections::BTreeMap;

    fn bundled_names() -> Vec<String> {
        const BUNDLED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/packages");
        Catalog::load_from_dirs(&[PathBuf::from(BUNDLED_DIR)])
            .expect("bundled catalog loads")
            .packages
            .into_iter()
            .map(|package| package.name)
            .collect()
    }

    #[test]
    fn bundled_catalog_is_valid() {
        let names = bundled_names();
        assert!(names.len() >= 18, "unexpected catalog: {names:?}");
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "packages should be sorted by name");
    }

    #[test]
    fn bundled_catalog_has_no_warnings() {
        const BUNDLED_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/packages");
        let catalog = Catalog::load_from_dirs(&[PathBuf::from(BUNDLED_DIR)]).unwrap();
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
    }

    fn distro(family: DistroFamily, name: &str) -> Distro {
        Distro::new(
            family,
            name,
            format!("example.invalid/{name}:latest"),
            DistroSource::Local,
        )
    }

    #[test]
    fn install_specs_cascade_from_distro_to_family_to_default() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let clangd = catalog.get("clangd").unwrap();

        // Exact distro name wins.
        assert_eq!(
            clangd.install_for(&distro(DistroFamily::Ros, "jazzy")),
            Some(&InstallSpec::PackageManager {
                pkg: "clangd-18".to_string()
            })
        );
        assert_eq!(
            clangd.label_for(&distro(DistroFamily::Ros, "kilted")),
            "clangd (clangd-19)"
        );

        // Then the family key, then `default`.
        assert_eq!(
            clangd.install_for(&distro(DistroFamily::Fedora, "41")),
            Some(&InstallSpec::PackageManager {
                pkg: "clang-tools-extra".to_string()
            })
        );
        assert_eq!(
            clangd.install_for(&distro(DistroFamily::Ubuntu, "24.04")),
            Some(&InstallSpec::PackageManager {
                pkg: "clangd".to_string()
            })
        );

        // The fluent API returns None for packages with no matching spec.
        let never = Package {
            name: "never".to_string(),
            version: String::new(),
            summary: String::new(),
            category: "Test".to_string(),
            requires: vec![],
            install: BTreeMap::from([(
                "arity".to_string(),
                InstallSpec::PackageManager {
                    pkg: "arity".to_string(),
                },
            )]),
            env: BTreeMap::new(),
        };
        assert!(!never.is_available_on(&distro(DistroFamily::Ros, "jazzy")));
    }

    #[test]
    fn labels_show_package_names_and_scripts() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();

        let ssh = catalog.get("ssh").unwrap();
        assert_eq!(
            ssh.label_for(&distro(DistroFamily::Ubuntu, "24.04")),
            "ssh (openssh-client)"
        );
        assert_eq!(
            ssh.label_for(&distro(DistroFamily::Fedora, "41")),
            "ssh (openssh-clients)"
        );

        let rust = catalog.get("rust").unwrap();
        assert_eq!(
            rust.label_for(&distro(DistroFamily::Ubuntu, "24.04")),
            "rust (script)"
        );

        let htop = catalog.get("htop").unwrap();
        assert_eq!(htop.label_for(&distro(DistroFamily::Fedora, "41")), "htop");
    }

    #[test]
    fn packages_from_the_old_install_script_are_available() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        for name in [
            "bat",
            "build-essential",
            "clang-format",
            "fd-find",
            "fastfetch",
            "gettext",
            "golang",
            "lazygit",
            "libusb",
            "lsd",
            "man",
            "manpages",
            "ninja",
            "python-venv",
            "software-properties-common",
            "stow",
            "tpm",
            "unzip",
        ] {
            assert!(catalog.get(name).is_some(), "missing {name}");
        }
    }

    #[test]
    fn script_installs_land_under_opt_toolbox_and_are_idempotent() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();

        let fzf = catalog.get("fzf").unwrap();
        let Some(InstallSpec::Script { script }) =
            fzf.install_for(&distro(DistroFamily::Ubuntu, "24.04"))
        else {
            panic!("fzf should install from a script");
        };
        assert!(
            script.contains("/opt/toolbox/fzf"),
            "fzf goes to /opt/toolbox/fzf: {script}"
        );
        assert!(
            script.trim_start().starts_with("rm -rf /opt/toolbox/fzf"),
            "the target is cleared first, so an image that already has fzf still builds: {script}"
        );
        assert!(
            script.contains("--no-update-rc"),
            "root's rc files are left alone: {script}"
        );
        assert!(
            !script.contains("make install"),
            "no Go toolchain is needed for a prebuilt fzf: {script}"
        );
        // `install` derives its target from the working directory, so it must run there.
        assert!(
            script.contains("cd /opt/toolbox/fzf && ./install --all"),
            "install runs from the fzf checkout: {script}"
        );

        let nvm = catalog.get("nvm").unwrap();
        let Some(InstallSpec::Script { script }) =
            nvm.install_for(&distro(DistroFamily::Ubuntu, "24.04"))
        else {
            panic!("nvm should install from a script");
        };
        assert!(script.contains("$NVM_DIR"), "nvm uses $NVM_DIR: {script}");
        assert_eq!(category_of(nvm), "Node");
        assert!(nvm.requires.iter().any(|req| req == "git"));

        let rust = catalog.get("rust").unwrap();
        let Some(InstallSpec::Script { script }) =
            rust.install_for(&distro(DistroFamily::Ros, "jazzy"))
        else {
            panic!("rust should install from a script");
        };
        assert!(
            script.contains("--no-modify-path"),
            "rustup must not edit the root rc files: {script}"
        );
    }

    #[test]
    fn system_wide_pip_installs_bypass_pep_668() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = distro(DistroFamily::Ubuntu, "24.04");

        for package in &catalog.packages {
            let Some(InstallSpec::Script { script }) = package.install_for(&distro) else {
                continue;
            };
            for line in script.lines() {
                let installs_system_wide = (line.contains("uv pip install")
                    || line.contains("pip3 install"))
                    && line.contains("--system");
                if installs_system_wide {
                    assert!(
                        line.contains("--break-system-packages"),
                        "{} installs into the system interpreter without \
                         --break-system-packages, which Debian/Ubuntu refuse: {line}",
                        package.name
                    );
                }
            }
        }
    }

    /// True when a shell token references one of the fixed install prefixes without being
    /// just an interpreter path (`.../bin/python`) handed to a package manager.
    fn is_fixed_path_write(token: &str) -> bool {
        let token = token.trim_matches(|c| c == '"' || c == '\'');
        let is_interpreter = token.rsplit('/').next().is_some_and(|last| {
            last.starts_with("python") || last.starts_with("pip") || last == "uv"
        });
        if is_interpreter {
            return false;
        }
        token.contains("/opt/")
            || token.contains("/usr/local/")
            || token.contains("$NVM_DIR")
            || token.contains("$CARGO_HOME")
    }

    #[test]
    fn fixed_path_writes_are_recognised() {
        for write in [
            "/opt/toolbox/foo",
            "\"/opt/toolbox/foo\"",
            "$NVM_DIR/nvm.sh",
            "/usr/local/bin/tool",
            "$CARGO_HOME/bin/cargo",
            "$NVM_DIR",
        ] {
            assert!(is_fixed_path_write(write), "{write} writes a fixed path");
        }
        // Interpreter paths handed to a package manager are not directory writes.
        for benign in [
            "/opt/toolbox/isaacsim/venv/bin/python",
            "--python",
            "python3",
        ] {
            assert!(!is_fixed_path_write(benign), "{benign} is not a write");
        }
    }

    #[test]
    fn every_script_package_is_reinstallable() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = distro(DistroFamily::Ubuntu, "24.04");

        for package in &catalog.packages {
            let Some(InstallSpec::Script { script }) = package.install_for(&distro) else {
                continue;
            };
            // Anything that writes to a fixed path must clear it first, otherwise building
            // on top of an image that already has the tool fails. Installing into an existing
            // virtualenv (`--python <prefix>/bin/python`) is idempotent on its own, so those
            // interpreter paths do not count as a write.
            let writes_fixed_path = script.split_whitespace().any(is_fixed_path_write);
            if writes_fixed_path {
                assert!(
                    script.contains("rm -rf") || script.contains("install /tmp/"),
                    "{} must be re-installable: {script}",
                    package.name
                );
            }
            // No script may pipe an installer into sh without an explicit env guard.
            if script.contains("| sh") {
                assert!(
                    script.contains("RUSTUP_INIT_SKIP_PATH_CHECK") || !script.contains("rustup.rs"),
                    "{} must tolerate an existing install: {script}",
                    package.name
                );
            }
        }
    }

    #[test]
    fn cppcheck_installs_from_the_package_manager_everywhere() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let cppcheck = catalog.get("cppcheck").expect("cppcheck is in the catalog");
        assert_eq!(category_of(cppcheck), "C/C++");
        assert_eq!(
            cppcheck.install_for(&distro(DistroFamily::Ubuntu, "24.04")),
            Some(&InstallSpec::PackageManager {
                pkg: "cppcheck".to_string()
            })
        );
        for (family, name) in [
            (DistroFamily::Ros, "jazzy"),
            (DistroFamily::Ros, "kilted"),
            (DistroFamily::Ros, "lyrical"),
            (DistroFamily::Fedora, "41"),
        ] {
            assert_eq!(
                cppcheck.install_for(&distro(family, name)),
                Some(&InstallSpec::PackageManager {
                    pkg: "cppcheck".to_string()
                }),
                "{family:?} {name}"
            );
        }
        assert!(cppcheck.requires.is_empty(), "no extra packages needed");
    }

    #[test]
    fn simulators_are_offered_where_they_install() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let jazzy = distro(DistroFamily::Ros, "jazzy");
        let ubuntu = distro(DistroFamily::Ubuntu, "24.04");
        let fedora = distro(DistroFamily::Fedora, "41");
        // A custom Ubuntu derivative whose name carries no distro token.
        let other = distro(DistroFamily::Other, "dev-box");

        // Gazebo: the ROS integration package on ROS distros, OSRF's repository elsewhere.
        let gazebo = catalog.get("ignition-gazebo").expect("ignition-gazebo");
        assert_eq!(category_of(gazebo), "Simulation");
        assert_eq!(
            gazebo.install_for(&jazzy),
            Some(&InstallSpec::PackageManager {
                pkg: "ros-jazzy-ros-gz".to_string()
            })
        );
        assert_eq!(
            gazebo.install_for(&distro(DistroFamily::Ros, "kilted")),
            Some(&InstallSpec::PackageManager {
                pkg: "ros-kilted-ros-gz".to_string()
            })
        );
        let Some(InstallSpec::Script { script }) = gazebo.install_for(&ubuntu) else {
            panic!("plain Ubuntu installs from the OSRF repository");
        };
        assert!(script.contains("packages.osrfoundation.org"), "{script}");
        assert!(script.contains("gz-harmonic"), "{script}");
        assert!(
            script.contains("gz sim --versions"),
            "installs are verified: {script}"
        );
        // Images classified as "other" but Ubuntu based get the same OSRF recipe.
        assert_eq!(
            gazebo.install_for(&other),
            gazebo.install_for(&ubuntu),
            "other apt images install Gazebo like plain Ubuntu"
        );
        assert!(
            gazebo.install_for(&fedora).is_none(),
            "Gazebo is not packaged for Fedora here"
        );

        // Isaac Sim: Ubuntu only, pinned to a version that exists on PyPI (the scheme is
        // `X.Y.Z.0`), in its own Python 3.12 venv under /opt/toolbox.
        let isaac = catalog.get("isaac-sim").expect("isaac-sim");
        assert_eq!(category_of(isaac), "Simulation");
        assert_eq!(isaac.version, "6.2.0.0");
        assert!(isaac.requires.iter().any(|req| req == "uv"));
        // Running it requires accepting the Omniverse EULA; the image exports it.
        assert_eq!(
            isaac.env.get("OMNI_KIT_ACCEPT_EULA").map(String::as_str),
            Some("YES")
        );
        for distro in [&jazzy, &ubuntu, &other] {
            let Some(InstallSpec::Script { script }) = isaac.install_for(distro) else {
                panic!("{distro:?} should install Isaac Sim from a script");
            };
            assert!(
                script.contains("[ \"$(uname -m)\" = x86_64 ] || [ \"$(uname -m)\" = aarch64 ]"),
                "both supported architectures are accepted: {script}"
            );
            assert!(
                script.contains("rm -rf /opt/toolbox/isaacsim"),
                "re-runnable: {script}"
            );
            assert!(script.contains("uv venv --python 3.12"), "{script}");
            assert!(script.contains("isaacsim[all]==6.2.0.0"), "{script}");
            // Plain `isaacsim` lacks the app extensions, so simulate_app would be missing.
            assert!(!script.contains("\"isaacsim==6.2.0.0\""), "{script}");
            // Several GB of wheels from one index overwhelm uv's defaults.
            assert!(script.contains("UV_HTTP_TIMEOUT="), "{script}");
            assert!(script.contains("UV_HTTP_RETRIES="), "{script}");
            assert!(script.contains("UV_CONCURRENT_DOWNLOADS="), "{script}");
            // isaacsim-core pins its own OpenCV fork, which only NVIDIA publishes.
            assert!(
                script.contains("--extra-index-url https://pypi.nvidia.com"),
                "the OpenCV fork lives on NVIDIA's index: {script}"
            );
            assert!(
                script.contains("--index-strategy unsafe-best-match"),
                "both indexes must be searched, PyPI has a placeholder of that name: {script}"
            );
            assert!(
                script.contains("--prerelease=allow"),
                "pinned dependencies include pre-releases: {script}"
            );
            assert!(
                script.contains("ln -sf /opt/toolbox/isaacsim/venv/bin/isaacsim"),
                "{script}"
            );
            // The verification must not boot Kit: metadata only.
            assert!(
                !script.contains("import isaacsim"),
                "the check stays metadata only: {script}"
            );
            assert!(script.contains("importlib.metadata"), "{script}");
        }
        // Same for Isaac Sim: any Ubuntu-based image, including "other" ones.
        assert_eq!(
            isaac.install_for(&other),
            isaac.install_for(&ubuntu),
            "other apt images install Isaac Sim like plain Ubuntu"
        );
        assert!(isaac.install_for(&fedora).is_none());
    }

    #[test]
    fn images_with_gpu_tools_are_flagged_without_the_vulkan_icd() {
        let hint = vulkan_hint(&["uv", "isaac-sim"]).expect("isaac-sim needs the ICD");
        assert!(hint.contains(NVIDIA_VULKAN_ICD), "{hint}");
        assert!(hint.contains("ERROR_INCOMPATIBLE_DRIVER"), "{hint}");

        // Gazebo renders too, and the hint is the same one.
        assert!(vulkan_hint(&["ignition-gazebo"]).is_some());

        // With the manifest package present there is nothing to warn about.
        assert!(vulkan_hint(&["isaac-sim", NVIDIA_VULKAN_ICD]).is_none());
        // And cpu-only images stay silent.
        assert!(vulkan_hint(&["htop", "ripgrep"]).is_none());
        assert!(vulkan_hint::<&str>(&[]).is_none());
    }

    #[test]
    fn environment_variable_names_and_values_are_validated() {
        for good in ["FOO", "_FOO", "FOO_BAR", "FOO1", "a"] {
            assert!(is_valid_env_name(good), "{good} is a valid name");
        }
        for bad in ["", "1FOO", "FOO-BAR", "FOO BAR", "FOO=BAR", "FÖÖ"] {
            assert!(!is_valid_env_name(bad), "{bad} is not a valid name");
        }

        assert!(validate_env("FOO", "bar").is_ok());
        // Spaces are fine in a value (they get quoted), newlines are not.
        assert!(validate_env("FOO", "hello world").is_ok());
        assert!(validate_env("FOO", "").is_ok());
        assert!(
            validate_env("1BAD", "x")
                .unwrap_err()
                .contains("variable name")
        );
        assert!(
            validate_env("FOO", "a\nb")
                .unwrap_err()
                .contains("one line")
        );
        assert!(validate_env("FOO", "a\rb").is_err());
    }

    #[test]
    fn the_nvidia_vulkan_icd_is_shipped_for_container_gpus() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let icd = catalog.get("nvidia-vulkan-icd").expect("nvidia-vulkan-icd");

        assert_eq!(category_of(icd), "Graphics");
        // vulkan-tools brings libvulkan1, the loader that reads the manifest.
        assert_eq!(icd.requires, vec!["vulkan-tools".to_string()]);

        let distro = distro(DistroFamily::Ubuntu, "24.04");
        let Some(InstallSpec::Script { script }) = icd.install_for(&distro) else {
            panic!("nvidia-vulkan-icd must install from a script");
        };
        assert!(
            script.contains("/usr/share/vulkan/icd.d/nvidia_icd.json"),
            "{script}"
        );
        // The loader resolves this through ldconfig, so it must stay relative.
        assert!(
            script.contains("\"library_path\": \"libGLX_nvidia.so.0\""),
            "{script}"
        );
        assert!(!script.contains("/usr/lib64/libGLX"), "{script}");
        assert!(script.contains("file_format_version"), "{script}");
        // Writing the same file again is what an idempotent run looks like.
        assert!(
            script.contains("> /usr/share/vulkan/icd.d/nvidia_icd.json"),
            "{script}"
        );
    }

    #[test]
    fn isaac_sim_cache_fills_in_the_extension_caches() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let cache = catalog.get("isaac-sim-cache").expect("isaac-sim-cache");

        assert_eq!(category_of(cache), "Simulation");
        assert_eq!(cache.version, "6.2.0.0");
        assert!(cache.summary.contains("7.4 GB"), "{}", cache.summary);
        // It extends the venv that `isaac-sim` creates, and must be installed after it.
        assert_eq!(cache.requires, vec!["isaac-sim".to_string()]);

        for case in [
            distro(DistroFamily::Ros, "jazzy"),
            distro(DistroFamily::Ubuntu, "24.04"),
            distro(DistroFamily::Other, "dev-box"),
        ] {
            let Some(InstallSpec::Script { script }) = cache.install_for(&case) else {
                panic!("isaac-sim-cache must install on {case:?}");
            };
            assert!(script.contains("isaacsim[extscache]==6.2.0.0"), "{script}");
            assert!(
                script.contains("/opt/toolbox/isaacsim/venv/bin/python"),
                "it installs into the simulator's venv: {script}"
            );
            assert!(
                script.contains("--extra-index-url https://pypi.nvidia.com"),
                "{script}"
            );
            assert!(
                script.contains("--index-strategy unsafe-best-match"),
                "{script}"
            );
            assert!(script.contains("--prerelease=allow"), "{script}");
            // One of those wheels is 5.5 GB, so the HTTP budgets are widened.
            assert!(script.contains("UV_HTTP_TIMEOUT=300"), "{script}");
            assert!(script.contains("UV_CONCURRENT_DOWNLOADS=2"), "{script}");
            assert!(
                script.contains("m.version('isaacsim-extscache-kit')"),
                "the cache is verified after installing: {script}"
            );
        }

        assert!(
            cache
                .install_for(&distro(DistroFamily::Fedora, "41"))
                .is_none()
        );
    }

    #[test]
    fn genesis_installs_from_pypi_on_every_distro() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let genesis = catalog.get("genesis").expect("genesis is in the catalog");
        assert_eq!(category_of(genesis), "Simulation");
        assert_eq!(genesis.version, "1.4.5");
        assert!(genesis.requires.iter().any(|req| req == "uv"));
        assert!(genesis.env.is_empty(), "no special environment needed");

        for distro in [
            distro(DistroFamily::Ros, "jazzy"),
            distro(DistroFamily::Ubuntu, "24.04"),
            distro(DistroFamily::Fedora, "41"),
        ] {
            let Some(InstallSpec::Script { script }) = genesis.install_for(&distro) else {
                panic!("{distro:?} should install Genesis from a script");
            };
            assert!(
                script.contains(
                    "uv pip install --system --break-system-packages --no-cache \
                     \"genesis-world==1.4.5\""
                ),
                "PEP 668 must be bypassed explicitly: {script}"
            );
            assert!(script.contains("importlib.metadata"), "verified: {script}");
        }
    }

    #[test]
    fn packages_can_declare_environment_variables() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        // uv exports the switch its recipes (and the container's shells) rely on.
        let uv = catalog.get("uv").unwrap();
        assert_eq!(
            uv.env.get("UV_BREAK_SYSTEM_PACKAGES").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            uv.env.get("UV_BREAK_SYSTEM_PACKAGES").map(String::as_str),
            Some("1"),
            "Debian/Ubuntu system interpreters are externally managed (PEP 668)"
        );
        assert!(
            !uv.env.contains_key("UV_SYSTEM_PYTHON"),
            "exporting UV_SYSTEM_PYTHON makes `uv venv` warn that the flag is ignored"
        );

        // Broken declarations are reported, not silently dropped.
        let dir = std::env::temp_dir().join(format!("toolshop-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("bad-name.toml"),
            "name = \"x\"\n[env]\n\"1BAD\" = \"v\"\n[install.default]\npkg = \"x\"\n",
        )
        .unwrap();
        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("invalid env variable name")),
            "{:?}",
            catalog.warnings
        );

        std::fs::write(
            dir.join("bad-value.toml"),
            "name = \"y\"\n[env]\nGOOD = \"two words\"\n[install.default]\npkg = \"y\"\n",
        )
        .unwrap();
        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("unusable value for env GOOD")),
            "{:?}",
            catalog.warnings
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mujoco_installs_everywhere_with_a_verified_tarball() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let mujoco = catalog.get("mujoco").expect("mujoco is in the catalog");
        assert_eq!(category_of(mujoco), "Simulation");
        assert_eq!(mujoco.version, "3.17.0");
        assert!(mujoco.requires.iter().any(|req| req == "uv"));

        // PyPI works on every family, so the recipe sits in `default`.
        for distro in [
            distro(DistroFamily::Ros, "jazzy"),
            distro(DistroFamily::Ubuntu, "24.04"),
            distro(DistroFamily::Fedora, "41"),
        ] {
            let Some(InstallSpec::Script { script }) = mujoco.install_for(&distro) else {
                panic!("{distro:?} should install MuJoCo from a script");
            };
            assert!(
                script.starts_with("MUJOCO_VERSION=3.17.0"),
                "the version is pinned in one place: {script}"
            );
            assert!(
                script.contains("mujoco-${MUJOCO_VERSION}-linux-{{arch:x86_64,aarch64}}.tar.gz"),
                "one recipe for both architectures: {script}"
            );
            assert!(
                script.contains("${MUJOCO_ASSET}.sha256"),
                "the published checksum is fetched next to the tarball: {script}"
            );
            assert!(
                script.contains("sha256sum -c -"),
                "the tarball is checksum-verified: {script}"
            );
            assert!(
                script.contains("tar zx --strip-components=1 -C /opt/toolbox/mujoco"),
                "the payload lands under the toolbox prefix: {script}"
            );
            assert!(
                script.contains("ln -sf /opt/toolbox/mujoco/bin/simulate"),
                "the viewer is on PATH: {script}"
            );
            assert!(
                script.contains(
                    "uv pip install --system --break-system-packages --no-cache \
                     \"mujoco==${MUJOCO_VERSION}\""
                ),
                "the Python bindings come from PyPI, bypassing PEP 668: {script}"
            );
            assert!(
                script.contains("import mujoco"),
                "the install is verified: {script}"
            );
        }
    }

    #[test]
    fn script_placeholders_expand_to_version_and_architecture() {
        let script = "wget example.test/v{{version}}/nvim-linux-{{arch:x86_64,arm64}}.tar.gz";
        assert_eq!(
            expand_script(script, "0.11.2"),
            "wget example.test/v0.11.2/nvim-linux-$(uname -m | grep -qE 'aarch64|arm64' && echo arm64 || echo x86_64).tar.gz"
        );

        // Several placeholders, and scripts without any, are handled.
        let two = "{{arch:x86_64,aarch64}} and {{arch:amd64,arm64}}";
        let expanded = expand_script(two, "");
        assert_eq!(expanded.matches("uname -m").count(), 2, "{expanded}");
        assert_eq!(expand_script("no placeholders", "1.2.3"), "no placeholders");

        // The architecture expression has to survive being embedded in a quoted URL.
        assert!(!expanded.contains('"'), "no quoting surprises: {expanded}");
    }

    #[test]
    fn versioned_packages_declare_a_default_version() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let neovim = catalog.get("neovim").unwrap();
        assert!(accepts_version(neovim), "neovim's script takes a version");
        assert_eq!(neovim.version, "0.12.1");
        assert!(neovim.install.values().all(
            |spec| matches!(spec, InstallSpec::Script { script } if script.contains("{{version}}"))
        ));

        // Packages installed by the package manager have nothing to choose.
        assert!(!accepts_version(catalog.get("htop").unwrap()));

        // The effective version prefers the user's choice.
        let mut versions = BTreeMap::new();
        assert_eq!(effective_version(neovim, &versions), "0.12.1");
        versions.insert("neovim".to_string(), "0.11.0".to_string());
        assert_eq!(effective_version(neovim, &versions), "0.11.0");
    }

    #[test]
    fn broken_placeholders_are_rejected() {
        let dir = std::env::temp_dir().join(format!("toolshop-ph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // `{{version}}` without a `version` to fall back on.
        std::fs::write(
            dir.join("no-version.toml"),
            "name = \"x\"\n[install.default]\nscript = \"install {{version}}\"\n",
        )
        .unwrap();
        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("declares no `version`")),
            "{:?}",
            catalog.warnings
        );

        // A single architecture is ambiguous.
        std::fs::write(
            dir.join("one-arch.toml"),
            "name = \"y\"\n[install.default]\nscript = \"fetch {{arch:x86_64}}\"\n",
        )
        .unwrap();
        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("two comma separated architectures")),
            "{:?}",
            catalog.warnings
        );

        // Unterminated placeholder.
        std::fs::write(
            dir.join("unterminated.toml"),
            "name = \"z\"\n[install.default]\nscript = \"fetch {{arch:x86_64,arm64\"\n",
        )
        .unwrap();
        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("unterminated")),
            "{:?}",
            catalog.warnings
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dev_tooling_packages_carry_their_distro_specific_names() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let ubuntu = distro(DistroFamily::Ubuntu, "24.04");
        let fedora = distro(DistroFamily::Fedora, "41");

        // Names verified against Launchpad (Ubuntu 24.04) and Repology (Fedora 41): most are
        // identical on both, these four are not.
        let overrides = [
            ("shellcheck", "shellcheck", "ShellCheck"),
            ("xz", "xz-utils", "xz"),
            ("pkg-config", "pkg-config", "pkgconf-pkg-config"),
            ("gdbserver", "gdbserver", "gdb-gdbserver"),
        ];
        for (name, on_ubuntu, on_fedora) in overrides {
            let package = catalog
                .get(name)
                .unwrap_or_else(|| panic!("{name} is in the catalog"));
            assert_eq!(
                package.install_for(&ubuntu),
                Some(&InstallSpec::PackageManager {
                    pkg: on_ubuntu.to_string()
                }),
                "{name} on Ubuntu"
            );
            assert_eq!(
                package.install_for(&fedora),
                Some(&InstallSpec::PackageManager {
                    pkg: on_fedora.to_string()
                }),
                "{name} on Fedora"
            );
        }

        // The rest install under the same name everywhere.
        for (name, category) in [
            ("clang", "C/C++"),
            ("lldb", "C/C++"),
            ("valgrind", "C/C++"),
            ("ccache", "Core"),
            ("bear", "Core"),
            ("autoconf", "Core"),
            ("automake", "Core"),
            ("libtool", "Core"),
            ("mold", "Core"),
            ("doxygen", "Core"),
            ("graphviz", "Core"),
            ("lcov", "Core"),
            ("just", "Core"),
            ("btop", "Shell"),
            ("direnv", "Shell"),
            ("jq", "Shell"),
            ("hyperfine", "Shell"),
            ("zip", "Files"),
            ("rsync", "Files"),
            ("file", "Files"),
            ("less", "Files"),
            ("tree", "Files"),
            ("git-delta", "Git"),
        ] {
            let package = catalog
                .get(name)
                .unwrap_or_else(|| panic!("{name} is in the catalog"));
            assert_eq!(category_of(package), category, "{name} category");
            for d in [&ubuntu, &fedora] {
                assert_eq!(
                    package.install_for(d),
                    Some(&InstallSpec::PackageManager {
                        pkg: name.to_string()
                    }),
                    "{name} on {d:?}"
                );
            }
        }

        // The catalog stays warn-free and alphabetical enough for the list pane.
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
        assert!(catalog.packages.len() >= 68, "{}", catalog.packages.len());
    }

    #[test]
    fn multi_package_specs_and_family_overrides_resolve() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let fedora = distro(DistroFamily::Fedora, "41");
        let ubuntu = distro(DistroFamily::Ubuntu, "24.04");

        // One entry may list several package names.
        assert_eq!(
            catalog.get("build-essential").unwrap().install_for(&fedora),
            Some(&InstallSpec::PackageManager {
                pkg: "gcc gcc-c++ make".to_string()
            })
        );
        assert_eq!(
            catalog.get("libusb").unwrap().install_for(&fedora),
            Some(&InstallSpec::PackageManager {
                pkg: "libusb1-devel".to_string()
            })
        );
        assert_eq!(
            catalog.get("man").unwrap().install_for(&fedora),
            Some(&InstallSpec::PackageManager {
                pkg: "man-db".to_string()
            })
        );

        // Script-based on Ubuntu, packaged on Fedora.
        let lazygit = catalog.get("lazygit").unwrap();
        assert!(matches!(
            lazygit.install_for(&ubuntu),
            Some(InstallSpec::Script { .. })
        ));
        assert_eq!(
            lazygit.install_for(&fedora),
            Some(&InstallSpec::PackageManager {
                pkg: "lazygit".to_string()
            })
        );
    }

    #[test]
    fn script_packages_can_be_replaced_by_family_packages() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let uv = catalog.get("uv").unwrap();
        assert!(matches!(
            uv.install_for(&distro(DistroFamily::Ros, "jazzy")),
            Some(InstallSpec::Script { .. })
        ));
        assert_eq!(
            uv.install_for(&distro(DistroFamily::Ros, "lyrical")),
            Some(&InstallSpec::PackageManager {
                pkg: "uv".to_string()
            })
        );
    }

    #[test]
    fn external_directory_overrides_bundled_package() {
        let dir = std::env::temp_dir().join(format!("toolshop-catalog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("zzz-extra.toml"),
            "name = \"zzz-extra\"\n\n[install.default]\npkg = \"zzz\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("htop.toml"),
            "name = \"htop\"\nsummary = \"patched\"\n\n[install.default]\npkg = \"htop-custom\"\n",
        )
        .unwrap();

        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(catalog.get("htop").unwrap().summary, "patched");
        assert!(catalog.get("zzz-extra").is_some());
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("htop overridden")),
            "{:?}",
            catalog.warnings
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_files_are_reported_and_skipped() {
        let dir = std::env::temp_dir().join(format!("toolshop-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken.toml"),
            "name = \"broken\"\n[install.default]\n",
        )
        .unwrap();

        let catalog = Catalog::load_from_dirs(std::slice::from_ref(&dir)).unwrap();
        assert!(!catalog.packages.iter().any(|p| p.name == "broken"));
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("skipping") && warning.contains("broken.toml")),
            "{:?}",
            catalog.warnings
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_requirements_produce_a_warning() {
        let package = parse_package(
            "name = \"x\"\nrequires = [\"nope\"]\n\n[install.default]\npkg = \"x\"\n",
        )
        .unwrap();
        let mut catalog = Catalog {
            packages: vec![package],
            warnings: vec![],
        };
        catalog.sort_and_validate();
        assert!(catalog.warnings[0].contains("unknown package nope"));
    }
}
