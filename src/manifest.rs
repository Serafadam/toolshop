//! Build manifests: what each toolshop-built image contains.
//!
//! One TOML file per image, kept in the user's cache directory so a build can be
//! inspected later without starting a container.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;

use crate::{
    catalog::{InstallOverride, InstallSpec, Package, resolve_install},
    distro::Distro,
    engine::normalize_image,
};

/// Current manifest schema version.
pub const MANIFEST_VERSION: u32 = 1;

/// Repository prefix for images built by toolshop.
pub const IMAGE_REPOSITORY: &str = "toolshop";

/// Environment variable overriding the manifest directory (used by tests too).
pub const CACHE_DIR_ENV: &str = "TOOLSHOP_CACHE_DIR";

/// What was installed into one toolshop image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Image reference the build produced, e.g. `localhost/toolshop:jazzy-1a2b3c`.
    pub image: String,
    /// Image ID at build time, used to detect images rebuilt outside toolshop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_id: Option<String>,
    pub distro: ManifestDistro,
    #[serde(default)]
    pub packages: Vec<ManifestPackage>,
    /// Unix seconds when the build finished.
    pub created_at: u64,
    pub tool_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDistro {
    /// Name used for package lookups, e.g. `jazzy`.
    pub name: String,
    /// Base image the Containerfile started `FROM`.
    pub image: String,
    /// Distro family key, e.g. `ros`.
    pub family: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPackage {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// Package name when installed through the distro package manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkg: Option<String>,
    /// Shell command when installed by a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

impl ManifestPackage {
    /// Short description of how the package lands in the image.
    pub fn install_label(&self) -> String {
        match (&self.pkg, &self.script) {
            (Some(pkg), _) => format!("pkg: {pkg}"),
            (None, Some(_)) => "script".to_string(),
            (None, None) => "unknown".to_string(),
        }
    }
}

impl Manifest {
    /// Manifest describing `packages` as installed on `distro` into `image`.
    pub fn from_selection(distro: &Distro, packages: &[Package], image: String) -> Self {
        Self::from_selection_with(distro, packages, &BTreeMap::new(), image)
    }

    /// Same as [`Manifest::from_selection`], including the user's install overrides.
    pub fn from_selection_with(
        distro: &Distro,
        packages: &[Package],
        overrides: &BTreeMap<String, InstallOverride>,
        image: String,
    ) -> Self {
        Self {
            version: MANIFEST_VERSION,
            image,
            image_id: None,
            distro: ManifestDistro {
                name: distro.name.clone(),
                image: distro.image.clone(),
                family: distro.family.key().to_string(),
            },
            packages: packages
                .iter()
                .map(|package| {
                    let spec = resolve_install(package, distro, overrides);
                    let (pkg, script) = match spec {
                        Some(InstallSpec::PackageManager { pkg }) => (Some(pkg), None),
                        Some(InstallSpec::Script { script }) => (None, Some(script)),
                        None => (None, None),
                    };
                    ManifestPackage {
                        name: package.name.clone(),
                        version: package.version.clone(),
                        pkg,
                        script,
                    }
                })
                .collect(),
            created_at: now_unix(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Image tag toolshop would use for this distro/package combination.
    pub fn image_tag(distro: &Distro, packages: &[Package]) -> String {
        let hash = selection_hash(distro, packages);
        format!("{IMAGE_REPOSITORY}:{}-{hash}", slug(&distro.name))
    }

    /// Reference the engine reports after `build -t <tag>`.
    pub fn image_reference(distro: &Distro, packages: &[Package]) -> String {
        format!("localhost/{}", Self::image_tag(distro, packages))
    }

    /// Seconds since the manifest was written.
    pub fn age_seconds(&self) -> u64 {
        now_unix().saturating_sub(self.created_at)
    }

    /// Compare the recorded image ID with the ID the engine currently reports.
    pub fn id_matches(&self, current_id: &str) -> bool {
        match &self.image_id {
            Some(recorded) => {
                crate::engine::short_id(recorded) == crate::engine::short_id(current_id)
            }
            // Manifests written before the ID was recorded cannot be compared.
            None => true,
        }
    }
}

/// Directory manifests are stored in.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("toolshop").join("manifests");
        }
    }
    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home).join(".cache/toolshop/manifests"),
        None => PathBuf::from("toolshop-manifests"),
    }
}

