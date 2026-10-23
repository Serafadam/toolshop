//! Access to the local container engine (podman, then docker).

use std::time::Duration;

use color_eyre::{Result, eyre::eyre};
use tokio::process::Command;

/// Engines tried, in order.
pub const ENGINES: [&str; 2] = ["podman", "docker"];

/// How long a single engine query may take before the engine is considered unusable.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// An image known to the local engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// `repository:tag`, e.g. `localhost/toolshop:jazzy-1a2b3c`.
    pub reference: String,
    /// Image ID as reported by the engine (`podman images --format '{{.ID}}'`).
    pub id: String,
}

/// Every image the first usable engine knows about.
pub async fn images() -> Result<Vec<ImageRef>> {
    let mut errors = Vec::new();

    for engine in ENGINES {
        match query(
            engine,
            &["images", "--format", "{{.Repository}}:{{.Tag}} {{.ID}}"],
        )
        .await
        {
            Ok(output) => return Ok(parse_images(&output)),
            Err(err) => errors.push(format!("{engine}: {err}")),
        }
    }

    Err(eyre!("{}", errors.join("; ")))
}

/// The ID of `reference`, or `None` when the engine does not know that image.
pub async fn image_id(reference: &str) -> Result<Option<String>> {
    let mut errors = Vec::new();

    for engine in ENGINES {
        let query = query(
            engine,
            &["image", "inspect", "--format", "{{.Id}}", reference],
        )
        .await;

        match query {
            Ok(output) => {
                let id = output.trim().to_string();
                return Ok((!id.is_empty()).then_some(id));
            }
            Err(err) => errors.push(format!("{engine}: {err}")),
        }
    }

    Err(eyre!("{}", errors.join("; ")))
}

async fn query(engine: &str, args: &[&str]) -> Result<String> {
    // `kill_on_drop` matters: a timed-out query must not leave the child behind,
    // otherwise it keeps the terminal (and runtime shutdown) busy.
    let output = tokio::time::timeout(QUERY_TIMEOUT, {
        let mut command = Command::new(engine);
        command.args(args).kill_on_drop(true);
        command.output()
    })
    .await
    .map_err(|_| eyre!("timed out"))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "exited with {}: {}",
            output.status,
            stderr.lines().next().unwrap_or_default().trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `repository:tag id` lines into image references.
fn parse_images(output: &str) -> Vec<ImageRef> {
    output
        .lines()
        .filter_map(|line| {
            let (reference, id) = line.trim().rsplit_once(' ')?;
            // Dangling images have no reference we could match a manifest against.
            if reference.contains("<none>") {
                return None;
            }
            Some(ImageRef {
                reference: reference.to_string(),
                id: id.trim().to_string(),
            })
        })
        .collect()
}

/// Drop the default registry prefixes so references recorded in manifests and the
/// ones the engine prints compare equal.
pub fn normalize_image(image: &str) -> &str {
    image
        .strip_prefix("docker.io/library/")
        .or_else(|| image.strip_prefix("docker.io/"))
        .or_else(|| image.strip_prefix("localhost/"))
        .unwrap_or(image)
}

/// The short form of an image ID, so `sha256:abc…` and `abc…` compare equal.
pub fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    match id.get(..12) {
        Some(prefix) => prefix,
        None => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_parsed_from_engine_output() {
        let output = "localhost/toolshop:jazzy-1a2b3c 8f2c1d4a91b3\n\
                      docker.io/library/ros:jazzy-ros-base ffff11112222\n\
                      <none>:<none> 000000000000\n";
        let images = parse_images(output);

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].reference, "localhost/toolshop:jazzy-1a2b3c");
        assert_eq!(images[0].id, "8f2c1d4a91b3");
        assert_eq!(images[1].reference, "docker.io/library/ros:jazzy-ros-base");
    }

    #[test]
    fn malformed_lines_are_ignored() {
        assert!(parse_images("garbage\n\n").is_empty());
    }

    #[test]
    fn images_are_normalised_for_comparison() {
        assert_eq!(
            normalize_image("docker.io/library/toolshop:jazzy-1a2b3c"),
            "toolshop:jazzy-1a2b3c"
        );
        assert_eq!(
            normalize_image("localhost/toolshop:jazzy-1a2b3c"),
            "toolshop:jazzy-1a2b3c"
        );
        assert_eq!(normalize_image("quay.io/x/y:1"), "quay.io/x/y:1");
    }

    #[test]
    fn image_ids_compare_by_short_form() {
        assert_eq!(short_id("sha256:8f2c1d4a91b3aaaa"), "8f2c1d4a91b3");
        assert_eq!(short_id("8f2c1d4a91b3"), "8f2c1d4a91b3");
        assert_eq!(short_id("abc"), "abc");
    }
}
