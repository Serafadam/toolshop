//! The toolboxes tab: toolshop-built images and whether they still exist locally.

use color_eyre::Result;
use tokio::process::Command;

use crate::{
    engine::{self, normalize_image, short_id},
    manifest::{self, Manifest},
};

/// How long a `toolbox list` query may take.
const TOOLBOX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A toolbox container created from a local image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolboxContainer {
    pub name: String,
    /// Image the container was created from.
    pub image: String,
}

/// Where a manifested image stands right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolboxStatus {
    /// The engine still has the image with the ID recorded at build time.
    Present,
    /// The tag exists, but its ID differs from the build manifest.
    Modified { current_id: String },
    /// The engine no longer has an image for this tag.
    Missing,
    /// The engine could not be queried, so nothing can be said.
    Unknown,
}

impl ToolboxStatus {
    /// The status an entry keeps once its manifest is gone: only existence is known.
    pub fn without_manifest(&self) -> ToolboxStatus {
        match self {
            ToolboxStatus::Present | ToolboxStatus::Modified { .. } => ToolboxStatus::Present,
            other => other.clone(),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ToolboxStatus::Present => "present",
            ToolboxStatus::Modified { .. } => "modified",
            ToolboxStatus::Missing => "missing",
            ToolboxStatus::Unknown => "unknown",
        }
    }
}

/// An image in the tab: either built by toolshop (and therefore described by a
/// manifest) or something else on the machine that has toolbox containers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolbox {
    /// Image reference, taken from the manifest or from the containers.
    pub image: String,
    /// Build manifest, set only when toolshop created this image.
    pub manifest: Option<Manifest>,
    pub status: ToolboxStatus,
    /// Names of toolbox containers created from this image.
    pub containers: Vec<String>,
}

impl Toolbox {
    /// An image toolshop built, described by `manifest`.
    pub fn built(manifest: Manifest, status: ToolboxStatus, containers: Vec<String>) -> Self {
        Self {
            image: manifest.image.clone(),
            manifest: Some(manifest),
            status,
            containers,
        }
    }

    /// A toolbox container image this tool did not create.
    pub fn external(
        image: impl Into<String>,
        status: ToolboxStatus,
        containers: Vec<String>,
    ) -> Self {
        Self {
            image: image.into(),
            manifest: None,
            status,
            containers,
        }
    }

    /// True when toolshop built this image and can describe what is inside it.
    pub fn is_toolshop_build(&self) -> bool {
        self.manifest.is_some()
    }

    /// Build time, or 0 for images toolshop did not create.
    pub fn created_at(&self) -> u64 {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.created_at)
            .unwrap_or_default()
    }

    /// Packages recorded at build time (empty for images toolshop did not create).
    pub fn package_count(&self) -> usize {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.packages.len())
            .unwrap_or_default()
    }

    /// One-line summary for the list pane.
    pub fn label(&self) -> String {
        let containers = match self.containers.len() {
            0 => String::new(),
            1 => " +1 toolbox".to_string(),
            count => format!(" +{count} toolboxes"),
        };
        let age = match &self.manifest {
            Some(manifest) => format!(" {}", humanize_age(manifest.age_seconds())),
            None => " external".to_string(),
        };

        format!(
            "{} [{}]{age}{containers}",
            normalize_image(&self.image),
            self.status.label(),
        )
    }
}

/// Result of a background toolbox scan.
#[derive(Debug)]
pub struct ToolboxScan {
    pub toolboxes: Vec<Toolbox>,
    pub warnings: Vec<String>,
}

/// Load manifests and match them against the images the engine currently has.
pub async fn scan() -> ToolboxScan {
    let (manifests, mut warnings) = manifest::load_all();

    let images = match engine::images().await {
        Ok(images) => Some(images),
        Err(err) => {
            warnings.push(format!("cannot list images: {err}"));
            None
        }
    };

    // `toolbox` is optional: without it images are still listed, just without containers.
    let containers = containers().await.unwrap_or_default();

    let toolboxes = build_toolboxes(manifests, images.as_deref(), &containers);

    ToolboxScan {
        toolboxes,
        warnings,
    }
}

