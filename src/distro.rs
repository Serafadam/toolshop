//! ROS distros and base images, grouped into families.

use std::path::{Path, PathBuf};

use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};

use crate::engine;

/// Distros that are always offered, even on a machine with no local images.
const PREDEFINED: [(DistroFamily, &str, &str); 6] = [
    (
        DistroFamily::Ros,
        "jazzy",
        "docker.io/library/ros:jazzy-ros-base",
    ),
    (
        DistroFamily::Ros,
        "kilted",
        "docker.io/library/ros:kilted-ros-base",
    ),
    (
        DistroFamily::Ros,
        "lyrical",
        "docker.io/library/ros:lyrical-ros-base",
    ),
    (
        DistroFamily::Ubuntu,
        "24.04",
        "docker.io/library/ubuntu:24.04",
    ),
    (
        DistroFamily::Ubuntu,
        "22.04",
        "docker.io/library/ubuntu:22.04",
    ),
    (DistroFamily::Fedora, "41", "docker.io/library/fedora:41"),
];

/// ROS distribution codenames used to name an image.
const KNOWN_ROS_NAMES: [&str; 7] = [
    "jazzy", "kilted", "lyrical", "humble", "iron", "rolling", "noetic",
];

/// How a distro installs packages. Distinguishes the generated `RUN` layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// Debian-style `apt-get`.
    Apt,
    /// Fedora-style `dnf`.
    Dnf,
}

impl PackageManager {
    pub fn command(self) -> &'static str {
        match self {
            PackageManager::Apt => "apt-get",
            PackageManager::Dnf => "dnf",
        }
    }
}

/// Directory user-added images are stored in.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TOOLSHOP_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join("toolshop");
        }
    }
    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home).join(".config").join("toolshop"),
        None => PathBuf::from(".toolshop"),
    }
}

/// File user-added images are persisted in.
pub fn images_file() -> PathBuf {
    config_dir().join("images.toml")
}

/// The family a distro belongs to. Families are the coarse grouping shown in the UI
/// and the middle step of the package-install lookup cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DistroFamily {
    /// ROS images (Ubuntu-based, but with their own package specs).
    Ros,
    Ubuntu,
    Debian,
    Fedora,
    /// Anything else: base images we cannot classify, and user-added images.
    Other,
}

impl DistroFamily {
    /// Families in display order.
    pub const ALL: [DistroFamily; 5] = [
        DistroFamily::Ros,
        DistroFamily::Ubuntu,
        DistroFamily::Debian,
        DistroFamily::Fedora,
        DistroFamily::Other,
    ];