/// File a manifest for `image` is stored in.
pub fn path_for(dir: &Path, image: &str) -> PathBuf {
    dir.join(format!("{}.toml", slug(normalize_image(image))))
}

/// Write `manifest` to the cache, returning the file it was written to.
pub fn save(manifest: &Manifest) -> Result<PathBuf> {
    save_to(&cache_dir(), manifest)
}

pub fn save_to(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).wrap_err_with(|| format!("cannot create {}", dir.display()))?;
    let path = path_for(dir, &manifest.image);
    let contents = toml::to_string_pretty(manifest).wrap_err("cannot serialise manifest")?;
    std::fs::write(&path, contents).wrap_err_with(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

pub fn forget_in(dir: &Path, image: &str) -> Result<()> {
    let path = path_for(dir, image);
    std::fs::remove_file(&path).wrap_err_with(|| format!("cannot remove {}", path.display()))?;
    Ok(())
}

/// Load every manifest in `dir`, newest first, plus warnings for broken files.
pub fn load_all() -> (Vec<Manifest>, Vec<String>) {
    load_all_from(&cache_dir())
}

pub fn load_all_from(dir: &Path) -> (Vec<Manifest>, Vec<String>) {
    let mut manifests = Vec::new();
    let mut warnings = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (manifests, warnings),
        Err(err) => {
            warnings.push(format!("cannot read {}: {err}", dir.display()));
            return (manifests, warnings);
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    for path in paths {
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) => {
                warnings.push(format!("cannot read {}: {err}", path.display()));
                continue;
            }
        };

        match toml::from_str::<Manifest>(&contents) {
            Ok(manifest) => {
                if manifest.version > MANIFEST_VERSION {
                    warnings.push(format!(
                        "{} was written by a newer toolshop (manifest v{})",
                        path.display(),
                        manifest.version
                    ));
                }
                manifests.push(manifest);
            }
            Err(err) => warnings.push(format!("skipping {}: {err}", path.display())),
        }
    }

    manifests.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.image.cmp(&b.image))
    });
    (manifests, warnings)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Stable hash of the build inputs, so the same selection always gets the same tag.
fn selection_hash(distro: &Distro, packages: &[Package]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64 offset basis
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };

    feed(distro.image.as_bytes());
    let mut names: Vec<&str> = packages
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    names.sort_unstable();
    for name in names {
        feed(name.as_bytes());
        if let Some(package) = packages.iter().find(|package| package.name == name)
            && let Some(spec) = package.install_for(distro)
        {
            match spec {
                InstallSpec::PackageManager { pkg } => feed(pkg.as_bytes()),
                InstallSpec::Script { script } => feed(script.as_bytes()),
            }
        }
    }

    format!("{:06x}", hash & 0xff_ffff)
}