/// Combine manifests, images and toolbox containers into the rows of the tab.
///
/// Toolshop builds come first (newest first). Every other image that has toolbox
/// containers is listed afterwards, so toolboxes this tool did not create are visible
/// too — just without a package list.
pub fn build_toolboxes(
    manifests: Vec<Manifest>,
    images: Option<&[engine::ImageRef]>,
    containers: &[ToolboxContainer],
) -> Vec<Toolbox> {
    let mut toolboxes: Vec<Toolbox> = manifests
        .into_iter()
        .map(|manifest| {
            let status = match_status(&manifest, images);
            let owned = containers_for(&manifest.image, containers);
            Toolbox::built(manifest, status, owned)
        })
        .collect();

    // Images with containers but no manifest were not created by this tool.
    let mut external: Vec<Toolbox> = Vec::new();
    for container in containers {
        let known = toolboxes
            .iter()
            .chain(external.iter())
            .any(|toolbox| normalize_image(&toolbox.image) == normalize_image(&container.image));
        if known {
            if let Some(toolbox) = external.iter_mut().find(|toolbox| {
                normalize_image(&toolbox.image) == normalize_image(&container.image)
            }) {
                toolbox.containers.push(container.name.clone());
            }
            continue;
        }

        external.push(Toolbox::external(
            container.image.clone(),
            image_status(&container.image, images),
            vec![container.name.clone()],
        ));
    }

    toolboxes.extend(external);
    sort_toolboxes(&mut toolboxes);
    toolboxes
}

/// Toolshop builds first (newest first), then external entries by image name.
pub fn sort_toolboxes(toolboxes: &mut [Toolbox]) {
    toolboxes.sort_by(|a, b| {
        b.is_toolshop_build()
            .cmp(&a.is_toolshop_build())
            .then_with(|| b.created_at().cmp(&a.created_at()))
            .then_with(|| a.image.cmp(&b.image))
    });
}

/// Status of an image that has no manifest: only existence can be reported.
fn image_status(image: &str, images: Option<&[engine::ImageRef]>) -> ToolboxStatus {
    let Some(images) = images else {
        return ToolboxStatus::Unknown;
    };
    let wanted = normalize_image(image);
    if images
        .iter()
        .any(|candidate| normalize_image(&candidate.reference) == wanted)
    {
        ToolboxStatus::Present
    } else {
        ToolboxStatus::Missing
    }
}

/// Create a toolbox container from `image` (`toolbox create --image …`).
///
/// `name` becomes the container name (`--container`); when it is `None` toolbox derives
/// one from the image. Returns the command output so the UI can show what happened.
pub async fn create_container(image: &str, name: Option<&str>) -> Result<String> {
    let mut command = Command::new("toolbox");
    command.args(["create", "--image", image]);
    if let Some(name) = name {
        command.args(["--container", name]);
    }
    command.kill_on_drop(true);

    let output = tokio::time::timeout(TOOLBOX_TIMEOUT * 12, command.output())
        .await?
        .map_err(|err| color_eyre::eyre::eyre!("cannot run `toolbox`: {err}"))?;

    let done = match name {
        Some(name) => format!("created toolbox {name} from {image}"),
        None => format!("created toolbox from {image}"),
    };
    combine_output(output, done)
}

/// Default name offered for a new toolbox: the image reference as a container name.
pub fn default_container_name(image: &str) -> String {
    let short = normalize_image(image);
    short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// True when `name` can be used as a container name.
pub fn is_valid_container_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Remove a toolbox container by name (`toolbox rm --force`).
pub async fn remove_container(name: &str) -> Result<String> {
    let output = tokio::time::timeout(
        TOOLBOX_TIMEOUT * 12,
        Command::new("toolbox")
            .args(["rm", "--force", name])
            .kill_on_drop(true)
            .output(),
    )
    .await?
    .map_err(|err| color_eyre::eyre::eyre!("cannot run `toolbox`: {err}"))?;

    combine_output(output, format!("removed toolbox {name}"))
}

/// Turn a finished command into either a message or an error with its output.
fn combine_output(output: std::process::Output, success: String) -> Result<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if output.status.success() {
        return Ok(match detail.last() {
            Some(line) => format!("{success}: {line}"),
            None => success,
        });
    }

    // Failures often explain themselves over several lines (e.g. `useradd: UID 1000 is
    // not unique`), so keep all of it instead of only the last line.
    Err(color_eyre::eyre::eyre!(
        "toolbox exited with {}: {}",
        output.status,
        detail.join(" | ")
    ))
}