    /// Heading shown above the family's distros.
    pub fn label(self) -> &'static str {
        match self {
            DistroFamily::Ros => "ROS",
            DistroFamily::Ubuntu => "Ubuntu",
            DistroFamily::Debian => "Debian",
            DistroFamily::Fedora => "Fedora",
            DistroFamily::Other => "Other",
        }
    }

    /// Key used for `[install.<key>]` tables in the package catalog.
    pub fn key(self) -> &'static str {
        match self {
            DistroFamily::Ros => "ros",
            DistroFamily::Ubuntu => "ubuntu",
            DistroFamily::Debian => "debian",
            DistroFamily::Fedora => "fedora",
            DistroFamily::Other => "other",
        }
    }

    /// Parse a family key (`ros`, `ubuntu`, `debian`, `fedora`, `other`).
    pub fn from_key(key: &str) -> Option<Self> {
        DistroFamily::ALL
            .into_iter()
            .find(|family| family.key().eq_ignore_ascii_case(key))
    }

    /// Package manager for the family. `Other` assumes Debian-style tooling, which is
    /// what most images use; per-package install overrides cover the rest.
    pub fn package_manager(self) -> PackageManager {
        match self {
            DistroFamily::Ros
            | DistroFamily::Ubuntu
            | DistroFamily::Debian
            | DistroFamily::Other => PackageManager::Apt,
            DistroFamily::Fedora => PackageManager::Dnf,
        }
    }

    /// Families an image reference looks like, most specific first.
    fn detect(repository: &str, tag: &str) -> Option<DistroFamily> {
        // ROS images are Ubuntu based, so they have to be recognised first.
        if has_token(repository, "ros")
            || has_token(tag, "ros")
            || KNOWN_ROS_NAMES.iter().any(|name| tag.contains(name))
        {
            return Some(DistroFamily::Ros);
        }
        if has_token(repository, "ubuntu") {
            return Some(DistroFamily::Ubuntu);
        }
        if has_token(repository, "debian") {
            return Some(DistroFamily::Debian);
        }
        if has_token(repository, "fedora") {
            return Some(DistroFamily::Fedora);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistroSource {
    /// Shipped with toolshop.
    Predefined,
    /// Found in the local container engine.
    Local,
    /// Added by the user and persisted in the config directory.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    /// Name used for the per-distro package specs (e.g. `jazzy`, `24.04`).
    pub name: String,
    /// Image reference used in the generated `FROM` line.
    pub image: String,
    pub family: DistroFamily,
    pub source: DistroSource,
}

impl Distro {
    pub fn new(
        family: DistroFamily,
        name: impl Into<String>,
        image: impl Into<String>,
        source: DistroSource,
    ) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            family,
            source,
        }
    }

    pub fn predefined() -> Vec<Self> {
        PREDEFINED
            .iter()
            .map(|(family, name, image)| {
                Distro::new(*family, *name, *image, DistroSource::Predefined)
            })
            .collect()
    }

    /// Derive a distro from an image reference such as `localhost/ros:jazzy-dev`.
    ///
    /// Every usable reference becomes a distro: images whose family cannot be detected
    /// end up in [`DistroFamily::Other`] rather than being dropped.
    pub fn from_image(image: &str) -> Option<Self> {
        let image = image.trim();
        if image.is_empty() || image.contains("<none>") {
            return None;
        }

        let lowercase = image.to_ascii_lowercase();
        let (repository, tag) = split_reference(&lowercase);
        let family = DistroFamily::detect(repository, tag).unwrap_or(DistroFamily::Other);
        let name = derive_name(family, repository, tag)?;

        Some(Distro {
            name,
            image: image.to_string(),
            family,
            source: DistroSource::Local,
        })
    }

    /// A distro the user added by hand, with an explicitly chosen family.
    pub fn custom(image: &str, family: DistroFamily) -> Option<Self> {
        let image = image.trim();
        if image.is_empty() || image.contains("<none>") {
            return None;
        }

        let lowercase = image.to_ascii_lowercase();
        let (repository, tag) = split_reference(&lowercase);
        let name = derive_name(family, repository, tag)?;

        Some(Distro {
            name,
            image: image.to_string(),
            family,
            source: DistroSource::Custom,
        })
    }

    /// True when the user added this distro themselves.
    pub fn is_custom(&self) -> bool {
        self.source == DistroSource::Custom
    }

    /// Short label for the distro list.
    pub fn label(&self) -> String {
        match self.source {
            DistroSource::Predefined => self.name.clone(),
            DistroSource::Local => format!("{} (local)", short_image(&self.image)),
            DistroSource::Custom => format!("{} (custom)", short_image(&self.image)),
        }
    }

    /// Image reference with the default registry prefix removed, for comparisons.
    pub fn normalized_image(&self) -> &str {
        short_image(&self.image)
    }
}

/// Predefined distros followed by local images not already covered by them.
pub async fn available() -> (Vec<Distro>, Vec<String>) {
    let (custom, mut warnings) = load_custom();
    let mut distros = Distro::predefined();
    distros.extend(custom);

    match engine::images().await {
        Ok(images) => {
            for image in images {
                let Some(distro) = Distro::from_image(&image.reference) else {
                    continue;
                };
                if distros
                    .iter()
                    .any(|known| known.normalized_image() == distro.normalized_image())
                {
                    continue;
                }
                distros.push(distro);
            }
        }
        Err(err) => warnings.push(format!("local images unavailable: {err}")),
    }

    (distros, warnings)
}

/// Images the user added by hand, read from the config directory.
pub fn load_custom() -> (Vec<Distro>, Vec<String>) {
    load_custom_from(&images_file())
}

pub fn load_custom_from(path: &Path) -> (Vec<Distro>, Vec<String>) {
    let mut warnings = Vec::new();
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (vec![], warnings),
        Err(err) => {
            warnings.push(format!("cannot read {}: {err}", path.display()));
            return (vec![], warnings);
        }
    };

    match toml::from_str::<ImagesFile>(&contents) {
        Ok(file) => {
            let distros = file
                .images
                .into_iter()
                .filter_map(|entry| {
                    let family = DistroFamily::from_key(&entry.family);
                    if family.is_none() {
                        warnings.push(format!(
                            "{}: unknown image type {:?}",
                            path.display(),
                            entry.family
                        ));
                    }
                    let family = family?;
                    let image = entry.image.trim();
                    if image.contains("<none>") || image.is_empty() {
                        return None;
                    }
                    let name = entry
                        .name
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty());

                    let mut distro = Distro::custom(image, family)?;
                    distro.source = DistroSource::Custom;
                    if let Some(name) = name {
                        distro.name = name;
                    }
                    Some(distro)
                })
                .collect();
            (distros, warnings)
        }
        Err(err) => {
            warnings.push(format!("cannot parse {}: {err}", path.display()));
            (vec![], warnings)
        }
    }
}

