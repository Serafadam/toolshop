//! The configuration last used, so the next start continues where you left off.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};

use crate::{catalog::InstallOverride, container_builder::BuildOptions, distro};

/// Format version of the state file; a mismatch is ignored rather than guessed at.
pub const STATE_VERSION: u32 = 1;

/// Everything worth remembering between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedState {
    #[serde(default = "state_version")]
    pub version: u32,
    /// Image reference of the selected distro (names are not unique across families).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    /// Selected packages, sorted so the file is stable.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub packages: BTreeSet<String>,
    /// Versions chosen with `v`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<String, String>,
    /// Install commands the user replaced.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, InstallOverride>,
    #[serde(default)]
    pub options: BuildOptions,
    #[serde(default = "enabled")]
    pub show_local_images: bool,
    /// Folded package categories.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub collapsed_categories: BTreeSet<String>,
}

const fn state_version() -> u32 {
    STATE_VERSION
}

const fn enabled() -> bool {
    true
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            distro: None,
            packages: BTreeSet::new(),
            versions: BTreeMap::new(),
            overrides: BTreeMap::new(),
            options: BuildOptions::default(),
            show_local_images: true,
            collapsed_categories: BTreeSet::new(),
        }
    }
}

impl SavedState {
    /// True when nothing is worth restoring (first run, or after a reset).
    pub fn is_empty(&self) -> bool {
        self.distro.is_none()
            && self.packages.is_empty()
            && self.versions.is_empty()
            && self.overrides.is_empty()
    }

    /// How many things the state remembers, for the log line.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(distro) = &self.distro {
            parts.push(format!("distro {distro}"));
        }
        if !self.packages.is_empty() {
            parts.push(format!(
                "{} package{}",
                self.packages.len(),
                if self.packages.len() == 1 { "" } else { "s" }
            ));
        }
        if !self.versions.is_empty() {
            parts.push(format!("{} version choice(s)", self.versions.len()));
        }
        if !self.overrides.is_empty() {
            parts.push(format!("{} override(s)", self.overrides.len()));
        }
        if parts.is_empty() {
            "nothing".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// File the state is stored in.
pub fn path() -> PathBuf {
    distro::config_dir().join("state.toml")
}

/// Read the saved state; a missing file is not an error, a broken one is a warning.
pub fn load() -> (Option<SavedState>, Vec<String>) {
    load_from(&path())
}

pub fn load_from(path: &Path) -> (Option<SavedState>, Vec<String>) {
    let mut warnings = Vec::new();
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (None, warnings),
        Err(err) => {
            warnings.push(format!("cannot read {}: {err}", path.display()));
            return (None, warnings);
        }
    };

    match toml::from_str::<SavedState>(&contents) {
        Ok(state) if state.version == STATE_VERSION => (Some(state), warnings),
        Ok(state) => {
            warnings.push(format!(
                "{}: state version {} is not {STATE_VERSION}, ignoring it",
                path.display(),
                state.version
            ));
            (None, warnings)
        }
        Err(err) => {
            warnings.push(format!("cannot parse {}: {err}", path.display()));
            (None, warnings)
        }
    }
}

/// Persist `state`, creating the directory when needed.
pub fn save(state: &SavedState) -> Result<()> {
    save_to(&path(), state)
}

pub fn save_to(path: &Path, state: &SavedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("cannot create {}", parent.display()))?;
    }
    let contents = toml::to_string_pretty(state).wrap_err("cannot serialise the state")?;
    std::fs::write(path, contents).wrap_err_with(|| format!("cannot write {}", path.display()))
}

/// Delete the file. Returns `true` when one was there.
pub fn remove(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).wrap_err_with(|| format!("cannot remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::InstallMethod;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("toolshop-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample() -> SavedState {
        SavedState {
            distro: Some("docker.io/library/ubuntu:24.04".to_string()),
            packages: BTreeSet::from(["neovim".to_string(), "htop".to_string()]),
            versions: BTreeMap::from([("neovim".to_string(), "0.11.2".to_string())]),
            overrides: BTreeMap::from([(
                "htop".to_string(),
                InstallOverride {
                    method: InstallMethod::Script,
                    value: "apt-get install -y htop".to_string(),
                },
            )]),
            options: BuildOptions {
                user_setup: false,
                hostname_fix: true,
                hide_username: true,
                username: Some("dev".to_string()),
                env: std::collections::BTreeMap::from([(
                    "PS1".to_string(),
                    "\\u@\\h \\w \\$ ".to_string(),
                )]),
            },
            show_local_images: false,
            collapsed_categories: BTreeSet::from(["Shell".to_string()]),
            ..SavedState::default()
        }
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let dir = scratch("round-trip");
        let path = dir.join("state.toml");
        let state = sample();

        save_to(&path, &state).expect("save");
        let (loaded, warnings) = load_from(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded.expect("state"), state);

        // The file is readable TOML with the pieces spelled out.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ubuntu:24.04"), "{text}");
        assert!(text.contains("0.11.2"), "{text}");
        assert!(text.contains("method = \"script\""), "{text}");
        assert!(text.contains("hide_username = true"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_not_an_error_and_a_broken_one_is_a_warning() {
        let dir = scratch("broken");
        let path = dir.join("state.toml");
        let (state, warnings) = load_from(&path);
        assert!(state.is_none());
        assert!(warnings.is_empty(), "no file, no noise");

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "this is not toml").unwrap();
        let (state, warnings) = load_from(&path);
        assert!(state.is_none());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cannot parse"), "{warnings:?}");

        // A file from a future version is ignored instead of misread.
        std::fs::write(&path, "version = 99\n").unwrap();
        let (state, warnings) = load_from(&path);
        assert!(state.is_none());
        assert!(warnings[0].contains("state version 99"), "{warnings:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removing_reports_whether_a_file_was_there() {
        let dir = scratch("remove");
        let path = dir.join("state.toml");
        assert!(!remove(&path).unwrap(), "nothing to remove yet");
        save_to(&path, &sample()).unwrap();
        assert!(remove(&path).unwrap());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_summary_names_what_is_remembered() {
        let state = sample();
        let summary = state.summary();
        assert!(
            summary.contains("distro docker.io/library/ubuntu:24.04"),
            "{summary}"
        );
        assert!(summary.contains("2 packages"), "{summary}");
        assert!(summary.contains("1 version choice"), "{summary}");
        assert_eq!(SavedState::default().summary(), "nothing");
        assert!(SavedState::default().is_empty());
        assert!(!state.is_empty());
    }
}