/// Lowercase, filesystem- and tag-safe form of `value`.
fn slug(value: &str) -> String {
    let slug: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::DEFAULT_INSTALL_KEY,
        distro::{Distro, DistroFamily, DistroSource},
    };
    use std::collections::BTreeMap;

    fn distro(name: &str) -> Distro {
        Distro::new(
            DistroFamily::Ros,
            name,
            format!("docker.io/library/ros:{name}-ros-base"),
            DistroSource::Predefined,
        )
    }

    fn package(name: &str, install: InstallSpec) -> Package {
        Package {
            name: name.to_string(),
            version: String::new(),
            summary: String::new(),
            category: "Test".to_string(),
            requires: vec![],
            install: BTreeMap::from([(DEFAULT_INSTALL_KEY.to_string(), install)]),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "toolshop-manifests-{}-{tag}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn manifest_records_resolved_installs() {
        let packages = vec![
            package(
                "htop",
                InstallSpec::PackageManager {
                    pkg: "htop".to_string(),
                },
            ),
            package(
                "uv",
                InstallSpec::Script {
                    script: "pip3 install uv".to_string(),
                },
            ),
        ];
        let manifest = Manifest::from_selection(
            &distro("jazzy"),
            &packages,
            "localhost/toolshop:jazzy-1a2b3c".to_string(),
        );

        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.distro.family, "ros");
        assert_eq!(manifest.packages.len(), 2);
        assert_eq!(manifest.packages[0].pkg.as_deref(), Some("htop"));
        assert_eq!(manifest.packages[1].install_label(), "script");
        assert!(manifest.created_at > 0);
    }

    #[test]
    fn manifests_round_trip_through_the_cache() {
        let dir = temp_dir("roundtrip");
        let manifest = Manifest::from_selection(
            &distro("jazzy"),
            &[package(
                "htop",
                InstallSpec::PackageManager {
                    pkg: "htop".to_string(),
                },
            )],
            "localhost/toolshop:jazzy-1a2b3c".to_string(),
        );

        let path = save_to(&dir, &manifest).unwrap();
        assert_eq!(path.file_name().unwrap(), "toolshop-jazzy-1a2b3c.toml");

        let (loaded, warnings) = load_all_from(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded, vec![manifest.clone()]);

        forget_in(&dir, &manifest.image).unwrap();
        assert!(load_all_from(&dir).0.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_manifests_are_reported_and_skipped() {
        let dir = temp_dir("broken");
        std::fs::write(dir.join("bad.toml"), "version = 1\nimage = 3\n").unwrap();

        let (manifests, warnings) = load_all_from(&dir);
        assert!(manifests.is_empty());
        assert!(warnings.iter().any(|warning| warning.contains("bad.toml")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tags_are_stable_and_depend_on_the_selection() {
        let htop = package(
            "htop",
            InstallSpec::PackageManager {
                pkg: "htop".to_string(),
            },
        );
        let tmux = package(
            "tmux",
            InstallSpec::PackageManager {
                pkg: "tmux".to_string(),
            },
        );

        let a = Manifest::image_tag(&distro("jazzy"), &[htop.clone(), tmux.clone()]);
        let b = Manifest::image_tag(&distro("jazzy"), &[tmux.clone(), htop.clone()]);
        assert_eq!(a, b, "package order must not change the tag");
        assert!(a.starts_with("toolshop:jazzy-"), "{a}");

        let c = Manifest::image_tag(&distro("jazzy"), std::slice::from_ref(&htop));
        assert_ne!(a, c, "different selections need different tags");

        let d = Manifest::image_tag(&distro("kilted"), &[htop, tmux]);
        assert_ne!(a, d, "distro must be part of the tag");
    }

    #[test]
    fn image_ids_are_compared_in_short_form() {
        let mut manifest = Manifest::from_selection(&distro("jazzy"), &[], "x".to_string());
        manifest.image_id = Some("sha256:8f2c1d4a91b3ffff".to_string());
        assert!(manifest.id_matches("8f2c1d4a91b3"));
        assert!(!manifest.id_matches("000000000000"));
    }

    #[test]
    fn manifests_without_an_id_are_not_flagged_as_modified() {
        let manifest = Manifest::from_selection(&distro("jazzy"), &[], "x".to_string());
        assert!(manifest.id_matches("000000000000"));
    }

    #[test]
    fn ages_are_relative_to_now() {
        let mut manifest = Manifest::from_selection(&distro("jazzy"), &[], "x".to_string());
        assert!(manifest.age_seconds() < 5);
        manifest.created_at = now_unix().saturating_sub(120);
        assert!((115..=125).contains(&manifest.age_seconds()));
    }
}