/// Persist the user-added images currently known to the app.
pub fn save_custom(distros: &[Distro]) -> Result<()> {
    save_custom_to(&images_file(), distros)
}

pub fn save_custom_to(path: &Path, distros: &[Distro]) -> Result<()> {
    let file = ImagesFile {
        images: distros
            .iter()
            .filter(|distro| distro.is_custom())
            .map(|distro| ImageEntry {
                image: distro.image.clone(),
                family: distro.family.key().to_string(),
                name: Some(distro.name.clone()),
            })
            .collect(),
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("cannot create {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(&file).wrap_err("cannot serialise images")?;
    std::fs::write(path, contents).wrap_err_with(|| format!("cannot write {}", path.display()))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ImagesFile {
    #[serde(default, rename = "image")]
    images: Vec<ImageEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ImageEntry {
    /// Image reference, e.g. `localhost/test-dev:ubuntu24`.
    image: String,
    /// Distro family key (`ros`, `ubuntu`, `debian`, `fedora`, `other`).
    #[serde(default = "default_family_key")]
    family: String,
    /// Optional distro name used for package-spec lookups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

fn default_family_key() -> String {
    DistroFamily::Other.key().to_string()
}

/// Name of the distro behind an image: the ROS codename or the image version tag.
fn derive_name(family: DistroFamily, repository: &str, tag: &str) -> Option<String> {
    if family == DistroFamily::Ros
        && let Some(name) = KNOWN_ROS_NAMES
            .iter()
            .find(|name| repository.contains(**name) || tag.contains(**name))
    {
        return Some((*name).to_string());
    }

    // A tag that merely repeats the family ("test-dev:ubuntu24") is less useful as a
    // name than the repository itself.
    let tag_names_family = !tag.is_empty() && tag.contains(family.key());
    let candidate = if tag.is_empty() || tag == "latest" || tag_names_family {
        repository.rsplit('/').next().unwrap_or(repository)
    } else {
        tag
    };
    let sanitized = sanitize(candidate);
    (!sanitized.is_empty()).then_some(sanitized)
}

/// True when `value` contains `token` delimited by non-alphanumeric characters, so
/// `my-ros` and `ros:jazzy` match while `cross` does not.
fn has_token(value: &str, token: &str) -> bool {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|candidate| candidate == token)
}

/// Split `repo/name:tag` into its parts, ignoring a registry port.
fn split_reference(reference: &str) -> (&str, &str) {
    match reference.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => (repository, tag),
        _ => (reference, ""),
    }
}

fn short_image(image: &str) -> &str {
    engine::normalize_image(image)
}

fn sanitize(candidate: &str) -> String {
    candidate
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['-', '.'])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predefined_distros_cover_every_family() {
        let distros = Distro::predefined();
        assert_eq!(distros.len(), 6);

        let families: Vec<DistroFamily> = distros.iter().map(|distro| distro.family).collect();
        assert!(families.contains(&DistroFamily::Ros));
        assert!(families.contains(&DistroFamily::Ubuntu));
        assert!(families.contains(&DistroFamily::Fedora));

        let jazzy = distros
            .iter()
            .find(|distro| distro.name == "jazzy")
            .unwrap();
        assert_eq!(jazzy.family, DistroFamily::Ros);
        assert_eq!(jazzy.image, "docker.io/library/ros:jazzy-ros-base");
        assert_eq!(jazzy.label(), "jazzy");
    }

    #[test]
    fn images_are_classified_by_family() {
        let cases = [
            (
                "docker.io/library/ros:jazzy-ros-base",
                DistroFamily::Ros,
                "jazzy",
            ),
            ("ros:kilted-ros-core", DistroFamily::Ros, "kilted"),
            ("localhost/ros:jazzy-dev", DistroFamily::Ros, "jazzy"),
            ("localhost/ros-lyrical:latest", DistroFamily::Ros, "lyrical"),
            (
                "docker.io/library/ubuntu:24.04",
                DistroFamily::Ubuntu,
                "24.04",
            ),
            ("ubuntu:22.04", DistroFamily::Ubuntu, "22.04"),
            ("docker.io/library/debian:12", DistroFamily::Debian, "12"),
            ("docker.io/library/fedora:41", DistroFamily::Fedora, "41"),
        ];
        for (image, family, name) in cases {
            let distro = Distro::from_image(image).expect(image);
            assert_eq!(distro.family, family, "{image}");
            assert_eq!(distro.name, name, "{image}");
            assert_eq!(distro.source, DistroSource::Local);
        }
    }

    #[test]
    fn ros_token_must_be_delimited() {
        // `cross` contains the letters "ros" but is not classified as ROS.
        let distro = Distro::from_image("crossbuild/base:latest").unwrap();
        assert_eq!(distro.family, DistroFamily::Other);
        assert_eq!(
            Distro::from_image("localhost/my-ros:dev").unwrap().family,
            DistroFamily::Ros
        );
    }

    #[test]
    fn unrecognised_images_are_listed_as_other() {
        // Everything usable is offered, so no local image is hidden.
        let distro = Distro::from_image("alpine:3.20").unwrap();
        assert_eq!(distro.family, DistroFamily::Other);
        assert_eq!(distro.name, "3.20");
        assert_eq!(distro.family.package_manager(), PackageManager::Apt);

        assert_eq!(
            Distro::from_image("ghcr.io/foo/bar:latest").unwrap().name,
            "bar"
        );

        // Dangling images have no usable reference.
        assert!(Distro::from_image("").is_none());
        assert!(Distro::from_image("<none>:<none>").is_none());
    }

    #[test]
    fn user_images_keep_the_family_they_were_given() {
        let distro = Distro::custom("localhost/test-dev:ubuntu24", DistroFamily::Fedora).unwrap();
        assert!(distro.is_custom());
        assert_eq!(distro.source, DistroSource::Custom);
        assert_eq!(distro.family, DistroFamily::Fedora);
        assert_eq!(
            distro.family.package_manager(),
            PackageManager::Dnf,
            "the chosen family decides the package manager"
        );
        assert!(distro.label().ends_with("(custom)"));
        assert!(Distro::custom("<none>:<none>", DistroFamily::Ros).is_none());
    }

    #[test]
    fn custom_images_round_trip_through_the_config() {
        let dir = std::env::temp_dir().join(format!("toolshop-images-{}", std::process::id()));
        let path = dir.join("images.toml");
        let distros = vec![
            Distro::custom("localhost/test-dev:ubuntu24", DistroFamily::Ubuntu).unwrap(),
            Distro::predefined().remove(0),
        ];

        save_custom_to(&path, &distros).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("test-dev"));
        assert!(
            !contents.contains("jazzy-ros-base"),
            "only custom images are stored"
        );

        let (loaded, warnings) = load_custom_from(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].image, "localhost/test-dev:ubuntu24");
        assert_eq!(loaded[0].family, DistroFamily::Ubuntu);
        assert_eq!(loaded[0].name, "test-dev");
        assert!(loaded[0].is_custom());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_image_configs_are_reported() {
        let dir = std::env::temp_dir().join(format!("toolshop-images-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("images.toml");
        std::fs::write(&path, "[[image]]\nimage = 3\n").unwrap();

        let (loaded, warnings) = load_custom_from(&path);
        assert!(loaded.is_empty());
        assert!(warnings.iter().any(|w| w.contains("cannot parse")));

        std::fs::write(&path, "[[image]]\nimage = \"x:1\"\nfamily = \"gentoo\"\n").unwrap();
        let (loaded, warnings) = load_custom_from(&path);
        assert!(loaded.is_empty());
        assert!(warnings.iter().any(|w| w.contains("unknown image type")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn family_keys_round_trip() {
        for family in DistroFamily::ALL {
            assert_eq!(DistroFamily::from_key(family.key()), Some(family));
        }
        assert_eq!(DistroFamily::from_key("nope"), None);
    }

    #[test]
    fn local_names_fall_back_to_the_image() {
        let distro = Distro::from_image("localhost/my-ros:dev-2024").unwrap();
        assert_eq!(distro.name, "dev-2024");

        let distro = Distro::from_image("localhost/ros-sandbox:latest").unwrap();
        assert_eq!(distro.name, "ros-sandbox");
    }

    #[test]
    fn local_labels_show_the_image() {
        let distro = Distro::from_image("localhost/ros:jazzy-dev").unwrap();
        assert_eq!(distro.label(), "ros:jazzy-dev (local)");
    }

    #[test]
    fn registry_ports_are_not_mistaken_for_tags() {
        assert_eq!(
            split_reference("localhost:5000/ros:jazzy"),
            ("localhost:5000/ros", "jazzy")
        );
        assert_eq!(
            split_reference("localhost:5000/ros"),
            ("localhost:5000/ros", "")
        );
    }

    #[test]
    fn families_pick_the_right_package_manager() {
        assert_eq!(DistroFamily::Ros.package_manager(), PackageManager::Apt);
        assert_eq!(DistroFamily::Ubuntu.package_manager(), PackageManager::Apt);
        assert_eq!(DistroFamily::Debian.package_manager(), PackageManager::Apt);
        assert_eq!(DistroFamily::Fedora.package_manager(), PackageManager::Dnf);
        assert_eq!(PackageManager::Dnf.command(), "dnf");
    }
}
