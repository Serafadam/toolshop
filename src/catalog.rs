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
use serde::Deserialize;

use crate::distro::Distro;

/// Catalog compiled into the binary, used when no external directory is present.
static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/packages");

/// Install entry used for distros that have no explicit entry.
pub const DEFAULT_INSTALL_KEY: &str = "default";

/// Environment variable pointing at an additional catalog directory.
pub const PACKAGES_DIR_ENV: &str = "TOOLSHOP_PACKAGES_DIR";

/// Category used for packages that do not declare one.
pub const DEFAULT_CATEGORY: &str = "Other";

/// Display order of the package categories; unknown ones are appended alphabetically.
pub const CATEGORY_ORDER: [&str; 10] = [
    "Core", "Shell", "Files", "Network", "Git", "Editors", "C/C++", "Rust", "Python", "Go",
];

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
                InstallSpec::Script { script }
            }
        };
        install.insert(distro, spec);
    }

    Ok(Package {
        name,
        version: file.version,
        summary: file.summary,
        category: file.category.trim().to_string(),
        requires: file.requires,
        install,
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
    fn every_script_package_is_reinstallable() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = distro(DistroFamily::Ubuntu, "24.04");

        for package in &catalog.packages {
            let Some(InstallSpec::Script { script }) = package.install_for(&distro) else {
                continue;
            };
            // Anything that writes to a fixed path must clear it first, otherwise building
            // on top of an image that already has the tool fails.
            let writes_fixed_path = ["/opt/", "/usr/local/", "$NVM_DIR", "$CARGO_HOME"]
                .iter()
                .any(|prefix| script.contains(prefix));
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