/// Toolbox containers present on the machine (`toolbox list --containers`).
///
/// Failing to run `toolbox` (not installed, or no engine backend) is not an error for
/// the UI, so callers treat `Err` as "no containers".
pub async fn containers() -> Result<Vec<ToolboxContainer>> {
    let output = tokio::time::timeout(
        TOOLBOX_TIMEOUT,
        Command::new("toolbox")
            .args(["list", "--containers"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;

    if !output.status.success() {
        return Err(color_eyre::eyre::eyre!(
            "toolbox list exited with {}",
            output.status
        ));
    }

    Ok(parse_containers(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse the fixed-width table printed by `toolbox list --containers`.
///
/// Columns are `CONTAINER ID  CONTAINER NAME  CREATED  STATUS  IMAGE NAME`; the image
/// name is last and may contain no spaces, so the name and image are enough for us.
fn parse_containers(output: &str) -> Vec<ToolboxContainer> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 3 || fields[1] == "NAME" || fields[0] == "CONTAINER" {
                return None;
            }
            Some(ToolboxContainer {
                name: fields[1].to_string(),
                image: fields[fields.len() - 1].to_string(),
            })
        })
        .collect()
}

/// Containers created from `image`, matched with the usual registry-prefix tolerance.
fn containers_for(image: &str, containers: &[ToolboxContainer]) -> Vec<String> {
    let wanted = normalize_image(image);
    containers
        .iter()
        .filter(|container| normalize_image(&container.image) == wanted)
        .map(|container| container.name.clone())
        .collect()
}

/// Compare a manifest with the images the engine currently has.
///
/// `images` is `None` when the engine could not be queried at all.
pub fn match_status(manifest: &Manifest, images: Option<&[engine::ImageRef]>) -> ToolboxStatus {
    let Some(images) = images else {
        return ToolboxStatus::Unknown;
    };
    let wanted = normalize_image(&manifest.image);

    match images
        .iter()
        .find(|image| normalize_image(&image.reference) == wanted)
    {
        None => ToolboxStatus::Missing,
        Some(image) if manifest.id_matches(&image.id) => ToolboxStatus::Present,
        Some(image) => ToolboxStatus::Modified {
            current_id: short_id(&image.id).to_string(),
        },
    }
}

/// Coarse, dependency-free age description for the list pane.
pub fn humanize_age(seconds: u64) -> String {
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestDistro, ManifestPackage};

    fn manifest(image: &str, image_id: Option<&str>) -> Manifest {
        Manifest {
            version: manifest::MANIFEST_VERSION,
            image: image.to_string(),
            image_id: image_id.map(str::to_string),
            distro: ManifestDistro {
                name: "jazzy".to_string(),
                image: "docker.io/library/ros:jazzy-ros-base".to_string(),
                family: "ros".to_string(),
            },
            packages: vec![ManifestPackage {
                name: "htop".to_string(),
                version: String::new(),
                pkg: Some("htop".to_string()),
                script: None,
            }],
            created_at: 0,
            tool_version: "test".to_string(),
        }
    }

    #[test]
    fn ages_are_humanized() {
        assert_eq!(humanize_age(0), "just now");
        assert_eq!(humanize_age(59), "just now");
        assert_eq!(humanize_age(60), "1m ago");
        assert_eq!(humanize_age(7200), "2h ago");
        assert_eq!(humanize_age(200_000), "2d ago");
    }

    fn image(reference: &str, id: &str) -> engine::ImageRef {
        engine::ImageRef {
            reference: reference.to_string(),
            id: id.to_string(),
        }
    }

    #[test]
    fn status_compares_manifest_ids_with_the_engine() {
        let images = vec![
            image("localhost/toolshop:jazzy-1a2b3c", "8f2c1d4a91b3"),
            image("localhost/toolshop:jazzy-9f9f9f", "000000000000"),
        ];

        let present = manifest(
            "localhost/toolshop:jazzy-1a2b3c",
            Some("sha256:8f2c1d4a91b3ffff"),
        );
        assert_eq!(
            match_status(&present, Some(&images)),
            ToolboxStatus::Present
        );

        let modified = manifest(
            "localhost/toolshop:jazzy-9f9f9f",
            Some("sha256:aaaa11112222"),
        );
        assert_eq!(
            match_status(&modified, Some(&images)),
            ToolboxStatus::Modified {
                current_id: "000000000000".to_string()
            }
        );

        let missing = manifest(
            "localhost/toolshop:jazzy-ffffff",
            Some("sha256:aaaa11112222"),
        );
        assert_eq!(
            match_status(&missing, Some(&images)),
            ToolboxStatus::Missing
        );

        assert_eq!(match_status(&present, None), ToolboxStatus::Unknown);
    }

    #[test]
    fn registry_prefixes_do_not_break_matching() {
        let images = vec![image("toolshop:jazzy-1a2b3c", "8f2c1d4a91b3")];
        let manifest = manifest(
            "localhost/toolshop:jazzy-1a2b3c",
            Some("sha256:8f2c1d4a91b3ffff"),
        );
        assert_eq!(
            match_status(&manifest, Some(&images)),
            ToolboxStatus::Present
        );
    }

    #[test]
    fn labels_show_image_status_and_age() {
        let toolbox = Toolbox::built(
            manifest(
                "localhost/toolshop:jazzy-1a2b3c",
                Some("sha256:aaaa11112222"),
            ),
            ToolboxStatus::Present,
            vec![],
        );
        assert!(
            toolbox
                .label()
                .starts_with("toolshop:jazzy-1a2b3c [present]")
        );
        assert_eq!(toolbox.package_count(), 1);

        let with_boxes = Toolbox {
            containers: vec!["dev".to_string(), "scratch".to_string()],
            ..toolbox
        };
        assert!(
            with_boxes.label().ends_with("+2 toolboxes"),
            "{}",
            with_boxes.label()
        );
    }

    #[test]
    fn containers_without_a_manifest_are_listed_as_external() {
        let images = vec![
            image("localhost/toolshop:jazzy-1a2b3c", "8f2c1d4a91b3"),
            image("localhost/test-dev:ubuntu24", "aaaa11112222"),
        ];
        let containers = vec![
            ToolboxContainer {
                name: "dev".to_string(),
                image: "localhost/toolshop:jazzy-1a2b3c".to_string(),
            },
            ToolboxContainer {
                name: "test".to_string(),
                image: "localhost/test-dev:ubuntu24".to_string(),
            },
            ToolboxContainer {
                name: "test-scratch".to_string(),
                image: "localhost/test-dev:ubuntu24".to_string(),
            },
            ToolboxContainer {
                name: "old".to_string(),
                image: "localhost/legacy:1".to_string(),
            },
        ];

        let toolboxes = build_toolboxes(
            vec![manifest(
                "localhost/toolshop:jazzy-1a2b3c",
                Some("sha256:8f2c1d4a91b3ffff"),
            )],
            Some(&images),
            &containers,
        );

        assert_eq!(toolboxes.len(), 3);
        // Toolshop builds keep their manifest and only their own containers.
        assert!(toolboxes[0].is_toolshop_build());
        assert_eq!(toolboxes[0].containers, vec!["dev".to_string()]);

        // Images this tool did not create are still listed, grouped by image.
        let test = toolboxes
            .iter()
            .find(|toolbox| toolbox.image.contains("test-dev"))
            .expect("external entry");
        assert!(!test.is_toolshop_build());
        assert_eq!(test.status, ToolboxStatus::Present);
        assert_eq!(test.containers.len(), 2);
        assert_eq!(test.package_count(), 0);
        assert!(test.label().contains("external"));

        // A container whose image is gone is still shown, marked missing.
        let legacy = toolboxes
            .iter()
            .find(|toolbox| toolbox.image.contains("legacy"))
            .expect("missing entry");
        assert_eq!(legacy.status, ToolboxStatus::Missing);

        // Builds sort before external entries.
        assert_eq!(
            toolboxes.iter().filter(|t| t.is_toolshop_build()).count(),
            1
        );
        assert!(toolboxes[0].is_toolshop_build());
    }

    #[test]
    fn external_entries_are_unknown_without_an_engine() {
        let containers = vec![ToolboxContainer {
            name: "test".to_string(),
            image: "localhost/test-dev:ubuntu24".to_string(),
        }];
        let toolboxes = build_toolboxes(vec![], None, &containers);
        assert_eq!(toolboxes.len(), 1);
        assert_eq!(toolboxes[0].status, ToolboxStatus::Unknown);
        assert!(!toolboxes[0].is_toolshop_build());
    }

    #[test]
    fn status_without_a_manifest_keeps_only_existence() {
        assert_eq!(
            ToolboxStatus::Modified {
                current_id: "abc".to_string()
            }
            .without_manifest(),
            ToolboxStatus::Present
        );
        assert_eq!(
            ToolboxStatus::Missing.without_manifest(),
            ToolboxStatus::Missing
        );
        assert_eq!(
            ToolboxStatus::Unknown.without_manifest(),
            ToolboxStatus::Unknown
        );
    }

    #[test]
    fn toolbox_containers_are_parsed_from_the_table() {
        let output = "\
CONTAINER ID  CONTAINER NAME  CREATED       STATUS   IMAGE NAME
1a2b3c4d5e6f  dev             2 hours ago   running  localhost/toolshop:jazzy-1a2b3c
9f8e7d6c5b4a  scratch         3 days ago    exited   localhost/toolshop:jazzy-1a2b3c
";
        let containers = parse_containers(output);
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].name, "dev");
        assert_eq!(containers[0].image, "localhost/toolshop:jazzy-1a2b3c");
        assert_eq!(containers[1].name, "scratch");
    }

    #[test]
    fn empty_or_header_only_output_yields_no_containers() {
        assert!(parse_containers("").is_empty());
        assert!(
            parse_containers("CONTAINER ID  CONTAINER NAME  CREATED  STATUS  IMAGE NAME\n")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_output_becomes_a_message_or_an_error() {
        use std::os::unix::process::ExitStatusExt;

        let ok = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"Created container: dev\n".to_vec(),
            stderr: vec![],
        };
        assert_eq!(
            combine_output(ok, "created toolbox from img".to_string()).unwrap(),
            "created toolbox from img: Created container: dev"
        );

        let failed = std::process::Output {
            status: std::process::ExitStatus::from_raw(256), // exit code 1
            stdout: vec![],
            stderr: b"Error: no such image\n".to_vec(),
        };
        let err = combine_output(failed, "created".to_string()).unwrap_err();
        assert!(format!("{err:#}").contains("no such image"), "{err:#}");
    }

    #[test]
    fn containers_are_matched_to_their_image() {
        let containers = vec![
            ToolboxContainer {
                name: "dev".to_string(),
                image: "localhost/toolshop:jazzy-1a2b3c".to_string(),
            },
            ToolboxContainer {
                name: "other".to_string(),
                image: "localhost/toolshop:jazzy-ffffff".to_string(),
            },
            ToolboxContainer {
                name: "unprefixed".to_string(),
                image: "toolshop:jazzy-1a2b3c".to_string(),
            },
        ];

        assert_eq!(
            containers_for("localhost/toolshop:jazzy-1a2b3c", &containers),
            vec!["dev".to_string(), "unprefixed".to_string()]
        );
        assert!(containers_for("localhost/toolshop:jazzy-999999", &containers).is_empty());
    }
}
