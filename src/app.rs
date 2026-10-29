use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ratatui::{
    crossterm::event::{MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
    style::Color,
    widgets::ListState,
};
use tachyonfx::{Effect, EffectTimer, Interpolation, fx};
use tokio::{sync::mpsc::Sender, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    catalog::{self, Catalog, InstallMethod, InstallOverride, InstallSpec, Package},
    container_builder::{BuildOptions, ContainerBuilder},
    distro::{self, Distro, DistroFamily, DistroSource},
    manifest::{self, Manifest},
    state::SavedState,
    toolbox::{self, Toolbox, ToolboxScan},
};

/// Maximum number of build-log lines kept in memory.
const MAX_LOG_LINES: usize = 5_000;

/// Which tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Builder,
    Toolboxes,
    /// The build log on its own, full window.
    Logs,
}

impl View {
    pub const ALL: [View; 3] = [View::Builder, View::Toolboxes, View::Logs];

    pub fn label(self) -> &'static str {
        match self {
            View::Builder => "Builder",
            View::Toolboxes => "Toolboxes",
            View::Logs => "Logs",
        }
    }

    fn index(self) -> usize {
        match self {
            View::Builder => 0,
            View::Toolboxes => 1,
            View::Logs => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Distro,
    /// The build options, above the distro list.
    Options,
    Apps,
    Container,
}

impl Focus {
    /// Panes in layout order — top-left to bottom-right — which is also the order
    /// `→` walks and `←` walks back.
    pub const ALL: [Focus; 4] = [Focus::Options, Focus::Distro, Focus::Apps, Focus::Container];

    fn index(self) -> usize {
        Focus::ALL
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0)
    }

    fn from_index(index: usize) -> Self {
        Focus::ALL[index % Focus::ALL.len()]
    }
}

/// A row in the distro pane: either a family heading or a distro.
#[derive(Debug, Clone)]
pub enum DistroRow {
    Header { label: String, accent: usize },
    Distro { index: usize, label: String },
}

impl DistroRow {
    pub fn label(&self) -> &str {
        match self {
            DistroRow::Header { label, .. } | DistroRow::Distro { label, .. } => label,
        }
    }

    pub fn is_header(&self) -> bool {
        matches!(self, DistroRow::Header { .. })
    }

    fn distro_index(&self) -> Option<usize> {
        match self {
            DistroRow::Distro { index, .. } => Some(*index),
            DistroRow::Header { .. } => None,
        }
    }
}

/// A row in the package pane: either a category heading or a package.
#[derive(Debug, Clone)]
pub enum PackageRow {
    Header {
        label: String,
        accent: usize,
        /// Packages in this category on the current distro.
        count: usize,
        /// Whether the category is folded shut (its packages are hidden).
        collapsed: bool,
    },
    /// `visible` is an index into [`App::visible_packages`].
    Package { visible: usize, label: String },
}

impl PackageRow {
    pub fn label(&self) -> &str {
        match self {
            PackageRow::Header { label, .. } | PackageRow::Package { label, .. } => label,
        }
    }

    pub fn is_header(&self) -> bool {
        matches!(self, PackageRow::Header { .. })
    }

    /// Text shown in the package list: headings carry a fold marker and their count.
    pub fn display_label(&self) -> std::borrow::Cow<'_, str> {
        match self {
            PackageRow::Header {
                label,
                count,
                collapsed,
                ..
            } => std::borrow::Cow::Owned(format!(
                "{} {label} ({count})",
                if *collapsed { "▸" } else { "▾" }
            )),
            PackageRow::Package { label, .. } => std::borrow::Cow::Borrowed(label),
        }
    }

    fn visible_index(&self) -> Option<usize> {
        match self {
            PackageRow::Package { visible, .. } => Some(*visible),
            PackageRow::Header { .. } => None,
        }
    }
}

/// Which Containerfile lines should stand out for the package under the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerHighlight {
    /// The package itself, when it is part of the selection.
    pub primary: Option<String>,
    /// Packages it requires (transitively).
    pub dependencies: BTreeSet<String>,
}

impl ContainerHighlight {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.dependencies.is_empty()
    }
}

/// What the input overlay is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// A new base image plus the family it should be treated as.
    Image { family: DistroFamily },
    /// A replacement install command for an already selected package.
    Script { package: String },
    /// Name for a toolbox container created from `image`.
    Toolbox { image: String, suggested: String },
    /// Version to install a package with (`{{version}}` in its script).
    Version { package: String },
    /// Name to record in `ARG DOCKER_USER`.
    User,
    /// Name for the toolbox created after the next successful build.
    PendingToolbox {
        image: String,
        #[allow(dead_code)]
        suggested: String,
    },
    /// Package search; filters the list live while typing (`/`).
    Filter,
    /// Extra `ENV` variables for the generated Containerfile (`NAME=VALUE`).
    Env,
}

/// A single-line input overlay (image reference, install command, container name, …).
#[derive(Debug, Clone)]
pub struct Prompt {
    pub title: String,
    pub kind: PromptKind,
    pub input: String,
    /// True while the input is an untouched suggestion, which the first keystroke replaces.
    pristine: bool,
}

impl Prompt {
    pub fn image() -> Self {
        Self {
            title: "Add image".to_string(),
            kind: PromptKind::Image {
                family: DistroFamily::Ros,
            },
            input: String::new(),
            pristine: true,
        }
    }

    pub fn script(package: &str, current: &str) -> Self {
        Self {
            title: format!("Install command — {package}"),
            kind: PromptKind::Script {
                package: package.to_string(),
            },
            input: current.to_string(),
            pristine: true,
        }
    }

    /// Extra environment variables; `current` is the variable being edited, if any.
    pub fn env(current: Option<&str>) -> Self {
        Self {
            title: "Environment — NAME=VALUE, NAME alone removes".to_string(),
            kind: PromptKind::Env,
            input: current.unwrap_or_default().to_string(),
            pristine: current.is_some(),
        }
    }

    /// Package search, pre-filled with the filter in effect.
    pub fn filter(current: &str) -> Self {
        Self {
            title: "Filter packages".to_string(),
            kind: PromptKind::Filter,
            input: current.to_string(),
            pristine: false,
        }
    }

    /// Version of `package` to install.
    pub fn version(package: &str, current: &str) -> Self {
        Self {
            title: format!("Version — {package}"),
            kind: PromptKind::Version {
                package: package.to_string(),
            },
            input: current.to_string(),
            pristine: true,
        }
    }

    /// User name recorded in the Containerfile.
    pub fn user(current: &str) -> Self {
        Self {
            title: "User in the Containerfile".to_string(),
            kind: PromptKind::User,
            input: current.to_string(),
            pristine: true,
        }
    }

    /// Name for a toolbox created right away from an existing image.
    pub fn toolbox(image: &str) -> Self {
        let suggested = toolbox::default_container_name(image);
        Self {
            title: "Toolbox name".to_string(),
            kind: PromptKind::Toolbox {
                image: image.to_string(),
                suggested: suggested.clone(),
            },
            input: suggested,
            pristine: true,
        }
    }

    /// Name for the toolbox that the next build should also create.
    pub fn pending_toolbox(image: &str) -> Self {
        let suggested = toolbox::default_container_name(image);
        Self {
            title: "Toolbox name for this build".to_string(),
            kind: PromptKind::PendingToolbox {
                image: image.to_string(),
                suggested: suggested.clone(),
            },
            input: suggested,
            pristine: true,
        }
    }

    /// Family currently chosen for an image prompt.
    pub fn family(&self) -> Option<DistroFamily> {
        match self.kind {
            PromptKind::Image { family } => Some(family),
            _ => None,
        }
    }

    /// Image a toolbox prompt will build from.
    pub fn toolbox_image(&self) -> Option<&str> {
        match &self.kind {
            PromptKind::Toolbox { image, .. } | PromptKind::PendingToolbox { image, .. } => {
                Some(image.as_str())
            }
            _ => None,
        }
    }

    fn cycle_family(&mut self, forward: bool) {
        let PromptKind::Image { family } = &mut self.kind else {
            return;
        };
        let families = DistroFamily::ALL;
        let index = families
            .iter()
            .position(|known| known == family)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % families.len()
        } else {
            (index + families.len() - 1) % families.len()
        };
        *family = families[next];
    }
}

/// Everything the info popup shows about the package under the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub category: String,
    pub version: String,
    pub summary: String,
    pub requires: Vec<String>,
    /// How it is installed on the selected distro, e.g. `apt-get: htop`.
    pub install: String,
    /// Effective install method (after overrides).
    pub method: InstallMethod,
    /// Package name or script body in effect.
    pub value: String,
    /// True when the user replaced the catalog entry.
    pub overridden: bool,
    pub script: Option<String>,
    /// True when the version is the user's choice rather than the catalog default.
    pub version_chosen: bool,
    pub available: bool,
    pub distro: Option<String>,
    /// Whether the package is part of the current selection.
    pub selected: bool,
}

/// Which part of the build is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildPhase {
    Idle,
    /// Containerfile written, podman starting.
    Preparing,
    /// Steps are being executed.
    Running,
    /// Final image commit.
    Committing,
    Succeeded,
    Failed,
    Cancelled,
}

impl BuildPhase {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            BuildPhase::Preparing | BuildPhase::Running | BuildPhase::Committing
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            BuildPhase::Idle => "idle",
            BuildPhase::Preparing => "preparing",
            BuildPhase::Running => "building",
            BuildPhase::Committing => "committing",
            BuildPhase::Succeeded => "done",
            BuildPhase::Failed => "failed",
            BuildPhase::Cancelled => "cancelled",
        }
    }
}

/// Live view of the running build, parsed from podman's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildProgress {
    pub phase: BuildPhase,
    /// Current and total step, from `STEP n/m:` lines.
    pub step: Option<(u32, u32)>,
    started: Option<Instant>,
    finished: Option<Instant>,
    pub lines: usize,
    pub warnings: usize,
    pub errors: usize,
    /// Most recent output line, so the UI can show what is happening now.
    pub last_line: Option<String>,
}

impl Default for BuildProgress {
    fn default() -> Self {
        Self {
            phase: BuildPhase::Idle,
            step: None,
            started: None,
            finished: None,
            lines: 0,
            warnings: 0,
            errors: 0,
            last_line: None,
        }
    }
}

impl BuildProgress {
    /// Reset for a new build.
    pub fn start(&mut self) {
        *self = Self {
            phase: BuildPhase::Preparing,
            started: Some(Instant::now()),
            ..Self::default()
        };
    }

    pub fn is_active(&self) -> bool {
        self.phase.is_active()
    }

    /// Feed one log line through the parser.
    pub fn observe(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        if line.contains("Build finished") {
            self.phase = BuildPhase::Succeeded;
            self.finished = Some(Instant::now());
            if let Some((_, total)) = self.step {
                self.step = Some((total, total));
            }
            return;
        }
        if line.starts_with("Build failed") {
            self.phase = BuildPhase::Failed;
            self.finished = Some(Instant::now());
            self.errors = self.errors.max(1);
            return;
        }
        if line.starts_with("Build cancelled") {
            self.phase = BuildPhase::Cancelled;
            self.finished = Some(Instant::now());
            return;
        }

        // The first line podman's wrapper emits marks the start of a build.
        if line.starts_with("Building ") && !self.is_active() {
            self.phase = BuildPhase::Preparing;
            self.started = Some(Instant::now());
        }

        if !self.is_active() {
            return;
        }

        self.lines += 1;
        self.last_line = Some(truncate(line, 90));

        if let Some(step) = parse_step(line) {
            self.step = Some(step);
            self.phase = BuildPhase::Running;
        }
        if line.starts_with("COMMIT") {
            self.phase = BuildPhase::Committing;
        }
        if line.contains("level=warning") || line.starts_with("warning:") {
            self.warnings += 1;
        }
        if line.contains("level=error") || line.starts_with("error:") || line.starts_with("Error:")
        {
            self.errors += 1;
        }
    }

    /// Fraction of the build that is done, when podman told us the step count.
    pub fn ratio(&self) -> Option<f64> {
        match (self.phase, self.step) {
            (BuildPhase::Succeeded, _) => Some(1.0),
            (_, Some((current, total))) if total > 0 => {
                Some((f64::from(current) / f64::from(total)).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    /// Time spent building (or the final duration).
    pub fn elapsed(&self) -> Option<Duration> {
        let started = self.started?;
        Some(self.finished.unwrap_or_else(Instant::now) - started)
    }

    /// `STEP 2/5 · 12s · 1 warning` style summary; empty when idle.
    pub fn summary(&self) -> String {
        if self.phase == BuildPhase::Idle {
            return String::new();
        }

        let mut parts = Vec::new();
        match (self.phase, self.step) {
            (BuildPhase::Running | BuildPhase::Committing, Some((current, total))) => {
                parts.push(format!("STEP {current}/{total}"));
            }
            _ => parts.push(self.phase.label().to_string()),
        }
        if let Some(elapsed) = self.elapsed() {
            parts.push(format_duration(elapsed));
        }
        if self.warnings > 0 {
            parts.push(format!(
                "{} warning{}",
                self.warnings,
                if self.warnings == 1 { "" } else { "s" }
            ));
        }
        if self.errors > 0 {
            parts.push(format!(
                "{} error{}",
                self.errors,
                if self.errors == 1 { "" } else { "s" }
            ));
        }
        parts.join(" · ")
    }
}

/// `STEP 2/5:` → `(2, 5)`.
fn parse_step(line: &str) -> Option<(u32, u32)> {
    let rest = line.split_once("STEP ")?.1;
    let token = rest.split_whitespace().next()?.trim_end_matches(':');
    let (current, total) = token.split_once('/')?;
    Some((current.parse().ok()?, total.parse().ok()?))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Screen rectangles recorded by the renderer so mouse events can be routed.
#[derive(Debug, Clone, Default)]
pub struct HitAreas {
    /// One rect per tab in the tab bar.
    pub tabs: Vec<(View, Rect)>,
    pub distros: Rect,
    /// The build-option checkboxes.
    pub options: Rect,
    pub packages: Rect,
    pub containerfile: Rect,
    pub toolboxes: Rect,
    pub toolbox_details: Rect,
    pub logs: Rect,
    /// Info popup, when open.
    pub info_popup: Rect,
}

impl HitAreas {
    fn tab_at(&self, column: u16, row: u16) -> Option<View> {
        self.tabs
            .iter()
            .find(|(_, rect)| contains(*rect, column, row))
            .map(|(view, _)| *view)
    }
}

/// Inclusive-of-origin, exclusive-of-edge rect test.
fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Item index under `(column, row)` in a bordered list, taking its scroll offset into account.
fn list_index(rect: Rect, state: &ListState, len: usize, column: u16, row: u16) -> Option<usize> {
    let inner_x = rect.x + 1;
    let inner_y = rect.y + 1;
    let inner_width = rect.width.saturating_sub(2);
    let inner_height = rect.height.saturating_sub(2);
    if column < inner_x
        || column >= inner_x.saturating_add(inner_width)
        || row < inner_y
        || row >= inner_y.saturating_add(inner_height)
    {
        return None;
    }
    let index = state.offset() + (row - inner_y) as usize;
    (index < len).then_some(index)
}

/// Result of a background scan for local container images.
#[derive(Debug)]
pub struct DistroRefresh {
    pub distros: Vec<Distro>,
    pub warnings: Vec<String>,
}

/// One-line count of toolbox states, for the log pane.
fn summarize(toolboxes: &[Toolbox]) -> String {
    use crate::toolbox::ToolboxStatus;

    if toolboxes.is_empty() {
        return "none yet".to_string();
    }

    let (mut present, mut modified, mut missing, mut unknown) = (0, 0, 0, 0);
    for toolbox in toolboxes {
        match toolbox.status {
            ToolboxStatus::Present => present += 1,
            ToolboxStatus::Modified { .. } => modified += 1,
            ToolboxStatus::Missing => missing += 1,
            ToolboxStatus::Unknown => unknown += 1,
        }
    }

    [
        (present, "present"),
        (modified, "modified"),
        (missing, "missing"),
        (unknown, "unknown"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(", ")
}

#[derive(Debug)]
pub struct App {
    pub view: View,
    pub catalog: Catalog,
    pub builder: ContainerBuilder,
    pub distros: Vec<Distro>,
    /// Indices into `catalog.packages` for the current distro, in display order.
    pub visible_packages: Vec<usize>,
    /// Distros grouped by family, with a heading row per family.
    pub distro_rows: Vec<DistroRow>,
    /// Packages grouped by category, with a heading row per category.
    pub package_rows: Vec<PackageRow>,
    pub selected_distro: Option<usize>,
    pub selected_packages: BTreeSet<String>,
    pub distro_list_state: ListState,
    pub apps_list_state: ListState,
    pub focus: Focus,
    pub should_quit: bool,
    pub token: CancellationToken,
    pub process_running: bool,
    pub logs: VecDeque<String>,
    /// Lines scrolled up from the bottom of the log pane (0 = follow the tail).
    pub log_scroll: usize,
    /// Height of the log pane, written by the renderer.
    pub log_view_height: usize,
    /// Whether the package info popup is open.
    pub info_open: bool,
    /// Install instructions the user changed for individual packages.
    pub overrides: BTreeMap<String, InstallOverride>,
    /// Versions the user picked for individual packages (`{{version}}` in scripts).
    pub versions: BTreeMap<String, String>,
    /// Categories the user folded shut. Empty means everything is expanded (the default).
    pub collapsed_categories: BTreeSet<String>,
    /// Whether locally discovered images are listed under the distro families.
    pub show_local_images: bool,
    /// Package filter typed with `/` (empty means "show everything").
    pub filter: String,
    /// Filter in effect before the search prompt opened, restored by `Esc`.
    filter_before_prompt: Option<String>,
    /// Last state written to disk, so unchanged runs do not rewrite the file.
    saved_state: Option<SavedState>,
    /// Checkbox state for the global build options.
    pub options: BuildOptions,
    /// Which option row the cursor is on.
    pub options_cursor: usize,
    /// Open input overlay, if any.
    pub prompt: Option<Prompt>,
    /// Name of the toolbox to create once the next build succeeds, if any.
    pub pending_toolbox: Option<String>,
    /// Set when the running build succeeded, so `on_tick` can consume the pending name.
    build_ok: Option<Arc<AtomicBool>>,
    /// Errors while persisting user-added images.
    pub config_warnings: Vec<String>,
    /// Animations are disabled by tests so renders stay deterministic.
    pub animation: bool,
    /// Time since the previous frame, set by the event loop.
    pub frame_dt: Duration,
    pub info_effect: Option<Effect>,
    /// Toolshop-built images, newest first.
    pub toolboxes: Vec<Toolbox>,
    pub toolbox_state: ListState,
    /// Lines scrolled down in the toolbox details pane.
    pub toolbox_scroll: u16,
    /// Lines scrolled down in the Containerfile pane.
    pub container_scroll: u16,
    /// Progress of the current (or last) build.
    pub build: BuildProgress,
    /// Rects recorded by the last render, used to route mouse events.
    pub hit: HitAreas,
    /// Where the last configuration is remembered.
    pub state_file: PathBuf,
    /// Where manifests live; the real cache directory unless overridden.
    pub cache_dir: PathBuf,
    /// Where user-added images are persisted; overridable so tests stay isolated.
    pub images_file: PathBuf,
    /// Where the theme preference is persisted; overridable for tests.
    pub theme_file: PathBuf,
    log_tx: Sender<String>,
    distro_tx: Sender<DistroRefresh>,
    toolbox_tx: Sender<ToolboxScan>,
    build_handle: Option<JoinHandle<()>>,
    refresh_handle: Option<JoinHandle<()>>,
    toolbox_handle: Option<JoinHandle<()>>,
    toolbox_action: Option<JoinHandle<()>>,
}

/// One row of the options block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    pub label: String,
    /// Checkbox state, or "has a value" for the editable rows.
    pub enabled: bool,
    /// Value rows open a prompt instead of toggling.
    pub editable: bool,
}

impl App {
    /// Rows in the options block: the build options plus the toolbox name.
    pub const OPTIONS_ROWS: usize = BuildOptions::ROWS + 4;
    /// Index of the toolbox-name row.
    pub const TOOLBOX_ROW: usize = BuildOptions::ROWS;
    /// Row that shows or hides the images found on this machine.
    pub const LOCAL_IMAGES_ROW: usize = BuildOptions::ROWS + 1;
    /// Row that opens the prompt for extra `ENV` variables.
    pub const ENV_ROW: usize = BuildOptions::ROWS + 2;
    /// Row that forgets the configuration saved from the previous run.
    pub const RESET_ROW: usize = BuildOptions::ROWS + 3;

    pub fn new(
        catalog: Catalog,
        distros: Vec<Distro>,
        log_tx: Sender<String>,
        distro_tx: Sender<DistroRefresh>,
        toolbox_tx: Sender<ToolboxScan>,
    ) -> Self {
        let mut app = Self {
            view: View::Builder,
            catalog,
            builder: ContainerBuilder::default(),
            distros,
            visible_packages: vec![],
            distro_rows: vec![],
            package_rows: vec![],
            selected_distro: None,
            selected_packages: BTreeSet::new(),
            distro_list_state: ListState::default().with_selected(Some(0)),
            apps_list_state: ListState::default().with_selected(None),
            focus: Focus::Distro,
            should_quit: false,
            token: CancellationToken::new(),
            process_running: false,
            logs: VecDeque::new(),
            log_scroll: 0,
            log_view_height: 0,
            info_open: false,
            overrides: BTreeMap::new(),
            versions: BTreeMap::new(),
            collapsed_categories: BTreeSet::new(),
            show_local_images: true,
            filter: String::new(),
            filter_before_prompt: None,
            saved_state: None,
            options: BuildOptions::default(),
            options_cursor: 0,
            prompt: None,
            pending_toolbox: None,
            build_ok: None,
            config_warnings: vec![],
            animation: true,
            frame_dt: Duration::ZERO,
            info_effect: None,
            toolboxes: vec![],
            toolbox_state: ListState::default(),
            toolbox_scroll: 0,
            container_scroll: 0,
            build: BuildProgress::default(),
            hit: HitAreas::default(),
            cache_dir: manifest::cache_dir(),
            images_file: distro::images_file(),
            theme_file: crate::theme::config_file(),
            state_file: default_state_file(),
            log_tx,
            distro_tx,
            toolbox_tx,
            build_handle: None,
            refresh_handle: None,
            toolbox_handle: None,
            toolbox_action: None,
        };
        app.rebuild_views();
        // NB: the saved theme is applied by `main` before the app is built, so tests never
        // depend on (or mutate) whatever flavour the developer has persisted.
        app
    }

    pub fn quit(&mut self) {
        // Belt and braces: every change saves already, but never lose the last one.
        self.persist_state();
        self.should_quit = true;
    }

    // --- last configuration ------------------------------------------------

    /// Snapshot of everything worth remembering between runs.
    pub fn saved_state(&self) -> SavedState {
        SavedState {
            distro: self
                .selected_distro
                .and_then(|index| self.distros.get(index))
                .map(|distro| distro.image.clone()),
            packages: self.selected_packages.clone(),
            versions: self.versions.clone(),
            overrides: self.overrides.clone(),
            options: self.options.clone(),
            show_local_images: self.show_local_images,
            collapsed_categories: self.collapsed_categories.clone(),
            ..SavedState::default()
        }
    }

    /// Write the current configuration when it differs from what was last written.
    pub fn persist_state(&mut self) {
        let state = self.saved_state();
        if self.saved_state.as_ref() == Some(&state) {
            return;
        }
        match crate::state::save_to(&self.state_file, &state) {
            Ok(()) => self.saved_state = Some(state),
            Err(err) => {
                // Only complain once per distinct failure, not on every keystroke.
                self.append_log(format!("warning: cannot save the configuration: {err:#}"));
                self.saved_state = Some(state);
            }
        }
    }

    /// Apply a configuration read from disk, dropping anything the catalog no longer has.
    pub fn restore_state(&mut self, saved: &SavedState) {
        let mut dropped = Vec::new();

        if let Some(image) = &saved.distro {
            match self
                .distros
                .iter()
                .position(|distro| &distro.image == image)
            {
                Some(index) => {
                    self.selected_distro = Some(index);
                    let distro = self.distros[index].clone();
                    self.builder.set_distro(distro);
                }
                None => dropped.push(format!("distro {image}")),
            }
        }

        self.show_local_images = saved.show_local_images;
        self.options = saved.options.clone();
        self.collapsed_categories = saved.collapsed_categories.clone();
        self.versions = saved
            .versions
            .iter()
            .filter(|(name, _)| self.catalog.get(name).is_some())
            .map(|(name, version)| (name.clone(), version.clone()))
            .collect();
        self.overrides = saved
            .overrides
            .iter()
            .filter(|(name, _)| self.catalog.get(name).is_some())
            .map(|(name, over)| (name.clone(), over.clone()))
            .collect();

        // Only packages that exist and that the restored distro can actually install.
        let distro = self.current_distro().cloned();
        let mut packages = BTreeSet::new();
        for name in &saved.packages {
            let usable = self.catalog.get(name).is_some_and(|package| match &distro {
                Some(distro) => package.is_available_on(distro),
                None => true,
            });
            if usable {
                packages.insert(name.clone());
            } else {
                dropped.push(name.clone());
            }
        }
        self.selected_packages = packages;

        self.rebuild_views();
        self.rebuild_container_file();
        self.saved_state = Some(saved.clone());

        self.append_log(format!(
            "restored the last configuration: {}",
            saved.summary()
        ));
        if !dropped.is_empty() {
            self.append_log(format!(
                "dropped from the saved configuration: {}",
                dropped.join(", ")
            ));
        }
    }

    /// Forget the remembered configuration: clear the selection and delete the file.
    pub fn reset_saved_state(&mut self) {
        let had_file = crate::state::remove(&self.state_file).unwrap_or_else(|err| {
            self.append_log(format!("warning: cannot remove the saved state: {err:#}"));
            false
        });

        let had_selection = !self.selected_packages.is_empty()
            || self.selected_distro.is_some()
            || !self.versions.is_empty()
            || !self.overrides.is_empty();

        self.selected_packages.clear();
        self.versions.clear();
        self.overrides.clear();
        self.options = BuildOptions::default();
        self.show_local_images = true;
        self.collapsed_categories.clear();
        self.filter.clear();
        self.selected_distro = None;
        self.builder.set_distro_option(None);
        self.saved_state = None;

        self.rebuild_views();
        self.rebuild_container_file();

        self.append_log(match (had_file, had_selection) {
            (true, _) | (_, true) => "last configuration reset".to_string(),
            (false, false) => "nothing saved to reset".to_string(),
        });
    }

    // --- extra environment variables ---------------------------------------

    /// Ask for an `ENV` variable: `NAME=VALUE` adds or replaces, `NAME` removes, `-` clears.
    pub fn open_env_prompt(&mut self) {
        let current = match self.options.env.len() {
            1 => self
                .options
                .env
                .iter()
                .next()
                .map(|(key, value)| format!("{key}={value}")),
            _ => None,
        };
        self.prompt = Some(Prompt::env(current.as_deref()));
    }

    /// Apply what was typed into the environment prompt.
    pub fn set_env(&mut self, input: &str) {
        let input = input.trim();

        if input == "-" || input == "*" {
            let count = self.options.env.len();
            self.options.env.clear();
            self.append_log(match count {
                0 => "env: nothing to clear".to_string(),
                count => format!("env: cleared {count} variable(s)"),
            });
            self.rebuild_container_file();
            self.persist_state();
            return;
        }

        match input.split_once('=') {
            Some((key, value)) => {
                let (key, value) = (key.trim(), value.trim());
                if let Err(err) = catalog::validate_env(key, value) {
                    self.append_log(format!("env: {err}"));
                    return;
                }
                let replaced = self
                    .options
                    .env
                    .insert(key.to_string(), value.to_string())
                    .is_some();
                self.append_log(format!(
                    "env: {key}={value} ({})",
                    if replaced { "replaced" } else { "added" }
                ));
            }
            None => {
                // A bare name removes it, which is how a variable gets deleted.
                if !catalog::is_valid_env_name(input) {
                    self.append_log(format!(
                        "env: {input:?} is not a variable name and has no =VALUE"
                    ));
                    return;
                }
                match self.options.env.remove(input) {
                    Some(value) => self.append_log(format!("env: {input}={value} removed")),
                    None => {
                        self.append_log(format!("env: {input} is not set"));
                        return;
                    }
                }
            }
        }

        self.rebuild_container_file();
        self.persist_state();
    }

    // --- package filter ----------------------------------------------------

    /// True when a filter is narrowing the package list.
    pub fn filter_active(&self) -> bool {
        !self.filter.trim().is_empty()
    }

    /// Narrow the package list to `text` (`/`), matching name, summary and category.
    pub fn set_filter(&mut self, text: &str) {
        self.filter = text.to_string();
        self.rebuild_package_views();
    }

    /// Drop the filter and show every package again.
    pub fn clear_filter(&mut self) {
        if !self.filter_active() {
            return;
        }
        self.filter.clear();
        self.rebuild_package_views();
        self.append_log("filter cleared".to_string());
    }

    /// Ask for a package name or summary (`/`), filtering as you type.
    pub fn open_filter_prompt(&mut self) {
        self.filter_before_prompt = Some(self.filter.clone());
        self.prompt = Some(Prompt::filter(&self.filter));
    }

    /// Undo the live filtering a cancelled search applied.
    fn cancel_filter_prompt(&mut self) {
        let previous = self.filter_before_prompt.take().unwrap_or_default();
        self.set_filter(&previous);
    }

    /// Cycle to the next Catppuccin flavour (`t`) and remember it.
    pub fn cycle_theme(&mut self) {
        let flavour = crate::theme::cycle();
        match crate::theme::save_to(&self.theme_file, flavour) {
            Ok(()) => self.append_log(format!("Theme: {}", flavour.name())),
            Err(err) => {
                self.append_log(format!("Theme: {} (cannot save: {err:#})", flavour.name()))
            }
        }
    }

    /// Called once per frame: picks up work that finished in the background.
    pub fn on_tick(&mut self) {
        let finished = |handle: &Option<JoinHandle<()>>| {
            handle
                .as_ref()
                .is_some_and(|handle: &JoinHandle<()>| handle.is_finished())
        };

        if finished(&self.build_handle) {
            self.build_handle = None;
            self.process_running = false;

            let succeeded = self
                .build_ok
                .take()
                .is_some_and(|flag| flag.load(Ordering::Relaxed));
            if succeeded && self.pending_toolbox.take().is_some() {
                self.append_log("Toolbox name used; press n to set another one.".to_string());
            }

            // A build may have added a toolbox.
            self.refresh_toolboxes();
        }
        if finished(&self.refresh_handle) {
            self.refresh_handle = None;
        }
        if finished(&self.toolbox_handle) {
            self.toolbox_handle = None;
        }
        if finished(&self.toolbox_action) {
            self.toolbox_action = None;
            self.refresh_toolboxes();
        }
    }

    // --- views -------------------------------------------------------------

    pub fn switch_view(&mut self, view: View) {
        self.view = view;
        if view == View::Toolboxes && self.toolboxes.is_empty() {
            self.refresh_toolboxes();
        }
    }

    pub fn next_view(&mut self) {
        let index = (self.view.index() + 1) % View::ALL.len();
        self.switch_view(View::ALL[index]);
    }

    /// Cycle to the previous tab (Shift+Tab).
    pub fn previous_view(&mut self) {
        let index = (self.view.index() + View::ALL.len() - 1) % View::ALL.len();
        self.switch_view(View::ALL[index]);
    }

    /// Click the logs tab open from the builder, and back again (`L`).
    pub fn toggle_logs_view(&mut self) {
        match self.view {
            View::Logs => self.switch_view(View::Builder),
            View::Builder | View::Toolboxes => self.switch_view(View::Logs),
        }
    }

    pub fn view_index(&self) -> usize {
        self.view.index()
    }

    // --- distros -----------------------------------------------------------

    pub fn current_distro(&self) -> Option<&Distro> {
        self.selected_distro
            .and_then(|index| self.distros.get(index))
    }

    pub fn current_distro_name(&self) -> Option<&str> {
        self.current_distro().map(|distro| distro.name.as_str())
    }

    /// Select the distro under the cursor, which is what the Containerfile is built from.
    fn select_distro_under_cursor(&mut self) {
        let Some(row) = self
            .distro_list_state
            .selected()
            .and_then(|index| self.distro_rows.get(index))
        else {
            return;
        };
        let Some(index) = row.distro_index() else {
            return;
        };
        if self.selected_distro == Some(index) {
            return;
        }
        let Some(distro) = self.distros.get(index).cloned() else {
            return;
        };

        self.selected_distro = Some(index);
        self.builder.set_distro(distro);
        self.rebuild_views();
        self.rebuild_container_file();
        self.persist_state();
    }

    /// Rescan local container images in the background.
    pub fn refresh_distros(&mut self) {
        if self
            .refresh_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return;
        }
        // Unit tests and other sync callers have no runtime to spawn on.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        self.append_log("Scanning local container images...".to_string());
        let tx = self.distro_tx.clone();
        self.refresh_handle = Some(runtime.spawn(async move {
            let (distros, warnings) = distro::available().await;
            let _ = tx.send(DistroRefresh { distros, warnings }).await;
        }));
    }

    pub fn apply_distro_refresh(&mut self, refresh: DistroRefresh) {
        for warning in &refresh.warnings {
            self.append_log(format!("warning: {warning}"));
        }

        let current = self.current_distro().map(|distro| distro.image.clone());
        let local_count = refresh
            .distros
            .iter()
            .filter(|distro| distro.source == DistroSource::Local)
            .count();
        self.distros = refresh.distros;
        self.append_log(format!("Found {local_count} local image(s)."));

        // Keep pointing at the same image if it is still available.
        self.selected_distro = current
            .and_then(|image| self.distros.iter().position(|distro| distro.image == image))
            .or(self.selected_distro);

        // The selected entry may have been removed or re-indexed.
        match self
            .selected_distro
            .and_then(|index| self.distros.get(index))
        {
            Some(distro) => {
                let distro = distro.clone();
                self.builder.set_distro(distro);
            }
            None => {
                self.selected_distro = None;
                self.builder.set_distro_option(None);
            }
        }

        self.rebuild_views();
        self.rebuild_container_file();
    }

    /// Register (or re-classify) a user image and persist it.
    pub fn add_custom_distro(&mut self, image: &str, family: DistroFamily) {
        let Some(distro) = distro::Distro::custom(image, family) else {
            self.append_log(format!("{image}: not a usable image reference"));
            return;
        };

        let index = match self
            .distros
            .iter()
            .position(|known| known.normalized_image() == distro.normalized_image())
        {
            Some(index) => {
                self.distros[index] = distro.clone();
                index
            }
            None => {
                self.distros.push(distro.clone());
                self.distros.len() - 1
            }
        };

        self.persist_custom_distros();
        self.append_log(format!(
            "Added {} as {} ({})",
            distro.image,
            family.label(),
            distro.name
        ));

        self.selected_distro = Some(index);
        self.builder.set_distro(distro);
        self.rebuild_views();
        self.rebuild_container_file();
    }

    /// Remove the selected user-added image, if it is one.
    pub fn remove_custom_distro(&mut self) {
        let Some(index) = self.selected_distro else {
            return;
        };
        let Some(distro) = self.distros.get(index).cloned() else {
            return;
        };
        if !distro.is_custom() {
            self.append_log(format!("{} is not a user-added image.", distro.image));
            return;
        }

        self.distros.retain(|known| known.image != distro.image);
        self.persist_custom_distros();
        self.append_log(format!("Removed {}", distro.image));

        self.selected_distro = None;
        self.builder.set_distro_option(None);
        self.rebuild_views();
        self.rebuild_container_file();
    }

    fn persist_custom_distros(&mut self) {
        if let Err(err) = distro::save_custom_to(&self.images_file, &self.distros) {
            let warning = format!("cannot save user images: {err:#}");
            self.append_log(warning.clone());
            self.config_warnings.push(warning);
        }
    }

    /// Recompute the grouped distro rows, the filtered package list and the labels.
    fn rebuild_views(&mut self) {
        self.rebuild_distro_rows();
        self.rebuild_package_views();
    }

    /// Regroup distros by family, keeping one heading row per non-empty family.
    fn rebuild_distro_rows(&mut self) {
        let mut rows = Vec::new();
        for family in DistroFamily::ALL {
            let mut indices: Vec<usize> = self
                .distros
                .iter()
                .enumerate()
                .filter(|(_, distro)| distro.family == family)
                .filter(|(_, distro)| {
                    self.show_local_images || distro.source != DistroSource::Local
                })
                .map(|(index, _)| index)
                .collect();
            if indices.is_empty() {
                continue;
            }
            // Predefined first (declared order), then user-added, then discovered images.
            indices.sort_by_key(|index| match self.distros[*index].source {
                DistroSource::Predefined => (0, *index),
                DistroSource::Custom => (1, *index),
                DistroSource::Local => (2, *index),
            });

            let accent = DistroFamily::ALL
                .iter()
                .position(|known| *known == family)
                .unwrap_or(0);
            rows.push(DistroRow::Header {
                label: family.label().to_string(),
                accent,
            });
            rows.extend(indices.into_iter().map(|index| DistroRow::Distro {
                index,
                label: self.distros[index].label(),
            }));
        }
        self.distro_rows = rows;

        // Park the cursor on a distro, never on a heading.
        if self.distro_rows.is_empty() {
            self.distro_list_state.select(None);
        } else {
            let current = self.distro_list_state.selected().unwrap_or(0);
            let index = if self
                .distro_rows
                .get(current)
                .is_some_and(|row| !row.is_header())
            {
                current
            } else {
                self.first_distro_row().unwrap_or(0)
            };
            self.distro_list_state.select(Some(index));
        }
    }

    fn first_distro_row(&self) -> Option<usize> {
        self.distro_rows.iter().position(|row| !row.is_header())
    }

    /// Move the distro cursor, skipping family headings.
    fn step_distro_cursor(&mut self, forward: bool) {
        let Some(current) = self.distro_list_state.selected() else {
            self.distro_list_state.select(self.first_distro_row());
            return;
        };

        let next = if forward {
            (current + 1..self.distro_rows.len())
                .find(|index| !self.distro_rows[*index].is_header())
        } else {
            (0..current)
                .rev()
                .find(|index| !self.distro_rows[*index].is_header())
        };

        if let Some(index) = next {
            self.distro_list_state.select(Some(index));
        }
    }

    fn rebuild_package_views(&mut self) {
        let distro = self.current_distro().cloned();
        let needle = self.filter.trim().to_lowercase();
        self.visible_packages = self
            .catalog
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| match &distro {
                Some(distro) => package.is_available_on(distro),
                None => true,
            })
            .filter(|(_, package)| needle.is_empty() || package_matches(package, &needle))
            .map(|(index, _)| index)
            .collect();

        // Group the visible packages by category, in the canonical category order.
        let mut grouped: Vec<(String, Vec<usize>)> = Vec::new();
        for (visible, package_index) in self.visible_packages.iter().enumerate() {
            let Some(package) = self.catalog.packages.get(*package_index) else {
                continue;
            };
            let category = catalog::category_of(package).to_string();
            match grouped
                .iter_mut()
                .find(|(known, _)| known.eq_ignore_ascii_case(&category))
            {
                Some((_, rows)) => rows.push(visible),
                None => grouped.push((category, vec![visible])),
            }
        }

        grouped.sort_by_key(|(category, _)| catalog::category_rank(category));

        let mut rows = Vec::new();
        for (category, visible_rows) in grouped {
            let accent = catalog::category_rank(&category).0;
            let collapsed = self
                .collapsed_categories
                .iter()
                .any(|folded| folded.eq_ignore_ascii_case(&category));
            rows.push(PackageRow::Header {
                label: category.clone(),
                accent,
                count: visible_rows.len(),
                collapsed,
            });
            if collapsed {
                continue;
            }
            for visible in visible_rows {
                let label = self
                    .visible_packages
                    .get(visible)
                    .and_then(|index| self.catalog.packages.get(*index))
                    .map(|package| match &distro {
                        Some(distro) => package.label_for(distro),
                        None => package.name.clone(),
                    })
                    .unwrap_or_default();
                rows.push(PackageRow::Package { visible, label });
            }
        }
        self.package_rows = rows;

        // Keep the cursor on the row it was on (headings included, so a folded category can
        // be reopened). A fresh list starts on the first package, not on a heading.
        if self.package_rows.is_empty() {
            self.apps_list_state.select(None);
        } else {
            match self.apps_list_state.selected() {
                Some(index) if index < self.package_rows.len() => {}
                Some(index) => {
                    self.apps_list_state
                        .select(Some(index.min(self.package_rows.len() - 1)));
                }
                None => {
                    let first = self
                        .package_rows
                        .iter()
                        .position(|row| !row.is_header())
                        .unwrap_or(0);
                    self.apps_list_state.select(Some(first));
                }
            }
        }
    }

    // --- packages ----------------------------------------------------------

    pub fn package_under_cursor(&self) -> Option<&Package> {
        let row = self
            .apps_list_state
            .selected()
            .and_then(|index| self.package_rows.get(index))?;
        let visible = row.visible_index()?;
        let package_index = self.visible_packages.get(visible)?;
        self.catalog.packages.get(*package_index)
    }

    /// Description of the package under the cursor, for the info popup.
    pub fn package_info(&self) -> Option<PackageInfo> {
        let package = self.package_under_cursor()?;
        let distro = self.current_distro();
        // Resolve through the overrides, so the popup shows what will really be built.
        let spec = self.resolved_spec(&package.name);

        let install = match (&spec, distro) {
            (Some(InstallSpec::PackageManager { pkg }), Some(distro)) => {
                format!("{}: {pkg}", distro.family.package_manager().command())
            }
            (Some(InstallSpec::Script { .. }), _) => "shell script".to_string(),
            _ => "unavailable on this distro".to_string(),
        };

        let overridden = self.overrides.contains_key(&package.name);
        let method = match &spec {
            Some(InstallSpec::Script { .. }) => InstallMethod::Script,
            _ => InstallMethod::PackageManager,
        };
        // Show what will run, not the recipe: the chosen version and the architecture
        // placeholder expanded, so picking a version visibly changes the command.
        let version = self.effective_version(&package.name);
        let expand = |script: &str| catalog::expand_script(script, &version);
        let value = match &spec {
            Some(InstallSpec::PackageManager { pkg }) => pkg.clone(),
            Some(InstallSpec::Script { script }) => expand(script),
            None => String::new(),
        };

        Some(PackageInfo {
            name: package.name.clone(),
            category: catalog::category_of(package).to_string(),
            version: version.clone(),
            summary: package.summary.clone(),
            requires: package.requires.clone(),
            install,
            method,
            value,
            overridden,
            version_chosen: self.versions.contains_key(&package.name),
            script: match &spec {
                Some(InstallSpec::Script { script }) => Some(expand(script)),
                _ => None,
            },
            available: distro.is_none_or(|distro| package.is_available_on(distro)),
            distro: distro.map(|distro| distro.name.clone()),
            selected: self.selected_packages.contains(&package.name),
        })
    }

    /// Version in effect for `name`: the user's choice or the catalog entry.
    fn effective_version(&self, name: &str) -> String {
        self.catalog
            .get(name)
            .map(|package| catalog::effective_version(package, &self.versions).to_string())
            .unwrap_or_default()
    }

    /// Install instruction in effect for `name` on the current distro.
    fn resolved_spec(&self, name: &str) -> Option<InstallSpec> {
        let distro = self.current_distro()?;
        let package = self.catalog.get(name)?;
        catalog::resolve_install(package, distro, &self.overrides)
    }

    /// Category of the heading under the cursor, if the cursor is on one.
    pub fn package_header_under_cursor(&self) -> Option<String> {
        let row = self
            .apps_list_state
            .selected()
            .and_then(|index| self.package_rows.get(index))?;
        match row {
            PackageRow::Header { label, .. } => Some(label.clone()),
            PackageRow::Package { .. } => None,
        }
    }

    /// Fold or unfold a category.
    pub fn toggle_category(&mut self, category: &str) {
        let folded = self
            .collapsed_categories
            .iter()
            .find(|known| known.eq_ignore_ascii_case(category))
            .cloned();

        match folded {
            Some(known) => {
                self.collapsed_categories.remove(&known);
                self.append_log(format!("{category}: expanded"));
            }
            None => {
                self.collapsed_categories.insert(category.to_string());
                self.append_log(format!("{category}: collapsed"));
            }
        }

        self.rebuild_package_views();
        self.park_cursor_on_category(category);
        self.persist_state();
    }

    /// Show or hide the images discovered on this machine.
    pub fn toggle_local_images(&mut self) {
        self.show_local_images = !self.show_local_images;
        let local = self
            .distros
            .iter()
            .filter(|distro| distro.source == DistroSource::Local)
            .count();
        self.append_log(match (self.show_local_images, local) {
            (true, 1) => "local images: shown (1 image)".to_string(),
            (true, count) => format!("local images: shown ({count} images)"),
            (false, 1) => "local images: hidden (1 image)".to_string(),
            (false, count) => format!("local images: hidden ({count} images)"),
        });

        // Hiding the selected distro must not leave the builder pointing at an invisible row.
        let hidden_selection = self
            .selected_distro
            .and_then(|index| self.distros.get(index))
            .is_some_and(|distro| !self.show_local_images && distro.source == DistroSource::Local);

        self.rebuild_views();
        if hidden_selection {
            // Fall back to the first distro still on screen; with nothing left, no distro.
            let next = self
                .first_distro_row()
                .and_then(|row| self.distro_rows.get(row))
                .and_then(DistroRow::distro_index);
            self.selected_distro = next;
            match next.and_then(|index| self.distros.get(index)).cloned() {
                Some(distro) => self.builder.set_distro(distro),
                None => self.builder.set_distro_option(None),
            }
            self.rebuild_views();
        }
        self.rebuild_container_file();
        self.persist_state();
    }

    /// Categories currently offered by the selected distro.
    fn categories_on_screen(&self) -> Vec<String> {
        let mut categories: Vec<String> = Vec::new();
        for index in &self.visible_packages {
            let Some(package) = self.catalog.packages.get(*index) else {
                continue;
            };
            let category = catalog::category_of(package).to_string();
            if !categories
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&category))
            {
                categories.push(category);
            }
        }
        categories
    }

    /// Fold every category, or unfold them all when anything is folded (`z`).
    pub fn toggle_all_categories(&mut self) {
        let categories = self.categories_on_screen();
        if categories.is_empty() {
            return;
        }

        if self.collapsed_categories.is_empty() {
            self.collapsed_categories = categories.iter().cloned().collect();
            self.append_log(format!("collapsed {} categories", categories.len()));
        } else {
            let folded = self.collapsed_categories.len();
            self.collapsed_categories.clear();
            self.append_log(format!("expanded {folded} categories"));
        }

        // Keep the cursor on the heading of the category it was on, if any: after folding
        // every package row is gone, so the cursor would otherwise point past the list.
        let rows = self.package_rows.clone();
        let cursor_category = self
            .apps_list_state
            .selected()
            .and_then(|index| rows.get(index))
            .and_then(|row| match row {
                PackageRow::Header { label, .. } => Some(label.clone()),
                PackageRow::Package { label, .. } => label
                    .split(' ')
                    .next()
                    .and_then(|name| self.category_of_package(name)),
            });

        self.rebuild_package_views();
        match cursor_category {
            Some(category) => self.park_cursor_on_category(&category),
            None => self.apps_list_state.select(Some(0)),
        }
    }

    /// Category of the package called `name`, if the catalog knows it.
    fn category_of_package(&self, name: &str) -> Option<String> {
        self.catalog
            .packages
            .iter()
            .find(|package| package.name == name)
            .map(|package| catalog::category_of(package).to_string())
    }

    /// Put the cursor on the heading of `category`.
    fn park_cursor_on_category(&mut self, category: &str) {
        let index = self.package_rows.iter().position(|row| match row {
            PackageRow::Header { label, .. } => label.eq_ignore_ascii_case(category),
            PackageRow::Package { .. } => false,
        });
        self.apps_list_state.select(index.or(Some(0)));
    }

    /// Toggle the package under the cursor, including its requirements.
    fn toggle_package_under_cursor(&mut self) {
        let Some(name) = self
            .package_under_cursor()
            .map(|package| package.name.clone())
        else {
            return;
        };

        if self.selected_packages.remove(&name) {
            self.overrides.remove(&name);
            self.prune_orphaned_requirements(&name);
        } else {
            self.selected_packages.insert(name.clone());
            self.add_requirements_of(&name);
        }

        self.rebuild_container_file();
        self.persist_state();
    }

    /// Select every package available on the current distro, or clear the selection when
    /// everything visible is already selected (`Ctrl+A`).
    pub fn toggle_all_packages(&mut self) {
        let visible: Vec<String> = self
            .visible_packages
            .iter()
            .filter_map(|index| self.catalog.packages.get(*index))
            .map(|package| package.name.clone())
            .collect();

        if visible.is_empty() {
            self.append_log("No packages available for this distro.".to_string());
            return;
        }

        let all_selected = visible
            .iter()
            .all(|name| self.selected_packages.contains(name));

        if all_selected {
            for name in &visible {
                self.selected_packages.remove(name);
                self.overrides.remove(name);
            }
            for name in &visible {
                self.prune_orphaned_requirements(name);
            }
            self.append_log(format!(
                "Cleared {} package(s); {} still selected.",
                visible.len(),
                self.selected_packages.len()
            ));
        } else {
            let mut added = 0;
            for name in &visible {
                if self.selected_packages.insert(name.clone()) {
                    added += 1;
                }
                self.add_requirements_of(name);
            }
            self.append_log(format!(
                "Selected {added} package(s); {} in total.",
                self.selected_packages.len()
            ));
        }

        self.rebuild_container_file();
    }

    fn add_requirements_of(&mut self, name: &str) {
        let Some(requirements) = self.catalog.get(name).map(|p| p.requires.clone()) else {
            return;
        };
        for requirement in requirements {
            if self.catalog.get(&requirement).is_none() {
                continue;
            }
            if self.selected_packages.insert(requirement.clone()) {
                self.add_requirements_of(&requirement);
            }
        }
    }

    /// Drop requirements of `removed` that no other selected package still needs.
    fn prune_orphaned_requirements(&mut self, removed: &str) {
        let Some(requirements) = self.catalog.get(removed).map(|p| p.requires.clone()) else {
            return;
        };

        for requirement in requirements {
            if !self.selected_packages.contains(&requirement)
                || self.selected_packages.iter().any(|selected| {
                    selected != &requirement
                        && self
                            .catalog
                            .get(selected)
                            .is_some_and(|package| package.requires.contains(&requirement))
                })
            {
                continue;
            }
            self.selected_packages.remove(&requirement);
            self.prune_orphaned_requirements(&requirement);
        }
    }

    /// Flip the package under the cursor between package-manager and script installs.
    pub fn toggle_install_method(&mut self) {
        let Some(package) = self.package_under_cursor().map(|p| p.name.clone()) else {
            return;
        };

        let current_method = match self.overrides.get(&package).map(|over| over.method) {
            Some(method) => method,
            None => match self.resolved_spec(&package) {
                Some(InstallSpec::Script { .. }) => InstallMethod::Script,
                _ => InstallMethod::PackageManager,
            },
        };
        let method = current_method.flipped();

        let value = match method {
            InstallMethod::Script => match self.resolved_spec(&package) {
                Some(InstallSpec::Script { script }) => script,
                Some(InstallSpec::PackageManager { pkg }) => self.install_command(&pkg),
                None => self.install_command(&package),
            },
            InstallMethod::PackageManager => match self.resolved_spec(&package) {
                Some(InstallSpec::PackageManager { pkg }) => pkg,
                _ => package.clone(),
            },
        };

        self.overrides
            .insert(package.clone(), InstallOverride { method, value });
        self.append_log(format!(
            "{package}: install method set to {}",
            method.label()
        ));
        self.rebuild_container_file();
        self.persist_state();
    }

    /// Drop the install override *and* the chosen version of the package under the cursor.
    pub fn reset_install_override(&mut self) {
        let Some(package) = self.package_under_cursor().map(|p| p.name.clone()) else {
            return;
        };
        let install = self.overrides.remove(&package).is_some();
        let version = self.versions.remove(&package);
        if !install && version.is_none() {
            return;
        }

        self.append_log(match (install, version) {
            (true, Some(version)) => {
                format!("{package}: install and version {version} reset to the catalog entry")
            }
            (true, None) => format!("{package}: install reset to the catalog entry"),
            (false, Some(version)) => {
                format!("{package}: version {version} reset to the catalog default")
            }
            (false, None) => return,
        });
        self.rebuild_container_file();
        self.persist_state();
    }

    /// A one-line install command for `pkg` using the current distro's package manager.
    fn install_command(&self, pkg: &str) -> String {
        let manager = self
            .current_distro()
            .map(|distro| distro.family.package_manager())
            .unwrap_or(crate::distro::PackageManager::Apt);
        match manager {
            crate::distro::PackageManager::Apt => {
                format!("apt-get update && apt-get install -y --no-install-recommends {pkg}")
            }
            crate::distro::PackageManager::Dnf => format!("dnf install -y {pkg}"),
        }
    }

    fn rebuild_container_file(&mut self) {
        // New content starts at the top (the `FROM` line must be visible).
        self.container_scroll = 0;

        let packages: Vec<Package> = self
            .catalog
            .packages
            .iter()
            .filter(|package| self.selected_packages.contains(&package.name))
            .cloned()
            .collect();
        self.builder.set_packages(packages);
        self.builder.set_overrides(self.overrides.clone());
        self.builder.set_versions(self.versions.clone());
        self.builder.set_options(self.options.clone());
        self.builder.create_container_file();
    }

    /// Regenerate the Containerfile from the current selection.
    pub fn create_container_file(&mut self) {
        self.rebuild_container_file();
    }

    /// Packages whose Containerfile lines should be highlighted: the package under the
    /// cursor (when selected) plus everything it requires.
    pub fn container_highlight(&self) -> ContainerHighlight {
        let Some(package) = self.package_under_cursor() else {
            return ContainerHighlight::default();
        };
        if !self.selected_packages.contains(&package.name) {
            return ContainerHighlight::default();
        }

        let mut dependencies = BTreeSet::new();
        self.collect_requirements(&package.name, &mut dependencies);
        dependencies.remove(&package.name);

        ContainerHighlight {
            primary: Some(package.name.clone()),
            dependencies,
        }
    }

    fn collect_requirements(&self, name: &str, out: &mut BTreeSet<String>) {
        let Some(package) = self.catalog.get(name) else {
            return;
        };
        for requirement in &package.requires {
            if out.insert(requirement.clone()) {
                self.collect_requirements(requirement, out);
            }
        }
    }

    // --- package info popup ------------------------------------------------

    pub fn toggle_info(&mut self) {
        self.info_open = !self.info_open;
        if self.info_open {
            self.info_effect = Some(fx::fade_from(
                Color::Black,
                Color::Black,
                EffectTimer::from_ms(220, Interpolation::QuadOut),
            ));
        }
    }

    pub fn close_info(&mut self) -> bool {
        if self.info_open {
            self.info_open = false;
            self.info_effect = None;
            return true;
        }
        false
    }

    // --- input overlay -----------------------------------------------------

    /// Ask for an image reference and the family to treat it as.
    pub fn open_image_prompt(&mut self) {
        self.prompt = Some(Prompt::image());
    }

    pub fn open_script_prompt(&mut self) {
        let Some(package) = self.package_under_cursor().map(|p| p.name.clone()) else {
            return;
        };
        let current = self
            .overrides
            .get(&package)
            .map(|over| over.value.clone())
            .or_else(|| match self.resolved_spec(&package) {
                Some(InstallSpec::Script { script }) => Some(script),
                _ => None,
            })
            .unwrap_or_default();

        self.prompt = Some(Prompt::script(&package, &current));
    }

    /// Ask for the name of the toolbox the next build should create (`n` in the Builder).
    pub fn open_pending_toolbox_prompt(&mut self) {
        let Some(image) = self.builder.image_reference() else {
            self.append_log("Select a distro first, then name the toolbox.".to_string());
            return;
        };
        self.prompt = Some(Prompt::pending_toolbox(&image));
    }

    /// Ask which version of the package under the cursor to install (`v` in the popup).
    ///
    /// Only packages whose install command takes a version have anything to ask about.
    pub fn open_version_prompt(&mut self) {
        let Some(package) = self.package_under_cursor().map(|p| p.name.clone()) else {
            return;
        };
        let versioned = matches!(
            self.resolved_spec(&package),
            Some(InstallSpec::Script { script }) if script.contains(catalog::VERSION_PLACEHOLDER)
        );
        if !versioned {
            self.append_log(format!(
                "{package}: this install has no selectable version (it uses the package manager)."
            ));
            return;
        }

        let current = self.effective_version(&package);
        self.prompt = Some(Prompt::version(&package, &current));
    }

    /// Remember the version to install `package` with.
    pub fn set_version(&mut self, package: &str, version: &str) {
        let version = version.trim();
        if version.is_empty() || version.contains(char::is_whitespace) {
            self.append_log(format!("{version:?} is not a usable version"));
            return;
        }
        self.versions
            .insert(package.to_string(), version.to_string());
        self.append_log(format!("{package}: version {version}"));
        self.rebuild_container_file();
        self.persist_state();
    }

    pub fn prompt_is_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn prompt_insert(&mut self, character: char) {
        if let Some(prompt) = &mut self.prompt {
            // The suggestion is selected by default: typing replaces it.
            if prompt.pristine {
                prompt.input.clear();
                prompt.pristine = false;
            }
            prompt.input.push(character);
        }
        self.apply_prompt_filter();
    }

    pub fn prompt_backspace(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.pristine = false;
            prompt.input.pop();
        }
        self.apply_prompt_filter();
    }

    /// Ctrl+U: empty the field.
    pub fn prompt_clear(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.input.clear();
            prompt.pristine = false;
        }
        self.apply_prompt_filter();
    }

    /// Searching filters as you type; every other prompt only applies on submit.
    fn apply_prompt_filter(&mut self) {
        let Some(input) = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.kind == PromptKind::Filter)
            .map(|prompt| prompt.input.clone())
        else {
            return;
        };
        self.set_filter(&input);
    }

    pub fn prompt_cycle_family(&mut self, forward: bool) {
        if let Some(prompt) = &mut self.prompt {
            prompt.cycle_family(forward);
        }
    }

    pub fn prompt_cancel(&mut self) {
        // A cancelled search puts the previous filter back.
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.kind == PromptKind::Filter)
        {
            self.cancel_filter_prompt();
        }
        self.prompt = None;
    }

    /// Apply the overlay input: add an image, set an override, or name a toolbox.
    pub fn prompt_submit(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let input = prompt.input.trim().to_string();
        if input.is_empty() && prompt.kind != PromptKind::User {
            self.append_log("Nothing entered — cancelled.".to_string());
            return;
        }

        match prompt.kind {
            PromptKind::Image { family } => self.add_custom_distro(&input, family),
            PromptKind::Script { package } => {
                self.overrides.insert(
                    package.clone(),
                    InstallOverride {
                        method: InstallMethod::Script,
                        value: input,
                    },
                );
                self.append_log(format!("{package}: install command overridden"));
                self.rebuild_container_file();
                self.persist_state();
            }
            PromptKind::Version { package } => self.set_version(&package, &input),
            PromptKind::Env => self.set_env(&input),
            PromptKind::Filter => {
                self.filter_before_prompt = None;
                self.set_filter(&input);
                self.append_log(match self.filter_active() {
                    true => format!("filter: {input}"),
                    false => "filter cleared".to_string(),
                });
            }
            PromptKind::User => self.set_username(&input),
            PromptKind::Toolbox { image, .. } => self.create_toolbox_named(&image, &input),
            PromptKind::PendingToolbox { .. } => self.set_pending_toolbox(&input),
        }
    }

    /// Remember the name to use once the next build succeeds.
    pub fn set_pending_toolbox(&mut self, name: &str) {
        let name = name.trim();
        if !toolbox::is_valid_container_name(name) {
            self.append_log(format!(
                "{name:?} is not a valid toolbox name (letters, digits, dot, dash, underscore)"
            ));
            return;
        }
        self.pending_toolbox = Some(name.to_string());
        self.append_log(format!("Build to create toolbox {name}."));
    }

    // --- toolboxes ---------------------------------------------------------

    /// Re-read manifests and their image state in the background.
    pub fn refresh_toolboxes(&mut self) {
        if self
            .toolbox_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        self.append_log("Scanning toolboxes...".to_string());
        let tx = self.toolbox_tx.clone();
        self.toolbox_handle = Some(runtime.spawn(async move {
            let scan = toolbox::scan().await;
            let _ = tx.send(scan).await;
        }));
    }

    pub fn apply_toolbox_scan(&mut self, scan: ToolboxScan) {
        for warning in &scan.warnings {
            self.append_log(format!("warning: {warning}"));
        }

        // Keep the cursor on the same image across refreshes.
        let selected_image = self.selected_toolbox().map(|toolbox| toolbox.image.clone());

        let summary = summarize(&scan.toolboxes);
        self.append_log(format!("Toolboxes: {summary}"));

        self.toolboxes = scan.toolboxes;
        self.toolbox_state.select(if self.toolboxes.is_empty() {
            None
        } else {
            selected_image
                .and_then(|image| {
                    self.toolboxes
                        .iter()
                        .position(|toolbox| toolbox.image == image)
                })
                .or(Some(0))
        });
        self.toolbox_scroll = 0;
    }

    pub fn selected_toolbox(&self) -> Option<&Toolbox> {
        self.toolbox_state
            .selected()
            .and_then(|index| self.toolboxes.get(index))
    }

    /// Delete the manifest of the selected toolbox (the image itself is left alone).
    pub fn forget_selected_toolbox(&mut self) {
        let Some(toolbox) = self.selected_toolbox() else {
            return;
        };
        let Some(manifest) = &toolbox.manifest else {
            self.append_log(format!(
                "{} was not built by toolshop, so there is no manifest to forget.",
                toolbox.image
            ));
            return;
        };

        let image = manifest.image.clone();
        let containers = toolbox.containers.clone();
        let status = toolbox.status.without_manifest();

        match manifest::forget_in(&self.cache_dir, &image) {
            Ok(()) => {
                self.append_log(format!("Forget {image}: manifest removed"));
                self.toolboxes.retain(|toolbox| toolbox.image != image);

                // Containers outlive the manifest, so the image stays listed as external.
                if !containers.is_empty() {
                    self.toolboxes
                        .push(Toolbox::external(image, status, containers));
                    toolbox::sort_toolboxes(&mut self.toolboxes);
                }

                let index = self.toolbox_state.selected().unwrap_or(0);
                self.toolbox_state.select(
                    (!self.toolboxes.is_empty()).then(|| index.min(self.toolboxes.len() - 1)),
                );
            }
            Err(err) => self.append_log(format!("Cannot remove manifest for {image}: {err:#}")),
        }
    }

    /// Message appended after a toolbox was created for `image`: a warning when the image
    /// ships GPU tools but no Vulkan driver manifest (they would fail at start-up).
    fn toolbox_hint(&self, image: &str) -> Option<String> {
        let packages: Vec<&str> = self
            .toolboxes
            .iter()
            .find(|toolbox| toolbox.image == image)
            .and_then(|toolbox| toolbox.manifest.as_ref())
            .map(|manifest| {
                manifest
                    .packages
                    .iter()
                    .map(|package| package.name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        catalog::vulkan_hint(&packages)
    }

    /// Run `toolbox create` for `image` under `name` in the background.
    pub fn create_toolbox_named(&mut self, image: &str, name: &str) {
        let name = name.trim();
        if !toolbox::is_valid_container_name(name) {
            self.append_log(format!(
                "{name:?} is not a valid toolbox name (letters, digits, dot, dash, underscore)"
            ));
            return;
        }

        self.append_log(format!("Creating toolbox {name} from {image}..."));

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let image = image.to_string();
        let name = name.to_string();
        let hint = self.toolbox_hint(&image);
        let tx = self.log_tx.clone();
        self.toolbox_action = Some(runtime.spawn(async move {
            let message = match toolbox::create_container(&image, Some(&name)).await {
                Ok(message) => message,
                Err(err) => format!("toolbox create failed: {err:#}"),
            };
            let _ = tx.send(message).await;
            if let Some(hint) = hint {
                let _ = tx.send(hint).await;
            }
        }));
    }

    /// Ask for the container name, then create a toolbox from the selected image.
    pub fn create_toolbox(&mut self) {
        let Some(image) = self.selected_toolbox().map(|toolbox| toolbox.image.clone()) else {
            self.append_log("No toolbox selected.".to_string());
            return;
        };
        if self.toolbox_action.is_some() {
            self.append_log("A toolbox action is still running.".to_string());
            return;
        }
        self.prompt = Some(Prompt::toolbox(&image));
    }

    /// Remove every toolbox container created from the selected image.
    pub fn remove_toolboxes(&mut self) {
        let Some(toolbox) = self.selected_toolbox() else {
            self.append_log("No toolbox selected.".to_string());
            return;
        };
        if toolbox.containers.is_empty() {
            self.append_log(format!("No toolbox containers for {}.", toolbox.image));
            return;
        }
        if self.toolbox_action.is_some() {
            self.append_log("A toolbox action is still running.".to_string());
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let containers = toolbox.containers.clone();
        let tx = self.log_tx.clone();
        self.append_log(format!(
            "Removing {} toolbox container(s)...",
            containers.len()
        ));
        self.toolbox_action = Some(runtime.spawn(async move {
            for name in containers {
                let message = match toolbox::remove_container(&name).await {
                    Ok(message) => message,
                    Err(err) => format!("cannot remove {name}: {err:#}"),
                };
                if tx.send(message).await.is_err() {
                    break;
                }
            }
        }));
    }

    pub fn scroll_toolbox_details(&mut self, forward: bool) {
        self.toolbox_scroll = if forward {
            self.toolbox_scroll.saturating_add(1)
        } else {
            self.toolbox_scroll.saturating_sub(1)
        };
    }

    fn move_toolbox_cursor(&mut self, forward: bool) {
        if self.toolboxes.is_empty() {
            self.toolbox_state.select(None);
            return;
        }
        let index = self.toolbox_state.selected().unwrap_or(0);
        let next = if forward {
            (index + 1).min(self.toolboxes.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        if next != index {
            self.toolbox_scroll = 0;
        }
        self.toolbox_state.select(Some(next));
    }

    // --- logs --------------------------------------------------------------

    pub fn append_log(&mut self, log: String) {
        self.build.observe(&log);

        // A failed build should be impossible to miss: show the log, at the failure.
        if log.trim_start().starts_with("Build failed") {
            self.log_scroll = 0;
            self.switch_view(View::Logs);
        }

        if self.logs.len() >= MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs
            .push_back(log.trim_end_matches(['\r', '\n']).to_string());

        // While the user is scrolled back, new output must not shift the lines they are
        // reading: follow the content instead of the tail.
        if self.log_scroll > 0 {
            self.log_scroll += 1;
        }
        self.log_scroll = self.log_scroll.min(self.max_log_scroll());
    }

    /// Largest useful scroll offset for the current content and viewport.
    pub fn max_log_scroll(&self) -> usize {
        self.logs.len().saturating_sub(self.log_view_height)
    }

    pub fn scroll_logs_up(&mut self) {
        self.log_scroll = (self.log_scroll + 1).min(self.max_log_scroll());
    }

    pub fn scroll_logs_down(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(1);
    }

    // --- focus and navigation ----------------------------------------------

    /// Move to the next pane in layout order: options → distro → packages → Containerfile.
    pub fn focus_right(&mut self) {
        self.focus = Focus::from_index(self.focus.index() + 1);
    }

    /// Move to the previous pane in layout order (the distro list goes back to the options).
    pub fn focus_left(&mut self) {
        self.focus = Focus::from_index(self.focus.index() + Focus::ALL.len() - 1);
    }

    pub fn move_up(&mut self) {
        if self.view == View::Toolboxes {
            self.move_toolbox_cursor(false);
            return;
        }
        if self.view == View::Logs {
            self.scroll_logs_up();
            return;
        }
        match self.focus {
            Focus::Distro => self.step_distro_cursor(false),
            Focus::Options => self.options_cursor = self.options_cursor.saturating_sub(1),
            Focus::Apps => self.step_package_cursor(false),
            Focus::Container => self.scroll_container(false),
        }
    }

    pub fn move_down(&mut self) {
        if self.view == View::Toolboxes {
            self.move_toolbox_cursor(true);
            return;
        }
        if self.view == View::Logs {
            self.scroll_logs_down();
            return;
        }
        match self.focus {
            Focus::Distro => self.step_distro_cursor(true),
            Focus::Options => {
                self.options_cursor = (self.options_cursor + 1).min(Self::OPTIONS_ROWS - 1)
            }
            Focus::Apps => self.step_package_cursor(true),
            Focus::Container => self.scroll_container(true),
        }
    }

    /// Move the package cursor, skipping category headings.
    fn step_package_cursor(&mut self, forward: bool) {
        let Some(current) = self.apps_list_state.selected() else {
            let first = self
                .package_rows
                .iter()
                .position(|row| !row.is_header())
                .unwrap_or(0);
            self.apps_list_state.select(Some(first));
            return;
        };

        if self.package_rows.is_empty() {
            self.apps_list_state.select(None);
            return;
        }

        let next = if forward {
            (current + 1).min(self.package_rows.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        self.apps_list_state.select(Some(next));
    }

    pub fn toggle_option(&mut self) {
        if self.view != View::Builder {
            return;
        }
        match self.focus {
            Focus::Distro => self.select_distro_under_cursor(),
            Focus::Options => self.toggle_option_row(self.options_cursor),
            Focus::Apps => match self.package_header_under_cursor() {
                Some(category) => self.toggle_category(&category),
                None => self.toggle_package_under_cursor(),
            },
            Focus::Container => {}
        }
    }

    /// Activate an options row: checkboxes flip, the last row asks for a user name.
    /// Rows of the options block: the build checkboxes, the user name and the toolbox name.
    pub fn options_rows(&self) -> Vec<OptionRow> {
        let mut rows: Vec<OptionRow> = self
            .options
            .toggles()
            .iter()
            .map(|(label, enabled)| OptionRow {
                label: format!("[{}] {label}", if *enabled { "x" } else { " " }),
                enabled: *enabled,
                editable: false,
            })
            .collect();

        rows.push(OptionRow {
            label: self
                .options
                .user_row(self.builder.docker_user_name(), &self.recorded_user()),
            enabled: !self.options.hide_username,
            editable: true,
        });
        rows.push(OptionRow {
            label: match &self.pending_toolbox {
                Some(name) => format!("toolbox: {name}"),
                None => "toolbox: (auto)".to_string(),
            },
            enabled: self.pending_toolbox.is_some(),
            editable: true,
        });
        rows.push(OptionRow {
            label: format!(
                "[{}] local images",
                if self.show_local_images { "x" } else { " " }
            ),
            enabled: self.show_local_images,
            editable: false,
        });
        rows.push(OptionRow {
            label: match self.options.env.is_empty() {
                true => "env: (none)".to_string(),
                false => format!(
                    "env: {}",
                    self.options
                        .env
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            },
            enabled: !self.options.env.is_empty(),
            editable: true,
        });
        rows.push(OptionRow {
            label: "reset last configuration".to_string(),
            enabled: false,
            editable: true,
        });
        rows
    }

    /// Activate an options row: checkboxes flip, value rows open a prompt.
    pub fn toggle_option_row(&mut self, index: usize) {
        if index >= Self::OPTIONS_ROWS {
            return;
        }

        match index {
            Self::TOOLBOX_ROW => self.open_pending_toolbox_prompt(),
            Self::LOCAL_IMAGES_ROW => self.toggle_local_images(),
            Self::ENV_ROW => self.open_env_prompt(),
            Self::RESET_ROW => self.reset_saved_state(),
            BuildOptions::USER_ROW => self.open_user_prompt(),
            _ => {
                self.options.toggle(index);
                let (label, enabled) = self
                    .options
                    .toggles()
                    .get(index)
                    .copied()
                    .unwrap_or(("option", false));
                self.append_log(format!("{label}: {}", if enabled { "on" } else { "off" }));
                self.rebuild_container_file();
                self.persist_state();
            }
        }
    }

    /// Ask which name to record in `ARG DOCKER_USER` (empty resets to the host user).
    pub fn open_user_prompt(&mut self) {
        let current = self.recorded_user();
        self.prompt = Some(Prompt::user(&current));
    }

    /// Record `name` in the Containerfile instead of the host account.
    ///
    /// An empty name clears the override, so the host user comes back.
    pub fn set_username(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.options.username = None;
            self.options.hide_username = false;
            self.append_log(format!("user: {} (host account)", self.recorded_user()));
        } else if name.contains(char::is_whitespace) {
            self.append_log(format!("{name:?} is not a usable user name"));
            return;
        } else {
            self.options.username = Some(name.to_string());
            self.options.hide_username = false;
            self.append_log(format!("user: {name} recorded in the Containerfile"));
        }
        self.rebuild_container_file();
    }

    /// The name that ends up in `ARG DOCKER_USER`.
    pub fn recorded_user(&self) -> String {
        self.builder.recorded_user_for(&self.options)
    }

    /// Scroll the Containerfile pane (keyboard, when it has focus).
    pub fn scroll_container(&mut self, down: bool) {
        if down {
            self.container_scroll = self.container_scroll.saturating_add(1);
        } else {
            self.container_scroll = self.container_scroll.saturating_sub(1);
        }
    }

    // --- mouse -------------------------------------------------------------

    /// Route a mouse event to the pane under the pointer.
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        // Overlays are modal: while a prompt is open the mouse is ignored entirely.
        if self.prompt_is_open() {
            return;
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(event.column, event.row),
            MouseEventKind::ScrollUp => self.wheel(event.column, event.row, false),
            MouseEventKind::ScrollDown => self.wheel(event.column, event.row, true),
            _ => {}
        }
    }

    fn click(&mut self, column: u16, row: u16) {
        if let Some(view) = self.hit.tab_at(column, row) {
            self.switch_view(view);
            return;
        }

        // Clicking outside the info popup dismisses it; clicks inside do nothing.
        if self.info_open {
            if !contains(self.hit.info_popup, column, row) {
                self.close_info();
            }
            return;
        }

        match self.view {
            View::Builder => {
                if contains(self.hit.distros, column, row) {
                    self.focus = Focus::Distro;
                    let index = list_index(
                        self.hit.distros,
                        &self.distro_list_state,
                        self.distro_rows.len(),
                        column,
                        row,
                    );
                    if let Some(index) = index {
                        if self.distro_list_state.selected() == Some(index) {
                            self.select_distro_under_cursor();
                        } else {
                            self.distro_list_state.select(Some(index));
                        }
                    }
                } else if contains(self.hit.packages, column, row) {
                    self.focus = Focus::Apps;
                    let index = list_index(
                        self.hit.packages,
                        &self.apps_list_state,
                        self.package_rows.len(),
                        column,
                        row,
                    );
                    if let Some(index) = index {
                        if let PackageRow::Header { label, .. } = self.package_rows[index].clone() {
                            self.apps_list_state.select(Some(index));
                            self.toggle_category(&label);
                            return;
                        }
                        if self.apps_list_state.selected() == Some(index) {
                            self.toggle_package_under_cursor();
                        } else {
                            self.apps_list_state.select(Some(index));
                        }
                    }
                } else if contains(self.hit.options, column, row) {
                    self.focus = Focus::Options;
                    // Rows start under the top border.
                    let index = row.saturating_sub(self.hit.options.y + 1) as usize;
                    if index < Self::OPTIONS_ROWS {
                        self.options_cursor = index;
                        self.toggle_option_row(index);
                    }
                } else if contains(self.hit.containerfile, column, row) {
                    self.focus = Focus::Container;
                }
            }
            View::Toolboxes => {
                if contains(self.hit.toolboxes, column, row) {
                    let index = list_index(
                        self.hit.toolboxes,
                        &self.toolbox_state,
                        self.toolboxes.len(),
                        column,
                        row,
                    );
                    if let Some(index) = index {
                        if self.toolbox_state.selected() != Some(index) {
                            self.toolbox_scroll = 0;
                        }
                        self.toolbox_state.select(Some(index));
                    }
                }
            }
            View::Logs => {}
        }
    }

    fn wheel(&mut self, column: u16, row: u16, down: bool) {
        if self.info_open {
            return;
        }

        match self.view {
            View::Logs => {
                if down {
                    self.scroll_logs_down();
                } else {
                    self.scroll_logs_up();
                }
            }
            View::Toolboxes => {
                if contains(self.hit.toolbox_details, column, row) {
                    if down {
                        self.toolbox_scroll = self.toolbox_scroll.saturating_add(1);
                    } else {
                        self.toolbox_scroll = self.toolbox_scroll.saturating_sub(1);
                    }
                } else if down {
                    self.move_toolbox_cursor(true);
                } else {
                    self.move_toolbox_cursor(false);
                }
            }
            View::Builder => {
                if contains(self.hit.containerfile, column, row) {
                    for _ in 0..3 {
                        self.scroll_container(down);
                    }
                } else if contains(self.hit.packages, column, row) {
                    self.step_package_cursor(down);
                } else if contains(self.hit.distros, column, row) {
                    self.step_distro_cursor(down);
                } else if contains(self.hit.options, column, row) {
                    self.focus = Focus::Options;
                    if down {
                        self.options_cursor = (self.options_cursor + 1).min(Self::OPTIONS_ROWS - 1);
                    } else {
                        self.options_cursor = self.options_cursor.saturating_sub(1);
                    }
                }
            }
        }
    }

    // --- build -------------------------------------------------------------

    pub fn run_build(&mut self) {
        if self.view == View::Toolboxes {
            return;
        }
        if self.process_running {
            self.append_log("Cancelling build...".to_string());
            self.token.cancel();
            return;
        }

        if self.builder.get_container_file().trim().is_empty() {
            self.append_log(
                "Nothing to build: select a distro and at least one package first.".to_string(),
            );
            return;
        }

        let Some(manifest) = self.manifest_for_selection() else {
            self.append_log("Cannot describe the build inputs (no distro selected).".to_string());
            return;
        };

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.append_log("No async runtime; cannot start the build.".to_string());
            return;
        };

        self.token = CancellationToken::new();
        self.build.start();
        let token = self.token.clone();
        let tx = self.log_tx.clone();
        let builder = self.builder.clone();
        let pending_toolbox = self.pending_toolbox.clone();

        let build_ok = Arc::new(AtomicBool::new(false));
        self.build_ok = Some(build_ok.clone());

        self.build_handle = Some(runtime.spawn(async move {
            match builder.run_build(tx.clone(), token).await {
                Ok(Some(outcome)) => {
                    build_ok.store(true, Ordering::Relaxed);
                    let manifest = Manifest {
                        image: outcome.image.clone(),
                        image_id: outcome.image_id,
                        ..manifest
                    };
                    match manifest::save(&manifest) {
                        Ok(path) => {
                            let _ = tx
                                .send(format!("Manifest saved to {}", path.display()))
                                .await;
                        }
                        Err(err) => {
                            let _ = tx
                                .send(format!("warning: cannot save the manifest: {err:#}"))
                                .await;
                        }
                    }

                    if let Some(name) = pending_toolbox {
                        let message =
                            match toolbox::create_container(&outcome.image, Some(&name)).await {
                                Ok(message) => message,
                                Err(err) => format!("toolbox create failed: {err:#}"),
                            };
                        let _ = tx.send(message).await;
                        // The package list is known here, so a missing Vulkan manifest can be
                        // pointed out before the container is ever entered.
                        let packages: Vec<&str> =
                            manifest.packages.iter().map(|p| p.name.as_str()).collect();
                        if let Some(hint) = catalog::vulkan_hint(&packages) {
                            let _ = tx.send(hint).await;
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.send(format!("Build failed: {err:#}")).await;
                }
            }
        }));
        self.process_running = true;
    }

    /// Manifest describing what the current selection would install.
    fn manifest_for_selection(&self) -> Option<Manifest> {
        let distro = self.builder.distro()?;
        let image = self.builder.image_reference()?;
        let packages: Vec<Package> = self
            .catalog
            .packages
            .iter()
            .filter(|package| self.selected_packages.contains(&package.name))
            .cloned()
            .collect();
        Some(Manifest::from_selection_with(
            distro,
            &packages,
            &self.overrides,
            &self.versions,
            image,
        ))
    }
}

/// State file to use: the real one, except under `cargo test`, where the saved configuration
/// belongs to the developer and must not be touched.
fn default_state_file() -> PathBuf {
    if cfg!(test) {
        std::env::temp_dir().join(format!(
            "toolshop-test-state-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ))
    } else {
        crate::state::path()
    }
}

/// True when `needle` (lowercase) appears in the package's name, category or summary.
fn package_matches(package: &Package, needle: &str) -> bool {
    package.name.to_lowercase().contains(needle)
        || catalog::category_of(package)
            .to_lowercase()
            .contains(needle)
        || package.summary.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::{DEFAULT_INSTALL_KEY, InstallSpec},
        manifest::ManifestDistro,
        toolbox::{ToolboxScan, ToolboxStatus},
    };
    use ratatui::crossterm::event::KeyModifiers;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    /// Catalog where every package simply installs itself, with `requires` links.
    fn catalog_with(entries: &[(&str, &[&str])]) -> Catalog {
        let packages = entries
            .iter()
            .map(|(name, requires)| Package {
                name: (*name).to_string(),
                version: String::new(),
                summary: format!("{name} summary"),
                category: "Test".to_string(),
                requires: requires.iter().map(|req| (*req).to_string()).collect(),
                install: BTreeMap::from([(
                    DEFAULT_INSTALL_KEY.to_string(),
                    InstallSpec::PackageManager {
                        pkg: (*name).to_string(),
                    },
                )]),
                env: BTreeMap::new(),
            })
            .collect();
        Catalog {
            packages,
            warnings: vec![],
        }
    }

    fn categorised(entries: &[(&str, &str)]) -> Catalog {
        let mut catalog = catalog_with(
            &entries
                .iter()
                .map(|(name, _)| (*name, &[] as &[&str]))
                .collect::<Vec<_>>(),
        );
        for (package, (_, category)) in catalog.packages.iter_mut().zip(entries) {
            package.category = (*category).to_string();
        }
        catalog
    }

    fn app(catalog: Catalog) -> App {
        let (log_tx, _log_rx) = mpsc::channel(64);
        let (distro_tx, _distro_rx) = mpsc::channel(16);
        let (toolbox_tx, _toolbox_rx) = mpsc::channel(16);
        let mut app = App::new(catalog, Distro::predefined(), log_tx, distro_tx, toolbox_tx);
        // Keep tests away from the developer's config/cache directories.
        let dir = std::env::temp_dir().join(format!(
            "toolshop-app-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        app.images_file = dir.join("images.toml");
        app.theme_file = dir.join("config.toml");
        app.cache_dir = dir.join("manifests");
        app.animation = false;
        app
    }

    #[test]
    fn creating_a_toolbox_warns_when_the_image_lacks_the_vulkan_icd() {
        let mut app = app(catalog_with(&[
            ("isaac-sim", &[]),
            ("nvidia-vulkan-icd", &[]),
        ]));
        let manifest = |names: &[&str]| Manifest {
            version: crate::manifest::MANIFEST_VERSION,
            image: "localhost/toolshop:jazzy-1a2b3c".to_string(),
            image_id: None,
            distro: ManifestDistro {
                name: "jazzy".to_string(),
                image: "docker.io/library/ros:jazzy-ros-base".to_string(),
                family: "ros".to_string(),
            },
            packages: names
                .iter()
                .map(|name| crate::manifest::ManifestPackage {
                    name: (*name).to_string(),
                    version: String::new(),
                    pkg: Some((*name).to_string()),
                    script: None,
                })
                .collect(),
            created_at: 0,
            tool_version: "0.1.0".to_string(),
        };

        app.toolboxes = vec![Toolbox::built(
            manifest(&["uv", "isaac-sim"]),
            ToolboxStatus::Present,
            Vec::new(),
        )];
        let image = app.toolboxes[0].image.clone();
        let hint = app.toolbox_hint(&image).expect("GPU tools without the ICD");
        assert!(hint.contains("ERROR_INCOMPATIBLE_DRIVER"), "{hint}");

        // The same image with the ICD package selected needs no warning.
        app.toolboxes = vec![Toolbox::built(
            manifest(&["uv", "isaac-sim", "nvidia-vulkan-icd"]),
            ToolboxStatus::Present,
            Vec::new(),
        )];
        assert!(app.toolbox_hint(&image).is_none());

        // External images carry no package list, so there is nothing to say.
        app.toolboxes = vec![Toolbox::external(
            image.clone(),
            ToolboxStatus::Present,
            vec!["dev".to_string()],
        )];
        assert!(app.toolbox_hint(&image).is_none());
    }

    /// App with the real bundled catalog, so recipes like neovim's are the ones users get.
    fn app_with_catalog() -> App {
        let (log_tx, _log_rx) = mpsc::channel(64);
        let (distro_tx, _distro_rx) = mpsc::channel(16);
        let (toolbox_tx, _toolbox_rx) = mpsc::channel(16);
        let catalog = Catalog::load().expect("bundled catalog");
        let mut app = App::new(catalog, Distro::predefined(), log_tx, distro_tx, toolbox_tx);
        app.animation = false;
        app
    }

    fn press_key(app: &mut App, code: ratatui::crossterm::event::KeyCode) {
        crate::update::update(
            app,
            ratatui::crossterm::event::KeyEvent::new(
                code,
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        );
    }

    /// Ctrl+U inside the input overlay.
    fn clear_prompt(app: &mut App) {
        crate::update::update(
            app,
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('u'),
                ratatui::crossterm::event::KeyModifiers::CONTROL,
            ),
        );
    }

    #[test]
    fn the_current_configuration_is_saved_and_restored() {
        let mut app = app_with_catalog();
        select_distro(&mut app, "24.04");
        select_package(&mut app, "neovim");
        app.toggle_option();
        assert!(app.selected_packages.contains("neovim"));
        app.set_version("neovim", "0.11.2");
        app.set_filter("rust");

        let saved = app.saved_state();
        assert_eq!(
            saved.distro.as_deref(),
            Some("docker.io/library/ubuntu:24.04")
        );
        assert!(saved.packages.contains("neovim"));
        assert!(
            saved.packages.contains("wget"),
            "requirements are saved too"
        );
        assert_eq!(
            saved.versions.get("neovim").map(String::as_str),
            Some("0.11.2")
        );

        // A fresh app that gets the state back ends up where the first one was.
        let mut restored = app_with_catalog();
        restored.restore_state(&saved);
        assert_eq!(restored.selected_packages, app.selected_packages);
        assert_eq!(restored.versions, app.versions);
        assert_eq!(
            restored
                .selected_distro
                .map(|index| restored.distros[index].image.clone()),
            saved.distro
        );
        let file = restored.builder.get_container_file();
        assert!(file.contains("ubuntu:24.04"), "{file}");
        assert!(file.contains("download/v0.11.2"), "{file}");
        // The filter is a transient view, not part of the saved configuration.
        assert!(restored.filter.is_empty());
    }

    #[test]
    fn restoring_drops_packages_and_versions_the_catalog_no_longer_has() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        let saved = SavedState {
            distro: Some("docker.io/library/ghost:latest".to_string()),
            packages: BTreeSet::from(["htop".to_string(), "gone".to_string()]),
            versions: BTreeMap::from([("gone".to_string(), "9.9.9".to_string())]),
            ..SavedState::default()
        };

        app.restore_state(&saved);
        // Nothing matched, so the app stays usable and says what it dropped.
        assert_eq!(app.selected_packages, BTreeSet::from(["htop".to_string()]));
        assert!(app.versions.is_empty());
        assert_eq!(app.selected_distro, None);
        let log = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(
            log.contains("dropped from the saved configuration"),
            "{log}"
        );
        assert!(log.contains("gone"), "{log}");
        assert!(log.contains("ghost:latest"), "{log}");
    }

    #[test]
    fn the_configuration_is_written_once_per_change() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        let path = std::env::temp_dir().join(format!(
            "toolshop-save-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        app.state_file = path.clone();

        select_distro(&mut app, "jazzy");
        assert!(path.exists(), "selecting a distro saves the state");
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("jazzy"), "{first}");

        // An unchanged configuration is not rewritten.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        app.persist_state();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );

        // A change is.
        select_package(&mut app, "htop");
        app.toggle_option();
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(second.contains("htop"), "{second}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_saved_file_restores_into_a_fresh_app() {
        let path = std::env::temp_dir().join(format!(
            "toolshop-restart-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        // First run: pick a distro, two packages, a version.
        let first = {
            let mut app = app_with_catalog();
            app.state_file = path.clone();
            select_distro(&mut app, "24.04");
            select_package(&mut app, "neovim");
            app.toggle_option();
            select_package(&mut app, "htop");
            app.toggle_option();
            app.set_version("neovim", "0.11.2");
            app.saved_state()
        };
        assert!(path.exists(), "the run saved itself");

        // Second run: exactly what `main` does.
        let (saved, warnings) = crate::state::load_from(&path);
        assert!(warnings.is_empty(), "{warnings:?}");
        let saved = saved.expect("state file parsed");
        assert_eq!(saved, first);

        let mut second = app_with_catalog();
        second.state_file = path.clone();
        second.restore_state(&saved);
        assert!(second.selected_packages.contains("neovim"));
        assert!(second.selected_packages.contains("htop"));
        assert_eq!(
            second.versions.get("neovim").map(String::as_str),
            Some("0.11.2")
        );
        let file = second.builder.get_container_file();
        assert!(file.contains("ubuntu:24.04"), "{file}");
        assert!(file.contains("download/v0.11.2"), "{file}");
        assert!(file.contains("htop"), "{file}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extra_environment_variables_can_be_added_replaced_and_removed() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "jazzy");
        assert_eq!(app.options_rows()[App::ENV_ROW].label, "env: (none)");

        app.focus = Focus::Options;
        app.options_cursor = App::ENV_ROW;
        app.toggle_option();
        assert!(app.prompt_is_open(), "the row opens a prompt");

        app.set_env("FOO=bar");
        let file = app.builder.get_container_file();
        assert!(file.contains("ENV FOO=bar"), "{file}");
        assert_eq!(app.options_rows()[App::ENV_ROW].label, "env: FOO");

        // Values with spaces are quoted; a second variable joins the first.
        app.set_env("GREETING=hello there");
        assert!(
            app.builder
                .get_container_file()
                .contains("ENV GREETING=\"hello there\"")
        );
        assert_eq!(app.options_rows()[App::ENV_ROW].label, "env: FOO,GREETING");

        // Same name replaces instead of duplicating.
        app.set_env("FOO=baz");
        let file = app.builder.get_container_file();
        assert_eq!(file.matches("ENV FOO=").count(), 1, "{file}");
        assert!(file.contains("ENV FOO=baz"), "{file}");

        // A bare name removes it, and the Containerfile follows.
        app.set_env("FOO");
        let file = app.builder.get_container_file();
        assert!(!file.contains("ENV FOO="), "{file}");
        assert!(file.contains("ENV GREETING="), "{file}");

        // Bad input is rejected with a message and changes nothing.
        let before = app.options.env.clone();
        app.set_env("1BAD=x");
        assert_eq!(app.options.env, before);
        app.set_env("nonsense without equals");
        assert_eq!(app.options.env, before);
        app.set_env("CAT");
        assert_eq!(
            app.options.env, before,
            "removing something absent is a no-op"
        );
        let log = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(log.contains("not a usable variable name"), "{log}");
        assert!(log.contains("is not set"), "{log}");

        // `-` clears everything.
        app.set_env("-");
        assert!(app.options.env.is_empty());
        assert!(!app.builder.get_container_file().contains("ENV GREETING="));
        assert_eq!(app.options_rows()[App::ENV_ROW].label, "env: (none)");
    }

    #[test]
    fn extra_environment_variables_survive_a_restart() {
        let mut app = app_with_catalog();
        select_distro(&mut app, "24.04");
        app.set_env("CUDA_VISIBLE_DEVICES=0");

        let saved = app.saved_state();
        assert_eq!(
            saved
                .options
                .env
                .get("CUDA_VISIBLE_DEVICES")
                .map(String::as_str),
            Some("0")
        );

        let mut restored = app_with_catalog();
        restored.restore_state(&saved);
        assert!(
            restored
                .builder
                .get_container_file()
                .contains("ENV CUDA_VISIBLE_DEVICES=0")
        );
    }

    #[test]
    fn the_reset_row_clears_the_selection_and_deletes_the_file() {
        let mut app = app_with_catalog();
        let path = std::env::temp_dir().join(format!(
            "toolshop-reset-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        app.state_file = path.clone();

        select_distro(&mut app, "24.04");
        select_package(&mut app, "neovim");
        app.toggle_option();
        app.set_version("neovim", "0.11.2");
        app.set_filter("rust");
        app.toggle_category("Shell");
        assert!(path.exists());

        // The row is offered in the options block and Enter activates it.
        assert_eq!(app.options_rows().len(), App::OPTIONS_ROWS);
        assert!(app.options_rows()[App::RESET_ROW].label.contains("reset"));
        assert!(!app.options_rows()[App::RESET_ROW].enabled);
        app.focus = Focus::Options;
        app.options_cursor = App::RESET_ROW;
        app.toggle_option();

        assert!(!path.exists(), "the saved file is gone");
        assert!(app.selected_packages.is_empty());
        assert!(app.versions.is_empty());
        assert!(app.overrides.is_empty());
        assert!(app.collapsed_categories.is_empty());
        assert!(!app.filter_active());
        assert_eq!(app.selected_distro, None);
        assert_eq!(app.options, BuildOptions::default());
        assert!(!app.builder.get_container_file().contains("neovim"));
        let log = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(log.contains("last configuration reset"), "{log}");

        // Pressing it again is harmless.
        app.toggle_option();
        let log = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(log.contains("nothing saved to reset"), "{log}");
    }

    #[test]
    fn the_filter_narrows_the_package_list_by_name_category_and_summary() {
        let mut catalog = catalog_with(&[
            ("neovim", &[]),
            ("rust-analyzer", &[]),
            ("htop", &[]),
            ("cargo-nextest", &[]),
        ]);
        for package in &mut catalog.packages {
            package.summary = match package.name.as_str() {
                "neovim" => "modal text editor".to_string(),
                "htop" => "interactive process viewer".to_string(),
                _ => String::new(),
            };
            package.category =
                if package.name.starts_with("cargo") || package.name.starts_with("rust") {
                    "Rust".to_string()
                } else {
                    "Shell".to_string()
                };
        }
        let mut app = app(catalog);
        select_distro(&mut app, "jazzy");

        // By name ("neovim" contains "neov", not "nvim").
        app.set_filter("neov");
        assert_eq!(names(&app), vec!["neovim"]);
        // By category.
        app.set_filter("rust");
        assert_eq!(names(&app), vec!["rust-analyzer", "cargo-nextest"]);
        // By summary text.
        app.set_filter("process viewer");
        assert_eq!(names(&app), vec!["htop"]);
        // Case-insensitive, and categories with no match disappear entirely.
        app.set_filter("NEOV");
        assert_eq!(names(&app).len(), 1);
        // The Rust heading is gone: nothing in that category matches.
        assert!(!package_row_labels(&app).contains(&"Rust"));

        // Clearing shows everything again.
        app.clear_filter();
        assert!(!app.filter_active());
        assert_eq!(app.visible_packages.len(), 4);
    }

    #[test]
    fn the_filter_prompt_filters_while_typing_and_esc_restores() {
        use ratatui::crossterm::event::KeyCode;

        let mut app = app_with_catalog();
        select_distro(&mut app, "24.04");

        press_key(&mut app, KeyCode::Char('/'));
        assert!(app.prompt_is_open());
        for character in "neov".chars() {
            press_key(&mut app, KeyCode::Char(character));
        }
        // Filtering happens on every keystroke, before the prompt is confirmed.
        assert_eq!(app.filter, "neov");
        assert_eq!(
            names(&app)
                .iter()
                .filter(|name| name.starts_with("neovim"))
                .count(),
            1
        );
        assert!(app.visible_packages.len() < 10);

        // Esc throws the search away and restores the previous list.
        press_key(&mut app, KeyCode::Esc);
        assert!(!app.prompt_is_open());
        assert!(!app.filter_active());
        assert_eq!(app.visible_packages.len(), app.catalog.packages.len());

        // Enter keeps the filter, and Esc in the builder clears it (then quits).
        press_key(&mut app, KeyCode::Char('/'));
        for character in "rust".chars() {
            press_key(&mut app, KeyCode::Char(character));
        }
        press_key(&mut app, KeyCode::Enter);
        assert!(!app.prompt_is_open());
        assert!(app.filter_active(), "the filter survives the prompt");
        assert!(!app.should_quit);

        press_key(&mut app, KeyCode::Esc);
        assert!(!app.filter_active());
        assert!(!app.should_quit, "clearing the filter does not quit");
        press_key(&mut app, KeyCode::Esc);
        assert!(app.should_quit, "the second Esc quits");
    }

    #[test]
    fn reset_also_forgets_the_chosen_version() {
        let mut app = app_with_catalog();
        select_distro(&mut app, "24.04");
        select_package(&mut app, "neovim");
        app.toggle_option();

        app.set_version("neovim", "0.11.2");
        assert!(
            app.builder
                .get_container_file()
                .contains("download/v0.11.2")
        );
        assert!(app.package_info().unwrap().version_chosen);

        // `r` in the popup restores the catalog entry ...
        app.reset_install_override();
        assert!(!app.versions.contains_key("neovim"));
        assert!(!app.package_info().unwrap().version_chosen);
        let file = app.builder.get_container_file();
        assert!(file.contains("download/v0.12.1"), "{file}");
        assert!(!file.contains("v0.11.2"), "{file}");

        // ... and does nothing (no stray log line) when there is nothing to reset.
        let before = app.logs.len();
        app.reset_install_override();
        assert_eq!(app.logs.len(), before);
    }

    #[test]
    fn version_and_command_edits_from_the_info_popup_reach_the_containerfile() {
        use ratatui::crossterm::event::KeyCode;

        let mut app = app_with_catalog();
        select_distro(&mut app, "24.04");
        select_package(&mut app, "neovim");
        app.toggle_option();
        let before = app.builder.get_container_file();
        assert!(before.contains("download/v0.12.1"), "{before}");

        // `i` opens the popup, `v` asks for the version.
        press_key(&mut app, KeyCode::Char('i'));
        assert!(app.info_open);
        press_key(&mut app, KeyCode::Char('v'));
        assert!(app.prompt_is_open(), "the version prompt must open");
        for character in "0.11.2".chars() {
            press_key(&mut app, KeyCode::Char(character));
        }
        press_key(&mut app, KeyCode::Enter);

        let versioned = app.builder.get_container_file();
        assert!(versioned.contains("download/v0.11.2"), "{versioned}");
        assert_eq!(app.package_info().unwrap().version, "0.11.2");
        // The popup shows the command that will run, not the raw recipe.
        let info = app.package_info().unwrap();
        assert!(info.value.contains("download/v0.11.2"), "{}", info.value);
        assert!(
            !info.value.contains("{{version}}"),
            "placeholders are expanded: {}",
            info.value
        );

        // `e` replaces the command itself: Ctrl+U clears the pre-filled recipe first.
        press_key(&mut app, KeyCode::Char('e'));
        assert!(app.prompt_is_open(), "the edit prompt must open");
        clear_prompt(&mut app);
        for character in "echo custom-neovim".chars() {
            press_key(&mut app, KeyCode::Char(character));
        }
        press_key(&mut app, KeyCode::Enter);

        let edited = app.builder.get_container_file();
        assert!(edited.contains("RUN echo custom-neovim"), "{edited}");
        assert!(
            !edited.contains("wget -qO-"),
            "the edited command replaced the recipe: {edited}"
        );
        assert!(app.overrides.contains_key("neovim"));
        assert_eq!(app.package_info().unwrap().value, "echo custom-neovim");
    }

    /// Move the package cursor onto `name`.
    fn select_package(app: &mut App, name: &str) {
        let row = app
            .package_rows
            .iter()
            .position(|row| match row {
                PackageRow::Package { label, .. } => label.split(' ').next() == Some(name),
                PackageRow::Header { .. } => false,
            })
            .unwrap_or_else(|| panic!("package {name} not listed"));
        app.focus = Focus::Apps;
        app.apps_list_state.select(Some(row));
    }

    /// Move the distro cursor onto `name` and select it.
    fn select_distro(app: &mut App, name: &str) {
        let target = app.distros.iter().position(|distro| distro.name == name);
        let row = app.distro_rows.iter().position(|row| match row {
            DistroRow::Distro { index, .. } => Some(*index) == target,
            DistroRow::Header { .. } => false,
        });
        app.focus = Focus::Distro;
        app.distro_list_state.select(row);
        app.toggle_option();
    }

    fn names(app: &App) -> Vec<&str> {
        app.package_rows
            .iter()
            .filter(|row| !row.is_header())
            .map(PackageRow::label)
            .collect()
    }

    fn package_row_labels(app: &App) -> Vec<&str> {
        app.package_rows.iter().map(PackageRow::label).collect()
    }

    fn row_labels(app: &App) -> Vec<String> {
        app.distro_rows
            .iter()
            .map(|row| row.label().to_string())
            .collect()
    }

    fn toolbox(image: &str, status: ToolboxStatus) -> Toolbox {
        Toolbox::built(
            Manifest {
                version: crate::manifest::MANIFEST_VERSION,
                image: image.to_string(),
                image_id: Some("sha256:aaaa11112222".to_string()),
                distro: ManifestDistro {
                    name: "jazzy".to_string(),
                    image: "docker.io/library/ros:jazzy-ros-base".to_string(),
                    family: "ros".to_string(),
                },
                packages: vec![crate::manifest::ManifestPackage {
                    name: "htop".to_string(),
                    version: String::new(),
                    pkg: Some("htop".to_string()),
                    script: None,
                }],
                created_at: 0,
                tool_version: "test".to_string(),
            },
            status,
            vec![],
        )
    }

    fn toolbox_scan(toolboxes: Vec<Toolbox>) -> ToolboxScan {
        ToolboxScan {
            toolboxes,
            warnings: vec![],
        }
    }

    // --- selection ---------------------------------------------------------

    #[test]
    fn packages_are_filtered_by_the_selected_distro() {
        let mut catalog = catalog_with(&[("always", &[]), ("only-lyrical", &[])]);
        catalog.packages[1].install.insert(
            "lyrical".to_string(),
            InstallSpec::PackageManager {
                pkg: "only-lyrical".to_string(),
            },
        );
        catalog.packages[1].install.remove(DEFAULT_INSTALL_KEY);

        let mut app = app(catalog);
        assert_eq!(names(&app), vec!["always", "only-lyrical"]);

        select_distro(&mut app, "jazzy");
        assert_eq!(names(&app), vec!["always"]);

        select_distro(&mut app, "lyrical");
        assert_eq!(names(&app), vec!["always", "only-lyrical"]);
    }

    #[test]
    fn selecting_a_distro_generates_a_containerfile() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "kilted");
        assert!(
            app.builder
                .get_container_file()
                .starts_with("FROM docker.io/library/ros:kilted-ros-base")
        );

        select_package(&mut app, "htop");
        app.toggle_option();
        assert!(app.builder.get_container_file().contains("apt-get install"));
        assert!(app.builder.get_container_file().contains("    htop \\"));
    }

    #[test]
    fn fedora_distro_builds_a_dnf_containerfile() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "41");
        select_package(&mut app, "htop");
        app.toggle_option();

        let file = app.builder.get_container_file();
        assert!(file.starts_with("FROM docker.io/library/fedora:41"));
        assert!(file.contains("RUN dnf install -y"));
        assert!(!file.contains("apt-get"));
    }

    #[test]
    fn selecting_a_package_adds_its_requirements() {
        let mut app = app(catalog_with(&[("curl", &[]), ("rust", &["curl"])]));
        select_package(&mut app, "curl");
        app.toggle_option();
        assert_eq!(app.selected_packages.len(), 1);

        select_package(&mut app, "rust");
        app.toggle_option();
        assert!(app.selected_packages.contains("rust"));
        assert!(app.selected_packages.contains("curl"));

        app.toggle_option(); // deselect rust again
        assert!(!app.selected_packages.contains("rust"));
        assert!(
            !app.selected_packages.contains("curl"),
            "orphaned requirement should be dropped"
        );
    }

    #[test]
    fn requirement_shared_by_two_packages_survives() {
        let mut app = app(catalog_with(&[
            ("curl", &[]),
            ("rust", &["curl"]),
            ("uv", &["curl"]),
        ]));
        select_package(&mut app, "rust");
        app.toggle_option();
        select_package(&mut app, "uv");
        app.toggle_option();

        select_package(&mut app, "rust");
        app.toggle_option(); // deselect rust
        assert!(app.selected_packages.contains("uv"));
        assert!(app.selected_packages.contains("curl"));
    }

    #[test]
    fn select_all_toggles_every_available_package() {
        let mut app = app(catalog_with(&[
            ("htop", &[]),
            ("ripgrep", &[]),
            ("curl", &[]),
        ]));
        select_distro(&mut app, "jazzy");

        app.toggle_all_packages();
        assert_eq!(app.selected_packages.len(), 3);
        let file = app.builder.get_container_file();
        for name in ["htop", "ripgrep", "curl"] {
            assert!(file.contains(name), "{name} missing");
        }

        app.toggle_all_packages();
        assert!(app.selected_packages.is_empty());
        let cleared = app.builder.get_container_file();
        for name in ["htop", "ripgrep", "curl"] {
            assert!(!cleared.contains(&format!("    {name} \\")), "{cleared}");
        }
        // The base-image fix stays: it is not tied to the selection.
        assert!(cleared.contains("libnss-myhostname"), "{cleared}");
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("Cleared 3 package(s)")),
            "{:?}",
            app.logs
        );
    }

    #[test]
    fn select_all_respects_distro_availability_and_clears_overrides() {
        let mut app = app(categorized_with_unavailable());
        select_distro(&mut app, "jazzy");

        app.toggle_all_packages();
        assert!(app.selected_packages.contains("curl"));
        assert!(app.selected_packages.contains("rust"));
        assert!(
            !app.selected_packages.contains("only-lyrical"),
            "unavailable packages must not be selected"
        );

        select_package(&mut app, "rust");
        app.toggle_install_method();
        assert_eq!(app.overrides.len(), 1);

        app.toggle_all_packages(); // everything visible selected -> clear
        assert!(app.selected_packages.is_empty());
        assert!(app.overrides.is_empty(), "overrides follow the selection");
    }

    fn categorized_with_unavailable() -> Catalog {
        let mut catalog =
            catalog_with(&[("curl", &[]), ("rust", &["curl"]), ("only-lyrical", &[])]);
        catalog.packages[2].install.insert(
            "lyrical".to_string(),
            InstallSpec::PackageManager {
                pkg: "only-lyrical".to_string(),
            },
        );
        catalog.packages[2].install.remove(DEFAULT_INSTALL_KEY);
        catalog
    }

    // --- grouping and navigation -------------------------------------------

    #[test]
    fn packages_are_grouped_by_category() {
        let app = app(categorised(&[
            ("htop", "Shell"),
            ("bat", "Shell"),
            ("ripgrep", "Files"),
            ("clangd", "C/C++"),
            ("weird", "Zzz custom"),
        ]));

        assert_eq!(
            package_row_labels(&app),
            vec![
                "Shell",
                "htop",
                "bat",
                "Files",
                "ripgrep",
                "C/C++",
                "clangd",
                "Zzz custom",
                "weird",
            ]
        );
        assert!(!app.package_rows[app.apps_list_state.selected().unwrap()].is_header());
    }

    #[test]
    fn package_navigation_walks_headings_so_they_can_be_folded() {
        let mut app = app(categorised(&[
            ("htop", "Shell"),
            ("bat", "Shell"),
            ("ripgrep", "Files"),
        ]));
        app.focus = Focus::Apps;

        // The cursor starts on the first package, not on the heading above it.
        assert_eq!(app.package_under_cursor().unwrap().name, "htop");

        // Down walks every row, headings included …
        app.move_down();
        assert_eq!(app.package_under_cursor().unwrap().name, "bat");
        app.move_down();
        assert_eq!(app.package_header_under_cursor().as_deref(), Some("Files"));
        assert!(
            app.package_under_cursor().is_none(),
            "a heading is not a package"
        );
        app.move_down();
        assert_eq!(app.package_under_cursor().unwrap().name, "ripgrep");
        app.move_down();
        assert_eq!(
            app.package_under_cursor().unwrap().name,
            "ripgrep",
            "clamped"
        );

        // … and back up.
        app.move_up();
        assert_eq!(app.package_header_under_cursor().as_deref(), Some("Files"));
        app.move_up();
        assert_eq!(app.package_under_cursor().unwrap().name, "bat");
    }

    #[test]
    fn one_key_folds_and_unfolds_every_category() {
        let mut app = app(categorised(&[
            ("htop", "Shell"),
            ("bat", "Shell"),
            ("ripgrep", "Files"),
            ("clangd", "C/C++"),
        ]));
        assert!(app.collapsed_categories.is_empty(), "expanded by default");

        // First press folds everything: only the headings stay.
        app.toggle_all_categories();
        assert_eq!(app.collapsed_categories.len(), 3);
        assert_eq!(package_row_labels(&app), vec!["Shell", "Files", "C/C++"]);
        assert!(app.package_rows.iter().all(|row| matches!(
            row,
            PackageRow::Header {
                collapsed: true,
                ..
            }
        )));

        // Second press unfolds them again.
        app.toggle_all_categories();
        assert!(app.collapsed_categories.is_empty());
        assert_eq!(
            package_row_labels(&app),
            vec![
                "Shell", "htop", "bat", "Files", "ripgrep", "C/C++", "clangd"
            ]
        );

        // A single folded category is enough for "fold all" to expand instead.
        app.toggle_category("Files");
        app.toggle_all_categories();
        assert!(app.collapsed_categories.is_empty(), "expanded everything");
    }

    #[test]
    fn folding_everything_keeps_the_cursor_on_a_visible_row() {
        let mut app = app(categorised(&[("htop", "Shell"), ("ripgrep", "Files")]));
        app.focus = Focus::Apps;

        // Cursor on the last package row: after folding it no longer exists.
        app.apps_list_state.select(Some(3));
        app.toggle_all_categories();
        let selected = app.apps_list_state.selected().expect("a row is selected");
        assert!(
            selected < app.package_rows.len(),
            "cursor stayed on the list"
        );
        assert_eq!(app.package_header_under_cursor().as_deref(), Some("Files"));

        // Cursor on a package in a middle category: the cursor follows that heading.
        app.toggle_all_categories();
        let files = app
            .package_rows
            .iter()
            .position(|row| row.label() == "Files")
            .unwrap();
        app.apps_list_state.select(Some(files + 1));
        app.toggle_all_categories();
        assert_eq!(app.package_header_under_cursor().as_deref(), Some("Files"));
    }

    #[test]
    fn local_images_can_be_hidden_and_shown_again() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        let mut distros = Distro::predefined();
        distros.push(Distro::new(
            DistroFamily::Ubuntu,
            "local-24.04",
            "localhost/my-ubuntu:24.04",
            DistroSource::Local,
        ));
        app.distros = distros;
        app.rebuild_views();
        assert!(app.show_local_images, "shown by default");
        assert!(
            app.distro_rows
                .iter()
                .any(|row| row.label().contains("(local)"))
        );

        // The option row exists and mirrors the state.
        assert_eq!(app.options_rows().len(), App::OPTIONS_ROWS);
        assert_eq!(
            app.options_rows()[App::LOCAL_IMAGES_ROW].label,
            "[x] local images"
        );

        // Hiding drops the local entries from the list.
        app.focus = Focus::Options;
        app.options_cursor = App::LOCAL_IMAGES_ROW;
        app.toggle_option();
        assert!(!app.show_local_images);
        assert!(
            !app.distro_rows
                .iter()
                .any(|row| row.label().contains("(local)"))
        );
        assert_eq!(
            app.options_rows()[App::LOCAL_IMAGES_ROW].label,
            "[ ] local images"
        );
        // The cursor is still parked on a selectable distro.
        let selected = app.distro_list_state.selected().expect("selection");
        assert!(!app.distro_rows[selected].is_header());

        // Showing them again brings the image back.
        app.toggle_option();
        assert_eq!(app.options_cursor, App::LOCAL_IMAGES_ROW);
        assert!(app.show_local_images);
        assert!(
            app.distro_rows
                .iter()
                .any(|row| row.label().contains("(local)"))
        );
    }

    #[test]
    fn hiding_local_images_releases_a_selected_local_distro() {
        let mut only_local = app(catalog_with(&[("htop", &[])]));
        let local = Distro::new(
            DistroFamily::Ubuntu,
            "local-24.04",
            "localhost/my-ubuntu:24.04",
            DistroSource::Local,
        );
        only_local.distros = vec![local.clone()];
        only_local.selected_distro = Some(0);
        only_local.builder.set_distro(local);
        only_local.rebuild_views();
        only_local.rebuild_container_file();
        assert!(
            only_local
                .builder
                .get_container_file()
                .contains("my-ubuntu")
        );

        only_local.toggle_local_images();

        // No distro is left, so the builder falls back to the bare Containerfile.
        assert!(!only_local.show_local_images);
        assert_eq!(only_local.selected_distro, None);
        assert!(
            !only_local
                .builder
                .get_container_file()
                .contains("my-ubuntu")
        );
        assert!(only_local.distro_rows.is_empty());

        // With a predefined distro present, that one takes over instead.
        let mut second = app(catalog_with(&[("htop", &[])]));
        let mut distros = Distro::predefined();
        distros.push(Distro::new(
            DistroFamily::Ubuntu,
            "local-24.04",
            "localhost/my-ubuntu:24.04",
            DistroSource::Local,
        ));
        second.distros = distros;
        let last = second.distros.len() - 1;
        second.selected_distro = Some(last);
        let local = second.distros[last].clone();
        second.builder.set_distro(local);
        second.rebuild_views();
        second.rebuild_container_file();

        second.toggle_local_images();
        let selected = second.selected_distro.expect("a distro stays selected");
        assert_ne!(second.distros[selected].source, DistroSource::Local);
        assert!(!second.builder.get_container_file().contains("my-ubuntu"));
    }

    #[test]
    fn categories_are_expanded_by_default_and_can_be_folded() {
        let mut app = app(categorised(&[
            ("htop", "Shell"),
            ("bat", "Shell"),
            ("ripgrep", "Files"),
            ("clangd", "C/C++"),
        ]));
        assert!(app.collapsed_categories.is_empty(), "expanded by default");
        assert_eq!(
            package_row_labels(&app),
            vec![
                "Shell", "htop", "bat", "Files", "ripgrep", "C/C++", "clangd"
            ]
        );

        // Fold "Shell" from its heading: its packages disappear, the heading stays.
        app.focus = Focus::Apps;
        let header = app
            .package_rows
            .iter()
            .position(|row| row.label() == "Shell")
            .unwrap();
        app.apps_list_state.select(Some(header));
        app.toggle_option();
        assert!(app.collapsed_categories.contains("Shell"));
        assert_eq!(
            package_row_labels(&app),
            vec!["Shell", "Files", "ripgrep", "C/C++", "clangd"]
        );
        // The cursor parks on the heading that was folded.
        assert_eq!(app.package_header_under_cursor().as_deref(), Some("Shell"));

        // The header carries its count and fold state for the UI.
        let PackageRow::Header {
            count, collapsed, ..
        } = app.package_rows[0].clone()
        else {
            panic!("expected a heading");
        };
        assert_eq!(count, 2, "both Shell packages counted, hidden or not");
        assert!(collapsed);
        assert_eq!(app.package_rows[0].display_label(), "▸ Shell (2)");

        // Unfold again.
        app.toggle_option();
        assert!(app.collapsed_categories.is_empty());
        assert_eq!(package_row_labels(&app).len(), 7);
        assert_eq!(app.package_rows[0].display_label(), "▾ Shell (2)");

        // Folding survives a distro change: it is keyed by category name.
        app.toggle_category("Shell");
        assert!(app.collapsed_categories.contains("Shell"));
        app.apply_distro_refresh(DistroRefresh {
            distros: Distro::predefined(),
            warnings: vec![],
        });
        assert!(app.collapsed_categories.contains("Shell"), "still folded");

        // Folding everything is fine: the cursor stays on a heading and nothing breaks.
        // ("Shell" is still folded from the distro-change check above.)
        for category in ["Files", "C/C++"] {
            let index = app
                .package_rows
                .iter()
                .position(|row| row.label() == category)
                .unwrap();
            app.apps_list_state.select(Some(index));
            app.toggle_option();
        }
        assert_eq!(app.collapsed_categories.len(), 3);
        assert_eq!(package_row_labels(&app), vec!["Shell", "Files", "C/C++"]);
        assert!(app.container_highlight().is_empty());
        app.toggle_option(); // toggling selection on a heading is a fold, not a selection
        assert!(app.selected_packages.is_empty());
    }

    #[test]
    fn distros_are_grouped_by_family_with_headings() {
        let app = app(catalog_with(&[("htop", &[])]));
        assert_eq!(
            row_labels(&app),
            vec![
                "ROS", "jazzy", "kilted", "lyrical", "Ubuntu", "24.04", "22.04", "Fedora", "41",
            ]
        );
        assert!(!app.distro_rows[app.distro_list_state.selected().unwrap()].is_header());
    }

    #[test]
    fn navigation_skips_family_headings() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.focus = Focus::Distro;

        for expected in ["kilted", "lyrical", "24.04", "22.04", "41"] {
            app.move_down();
            let row = &app.distro_rows[app.distro_list_state.selected().unwrap()];
            assert_eq!(row.label(), expected);
            assert!(!row.is_header());
        }

        app.move_down();
        let row = &app.distro_rows[app.distro_list_state.selected().unwrap()];
        assert_eq!(row.label(), "41", "stops at the last distro");

        for expected in ["22.04", "24.04", "lyrical"] {
            app.move_up();
            let row = &app.distro_rows[app.distro_list_state.selected().unwrap()];
            assert_eq!(row.label(), expected);
        }
    }

    // --- info popup and overrides ------------------------------------------

    #[test]
    fn info_popup_describes_the_package_under_the_cursor() {
        let mut app = app(categorised(&[("htop", "Shell"), ("ripgrep", "Files")]));
        select_distro(&mut app, "jazzy");

        let info = app.package_info().expect("info");
        assert_eq!(info.name, "htop");
        assert_eq!(info.category, "Shell");
        assert_eq!(info.summary, "htop summary");
        assert_eq!(info.install, "apt-get: htop");
        assert!(info.available);
        assert_eq!(info.distro.as_deref(), Some("jazzy"));
        assert!(!info.selected);

        select_distro(&mut app, "41");
        assert_eq!(app.package_info().unwrap().install, "dnf: htop");
    }

    #[test]
    fn info_popup_toggles_and_esc_closes_it_first() {
        let mut app = app(categorised(&[("htop", "Shell")]));
        assert!(!app.info_open);
        app.toggle_info();
        assert!(app.info_open);
        assert!(app.info_effect.is_some(), "opening starts a fade-in effect");

        assert!(app.close_info());
        assert!(!app.info_open);
        assert!(app.info_effect.is_none());
        assert!(!app.close_info(), "nothing left to close");
    }

    #[test]
    fn install_method_can_be_overridden_and_reset() {
        let mut app = app(categorised(&[("htop", "Shell")]));
        select_distro(&mut app, "jazzy");
        select_package(&mut app, "htop");
        app.toggle_option();

        assert_eq!(
            app.package_info().unwrap().method,
            InstallMethod::PackageManager
        );
        assert!(!app.package_info().unwrap().overridden);

        app.toggle_install_method();
        let info = app.package_info().unwrap();
        assert_eq!(info.method, InstallMethod::Script);
        assert!(info.overridden);
        assert!(info.value.contains("apt-get install"));
        assert!(!app.builder.get_container_file().contains("    htop \\"));

        app.reset_install_override();
        assert!(app.overrides.is_empty());
        assert_eq!(
            app.package_info().unwrap().method,
            InstallMethod::PackageManager
        );
        assert!(app.builder.get_container_file().contains("    htop \\"));
    }

    #[test]
    fn overrides_are_recorded_in_the_manifest() {
        let mut app = app(categorised(&[("htop", "Shell")]));
        select_distro(&mut app, "jazzy");
        select_package(&mut app, "htop");
        app.toggle_option();
        app.overrides.insert(
            "htop".to_string(),
            InstallOverride {
                method: InstallMethod::Script,
                value: "make install-htop".to_string(),
            },
        );
        app.rebuild_container_file();

        let manifest = app.manifest_for_selection().unwrap();
        assert_eq!(manifest.packages[0].name, "htop");
        assert_eq!(manifest.packages[0].pkg, None);
        assert_eq!(
            manifest.packages[0].script.as_deref(),
            Some("make install-htop")
        );
    }

    #[test]
    fn the_version_of_a_script_package_can_be_chosen() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        // A versioned script package, on top of the fixture catalog.
        app.catalog.packages.push(Package {
            name: "nvim".to_string(),
            version: "0.12.1".to_string(),
            summary: String::new(),
            category: "Editors".to_string(),
            requires: vec![],
            install: BTreeMap::from([(
                DEFAULT_INSTALL_KEY.to_string(),
                InstallSpec::Script {
                    script: "fetch v{{version}}/nvim-linux-{{arch:x86_64,arm64}}.tar.gz"
                        .to_string(),
                },
            )]),
            env: BTreeMap::new(),
        });
        select_distro(&mut app, "jazzy");
        app.rebuild_views();

        // Package-manager installs have no version to pick.
        select_package(&mut app, "htop");
        app.open_version_prompt();
        assert!(!app.prompt_is_open());
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("no selectable version")),
            "{:?}",
            app.logs
        );

        select_package(&mut app, "nvim");
        app.toggle_option();
        app.open_version_prompt();
        let prompt = app.prompt.as_ref().expect("version prompt");
        assert_eq!(prompt.title, "Version — nvim");
        assert_eq!(prompt.input, "0.12.1", "prefilled with the catalog version");

        app.prompt_insert('0');
        app.prompt_insert('.');
        app.prompt_insert('1');
        app.prompt_insert('1');
        app.prompt_submit();
        assert_eq!(app.versions.get("nvim").map(String::as_str), Some("0.11"));

        let file = app.builder.get_container_file();
        assert!(file.contains("v0.11/nvim-linux-"), "{file}");
        assert!(!file.contains("{{"), "{file}");
        assert_eq!(app.package_info().unwrap().version, "0.11");

        // The manifest records the version that was actually installed.
        let manifest = app.manifest_for_selection().unwrap();
        let recorded = manifest
            .packages
            .iter()
            .find(|package| package.name == "nvim")
            .expect("nvim in the manifest");
        assert_eq!(recorded.version, "0.11");
    }

    #[test]
    fn the_containerfile_highlight_follows_the_selected_cursor_package() {
        let mut app = app(catalog_with(&[
            ("curl", &[]),
            ("rust", &["curl"]),
            ("htop", &[]),
        ]));
        select_distro(&mut app, "jazzy");

        select_package(&mut app, "rust");
        assert!(app.container_highlight().is_empty(), "nothing selected");

        app.toggle_option();
        let highlight = app.container_highlight();
        assert_eq!(highlight.primary.as_deref(), Some("rust"));
        assert!(highlight.dependencies.contains("curl"));

        select_package(&mut app, "htop");
        app.toggle_option();
        let highlight = app.container_highlight();
        assert_eq!(highlight.primary.as_deref(), Some("htop"));
        assert!(highlight.dependencies.is_empty());

        select_package(&mut app, "curl");
        app.toggle_option(); // deselect
        assert!(app.container_highlight().is_empty());
    }

    // --- logs and build progress -------------------------------------------

    #[test]
    fn logs_are_trimmed_and_bounded() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.append_log("first\r".to_string());
        assert_eq!(app.logs.back().map(String::as_str), Some("first"));

        for i in 0..MAX_LOG_LINES + 10 {
            app.append_log(format!("line {i}"));
        }
        assert_eq!(app.logs.len(), MAX_LOG_LINES);
        assert_eq!(
            app.logs.back().map(String::as_str),
            Some(format!("line {}", MAX_LOG_LINES + 9).as_str())
        );
    }

    #[test]
    fn new_output_does_not_shift_the_view_while_scrolled_back() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.log_view_height = 5;
        for i in 0..20 {
            app.append_log(format!("line {i}"));
        }
        assert_eq!(app.log_scroll, 0, "following the tail by default");

        // Scroll back three lines: the window is pinned to the content, not the tail.
        for _ in 0..3 {
            app.scroll_logs_up();
        }
        let first_visible = |app: &App| {
            let offset = app.logs.len() - app.log_view_height - app.log_scroll;
            app.logs[offset].clone()
        };
        let pinned = first_visible(&app);

        // Ten more lines arrive while the build streams output.
        for i in 20..30 {
            app.append_log(format!("line {i}"));
        }
        assert_eq!(app.log_scroll, 13, "scrolled by as many lines as arrived");
        assert_eq!(
            first_visible(&app),
            pinned,
            "the reader keeps looking at the same lines"
        );

        // Back at the tail it follows again.
        for _ in 0..20 {
            app.scroll_logs_down();
        }
        assert_eq!(app.log_scroll, 0);
        app.append_log("latest".to_string());
        assert_eq!(app.logs.back().map(String::as_str), Some("latest"));
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn dropping_the_oldest_line_keeps_the_view_still() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.log_view_height = 5;
        for i in 0..MAX_LOG_LINES {
            app.append_log(format!("line {i}"));
        }
        for _ in 0..3 {
            app.scroll_logs_up();
        }
        let offset = app.logs.len() - app.log_view_height - app.log_scroll;
        let pinned = app.logs[offset].clone();

        // The buffer is full, so the next line evicts the oldest one.
        app.append_log("one more".to_string());
        assert_eq!(app.logs.len(), MAX_LOG_LINES);
        let offset_after = app.logs.len() - app.log_view_height - app.log_scroll;
        assert_eq!(app.logs[offset_after], pinned);
    }

    #[test]
    fn log_scroll_is_clamped_to_the_content() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.log_view_height = 10;
        for i in 0..25 {
            app.append_log(format!("line {i}"));
        }
        assert_eq!(app.max_log_scroll(), 15);

        for _ in 0..100 {
            app.scroll_logs_up();
        }
        assert_eq!(app.log_scroll, 15);
        app.scroll_logs_down();
        assert_eq!(app.log_scroll, 14);
    }

    #[test]
    fn run_build_without_a_containerfile_only_logs() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.run_build();
        assert!(!app.process_running);
        assert!(
            app.logs
                .back()
                .is_some_and(|line| line.contains("Nothing to build"))
        );
    }

    #[test]
    fn build_progress_follows_podman_output() {
        let mut progress = BuildProgress::default();
        assert_eq!(progress.phase, BuildPhase::Idle);
        assert!(progress.summary().is_empty());

        progress.observe("Building localhost/toolshop:jazzy-1a2b3c from ros");
        assert_eq!(progress.phase, BuildPhase::Preparing);

        progress.observe("STEP 1/4: FROM docker.io/library/ros:jazzy-ros-base");
        assert_eq!(progress.phase, BuildPhase::Running);
        assert_eq!(progress.step, Some((1, 4)));
        assert_eq!(progress.ratio(), Some(0.25));

        progress.observe("STEP 2/4: RUN apt-get install -y htop");
        assert!(progress.summary().starts_with("STEP 2/4 · "));

        progress.observe("level=warning msg=\"rootless mapping\"");
        progress.observe("Error: something odd");
        assert_eq!(progress.warnings, 1);
        assert_eq!(progress.errors, 1);
        assert!(progress.summary().contains("1 warning"));
        assert!(progress.summary().contains("1 error"));

        progress.observe("COMMIT localhost/toolshop:jazzy-1a2b3c");
        assert_eq!(progress.phase, BuildPhase::Committing);

        progress.observe("Build finished (exit status: 0): localhost/toolshop:jazzy-1a2b3c");
        assert_eq!(progress.phase, BuildPhase::Succeeded);
        assert_eq!(progress.ratio(), Some(1.0));
        assert!(progress.summary().starts_with("done · "));
        assert!(!progress.is_active());
    }

    #[test]
    fn build_progress_ignores_logs_outside_a_build() {
        let mut progress = BuildProgress::default();
        progress.observe("warning: local images unavailable: boom");
        progress.observe("Error: unrelated");
        assert_eq!(progress.phase, BuildPhase::Idle);
        assert_eq!(progress.warnings, 0);
        assert_eq!(progress.errors, 0);
        assert_eq!(progress.lines, 0);

        progress.start();
        progress.observe("Build failed: podman exited with exit status: 125");
        assert_eq!(progress.phase, BuildPhase::Failed);
        assert_eq!(progress.errors, 1);
        assert!(progress.summary().starts_with("failed"));
    }

    #[test]
    fn starting_a_build_resets_the_progress() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.build.start();
        app.build.observe("STEP 3/4: RUN x");
        app.build.observe("level=warning msg=\"w\"");
        assert_eq!(app.build.step, Some((3, 4)));
        assert_eq!(app.build.warnings, 1);

        app.build.start();
        assert_eq!(app.build.phase, BuildPhase::Preparing);
        assert_eq!(app.build.step, None);
        assert_eq!(app.build.lines, 0);

        // Without a runtime the build refuses to start instead of panicking.
        select_distro(&mut app, "jazzy");
        select_package(&mut app, "htop");
        app.toggle_option();
        app.run_build();
        assert!(!app.process_running);
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("No async runtime")),
            "{:?}",
            app.logs
        );
    }

    #[test]
    fn a_failed_build_opens_the_log_at_the_failure() {
        let mut failed = app(catalog_with(&[("htop", &[])]));
        failed.view = View::Toolboxes;
        failed.log_view_height = 5;
        for i in 0..20 {
            failed.append_log(format!("line {i}"));
        }
        failed.log_scroll = 10;

        failed.build.start();
        failed.append_log("STEP 1/3: FROM x".to_string());
        failed.append_log("Build failed: podman exited with exit status: 125".to_string());

        assert_eq!(failed.view, View::Logs);
        assert_eq!(failed.log_scroll, 0);
        assert_eq!(failed.build.phase, BuildPhase::Failed);

        // A successful build does not steal the view.
        let mut ok = app(catalog_with(&[("htop", &[])]));
        ok.view = View::Builder;
        ok.build.start();
        ok.append_log("Build finished (exit status: 0): img".to_string());
        assert_eq!(ok.view, View::Builder);
    }

    #[test]
    fn step_parsing_handles_podman_shapes() {
        assert_eq!(parse_step("STEP 1/3: FROM x"), Some((1, 3)));
        assert_eq!(parse_step("STEP 12/40: RUN y"), Some((12, 40)));
        assert_eq!(parse_step("--> abc123"), None);
        assert_eq!(parse_step("garbage"), None);
        assert_eq!(parse_step("STEP nope: x"), None);
        assert_eq!(format_duration(Duration::from_secs(3)), "3s");
        assert_eq!(format_duration(Duration::from_secs(75)), "1m15s");
    }

    // --- views -------------------------------------------------------------

    #[test]
    fn build_options_are_checkboxes_that_rebuild_the_containerfile() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "jazzy");
        assert_eq!(app.options, BuildOptions::default(), "both on by default");
        assert!(app.builder.get_container_file().contains("usermod -l"));

        // Cursor navigation inside the options block: 3 checkboxes + the user row.
        app.focus = Focus::Options;
        assert_eq!(app.options_cursor, 0);
        app.move_down();
        assert_eq!(app.options_cursor, 1);
        app.move_down();
        assert_eq!(app.options_cursor, 2);
        app.move_down();
        assert_eq!(app.options_cursor, BuildOptions::USER_ROW);
        app.move_down();
        assert_eq!(app.options_cursor, App::TOOLBOX_ROW);
        app.move_down();
        assert_eq!(app.options_cursor, App::LOCAL_IMAGES_ROW);
        app.move_down();
        assert_eq!(app.options_cursor, App::ENV_ROW);
        app.move_down();
        assert_eq!(app.options_cursor, App::RESET_ROW);
        app.move_down();
        assert_eq!(app.options_cursor, App::RESET_ROW, "clamped");
        app.move_up();
        assert_eq!(app.options_cursor, App::ENV_ROW);
        app.move_up();
        assert_eq!(app.options_cursor, App::LOCAL_IMAGES_ROW);
        app.move_up();
        assert_eq!(app.options_cursor, App::TOOLBOX_ROW);
        app.move_up();
        assert_eq!(app.options_cursor, BuildOptions::USER_ROW);
        app.move_up();
        assert_eq!(app.options_cursor, 2);
        app.move_up();
        app.move_up();
        assert_eq!(app.options_cursor, 0);

        // Enter toggles the row under the cursor.
        app.toggle_option();
        assert!(!app.options.user_setup);
        assert!(app.options.hostname_fix);
        let file = app.builder.get_container_file();
        assert!(!file.contains("usermod -l"), "{file}");
        assert!(file.contains("libnss-myhostname"), "{file}");
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("user setup (uid 1000): off")),
            "{:?}",
            app.logs
        );

        app.move_down();
        app.toggle_option();
        assert!(!app.options.hostname_fix);
        let file = app.builder.get_container_file();
        assert!(!file.contains("libnss-myhostname"), "{file}");
        assert!(!file.contains("nsswitch.conf"), "{file}");

        // Toggling back restores the defaults.
        app.toggle_option();
        app.move_up();
        app.toggle_option();
        assert_eq!(app.options, BuildOptions::default());
        assert!(app.builder.get_container_file().contains("usermod -l"));
    }

    #[test]
    fn the_user_row_opens_a_prompt_and_hides_the_name() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "jazzy");
        app.focus = Focus::Options;
        app.options_cursor = BuildOptions::USER_ROW;

        // Enter on the value row asks for the name.
        app.toggle_option();
        let prompt = app.prompt.as_ref().expect("user prompt");
        assert_eq!(prompt.title, "User in the Containerfile");

        app.prompt_insert('d');
        app.prompt_insert('e');
        app.prompt_insert('v');
        app.prompt_submit();
        assert_eq!(app.options.username.as_deref(), Some("dev"));
        assert!(
            app.builder
                .get_container_file()
                .contains("ARG DOCKER_USER=dev")
        );

        // Clearing the field goes back to the host account.
        app.open_user_prompt();
        app.prompt_clear();
        app.prompt_submit();
        assert!(app.options.username.is_none());
        assert_eq!(app.recorded_user(), app.builder.docker_user_name());

        // The `hide username` checkbox records the placeholder instead.
        app.options_cursor = 2;
        app.toggle_option();
        assert!(app.options.hide_username);
        assert_eq!(app.recorded_user(), "ubuntu");
        let file = app.builder.get_container_file();
        assert!(file.contains("ARG DOCKER_USER=ubuntu"), "{file}");
        assert!(
            !file.contains(&format!(
                "ARG DOCKER_USER={}",
                app.builder.docker_user_name()
            )),
            "{file}"
        );
        // The row says so.
        assert!(
            app.options_rows()
                .iter()
                .any(|row| row.label.contains("(hidden)")),
            "{:?}",
            app.options_rows()
        );
    }

    #[test]
    fn the_toolbox_name_is_set_from_the_options_block() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "jazzy");
        select_package(&mut app, "htop");
        app.toggle_option();

        let rows = app.options_rows();
        assert_eq!(rows.len(), App::OPTIONS_ROWS);
        assert!(
            rows[App::TOOLBOX_ROW].label.contains("(auto)"),
            "{:?}",
            rows[App::TOOLBOX_ROW]
        );
        assert!(!rows[App::TOOLBOX_ROW].enabled);
        assert!(rows[App::TOOLBOX_ROW].editable);

        // Enter on that row opens the same prompt the `n` key does.
        app.focus = Focus::Options;
        app.options_cursor = App::TOOLBOX_ROW;
        app.toggle_option();
        let prompt = app.prompt.as_ref().expect("toolbox name prompt");
        assert!(prompt.title.starts_with("Toolbox name"), "{}", prompt.title);

        app.prompt_insert('d');
        app.prompt_insert('e');
        app.prompt_insert('v');
        app.prompt_submit();
        assert_eq!(app.pending_toolbox.as_deref(), Some("dev"));
        let rows = app.options_rows();
        assert_eq!(rows[App::TOOLBOX_ROW].label, "toolbox: dev");
        assert!(rows[App::TOOLBOX_ROW].enabled);
    }

    #[test]
    fn focus_follows_the_layout_from_top_left_to_bottom_right() {
        let mut app = app(catalog_with(&[("htop", &[])]));

        // Layout order: options (above), distro list, packages, Containerfile.
        assert_eq!(
            Focus::ALL,
            [Focus::Options, Focus::Distro, Focus::Apps, Focus::Container]
        );

        // Right walks the layout.
        app.focus = Focus::Options;
        for expected in [Focus::Distro, Focus::Apps, Focus::Container, Focus::Options] {
            app.focus_right();
            assert_eq!(app.focus, expected);
        }

        // Left walks back: from the distro list it reaches the options above it, and from
        // the packages it goes to the distro list (not the options).
        app.focus = Focus::Distro;
        app.focus_left();
        assert_eq!(app.focus, Focus::Options);
        app.focus_right();
        assert_eq!(app.focus, Focus::Distro);
        app.focus_right();
        assert_eq!(app.focus, Focus::Apps);
        app.focus_left();
        assert_eq!(app.focus, Focus::Distro);
        app.focus_left();
        assert_eq!(app.focus, Focus::Options);
        app.focus_left();
        assert_eq!(app.focus, Focus::Container, "wraps around");
    }

    #[test]
    fn clicking_an_option_toggles_it() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "jazzy");
        app.hit.options = Rect::new(0, 10, 20, App::OPTIONS_ROWS as u16 + 2);
        // Rows start one line below the top border.
        click_at(&mut app, 3, 11);
        assert_eq!(app.focus, Focus::Options);
        assert!(!app.options.user_setup);
        assert!(app.options.hostname_fix);

        click_at(&mut app, 3, 12);
        assert!(!app.options.hostname_fix);
        assert_eq!(app.options_cursor, 1);

        // Clicking the toolbox row opens its prompt rather than toggling anything.
        click_at(&mut app, 3, 10 + 1 + App::TOOLBOX_ROW as u16);
        assert_eq!(app.options_cursor, App::TOOLBOX_ROW);
        assert!(app.prompt_is_open());
    }

    #[test]
    fn tabs_cycle_through_every_view() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        assert_eq!(app.view, View::Builder);
        assert_eq!(app.view_index(), 0);

        app.next_view();
        assert_eq!(app.view, View::Toolboxes);
        app.next_view();
        assert_eq!(app.view, View::Logs);
        app.next_view();
        assert_eq!(app.view, View::Builder, "wraps around");

        app.previous_view();
        assert_eq!(app.view, View::Logs);
        app.previous_view();
        assert_eq!(app.view, View::Toolboxes);
    }

    #[test]
    fn the_logs_view_toggles_from_the_builder_and_scrolls() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.log_view_height = 5;
        for i in 0..20 {
            app.append_log(format!("line {i}"));
        }

        app.toggle_logs_view();
        assert_eq!(app.view, View::Logs);

        let cursor = app.distro_list_state.selected();
        app.move_up();
        app.move_up();
        assert_eq!(app.log_scroll, 2);
        app.move_down();
        assert_eq!(app.log_scroll, 1);
        assert_eq!(app.distro_list_state.selected(), cursor);

        app.toggle_option();
        assert!(app.selected_packages.is_empty());

        app.toggle_logs_view();
        assert_eq!(app.view, View::Builder);
    }

    #[test]
    fn t_cycles_the_theme_and_reports_it() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        crate::theme::with_flavour(crate::theme::Flavour::Mocha, || {
            app.cycle_theme();
            assert_eq!(crate::theme::flavour(), crate::theme::Flavour::Latte);
            assert!(app.logs.iter().any(|line| line.contains("Theme: Latte")));

            for _ in 0..3 {
                app.cycle_theme();
            }
            assert_eq!(crate::theme::flavour(), crate::theme::Flavour::Mocha);
            assert!(
                std::fs::read_to_string(&app.theme_file)
                    .unwrap()
                    .contains("theme = \"mocha\""),
                "the choice is persisted"
            );
        });
    }

    // --- distro refresh and user images ------------------------------------

    #[test]
    fn distro_refresh_keeps_the_selected_image_and_adds_labels() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        select_distro(&mut app, "jazzy");

        let mut distros = Distro::predefined();
        distros.push(Distro::from_image("localhost/ros:jazzy-dev").unwrap());
        app.apply_distro_refresh(DistroRefresh {
            distros,
            warnings: vec!["local images unavailable: boom".to_string()],
        });

        assert_eq!(app.distros.len(), 7);
        assert!(
            app.distro_rows
                .iter()
                .any(|row| row.label() == "ros:jazzy-dev (local)")
        );
        assert_eq!(
            app.current_distro().map(|distro| distro.image.as_str()),
            Some("docker.io/library/ros:jazzy-ros-base")
        );
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("local images unavailable"))
        );
    }

    #[test]
    fn adding_an_image_through_the_prompt_registers_and_selects_it() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.open_image_prompt();
        for character in "localhost/dev-sandbox:ubuntu24".chars() {
            app.prompt_insert(character);
        }
        app.prompt_cycle_family(true); // ROS -> Ubuntu
        assert_eq!(
            app.prompt.as_ref().unwrap().family(),
            Some(DistroFamily::Ubuntu)
        );
        app.prompt_submit();

        let added = app
            .distros
            .iter()
            .find(|distro| distro.image == "localhost/dev-sandbox:ubuntu24")
            .expect("image added");
        assert_eq!(added.family, DistroFamily::Ubuntu);
        assert!(added.is_custom());
        assert_eq!(
            app.current_distro().map(|distro| distro.name.as_str()),
            Some("dev-sandbox")
        );
        assert!(
            app.builder
                .get_container_file()
                .starts_with("FROM localhost/dev-sandbox:ubuntu24")
        );

        let saved = std::fs::read_to_string(&app.images_file).expect("images file");
        assert!(saved.contains("localhost/dev-sandbox:ubuntu24"), "{saved}");
    }

    #[test]
    fn prompt_editing_and_cancelling() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.open_image_prompt();
        app.prompt_insert('a');
        app.prompt_insert('b');
        app.prompt_backspace();
        assert_eq!(app.prompt.as_ref().unwrap().input, "a");
        app.prompt_clear();
        assert_eq!(app.prompt.as_ref().unwrap().input, "");
        app.prompt_cancel();
        assert!(!app.prompt_is_open());
        assert_eq!(app.distros.len(), Distro::predefined().len());

        app.open_image_prompt();
        app.prompt_submit();
        assert!(app.logs.iter().any(|line| line.contains("Nothing entered")));
    }

    #[test]
    fn a_prefilled_prompt_is_replaced_by_the_first_keystroke() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.apply_toolbox_scan(toolbox_scan(vec![toolbox(
            "localhost/toolshop:jazzy-aaaaaa",
            ToolboxStatus::Present,
        )]));
        app.create_toolbox();
        assert_eq!(
            app.prompt.as_ref().unwrap().input,
            "toolshop-jazzy-aaaaaa",
            "opens with a suggestion"
        );

        app.prompt_insert('d');
        assert_eq!(app.prompt.as_ref().unwrap().input, "d");
        app.prompt_insert('e');
        app.prompt_insert('v');
        app.prompt_submit();
        assert_eq!(app.toolbox_scroll, 0);
        assert!(
            app.logs
                .iter()
                .any(|line| line
                    .contains("Creating toolbox dev from localhost/toolshop:jazzy-aaaaaa")),
            "{:?}",
            app.logs
        );
    }

    #[test]
    fn removing_a_user_image_clears_the_selection() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.add_custom_distro("localhost/dev-sandbox:ubuntu24", DistroFamily::Ubuntu);
        assert!(app.current_distro().is_some());

        app.remove_custom_distro();
        assert!(
            !app.distros
                .iter()
                .any(|distro| distro.image == "localhost/dev-sandbox:ubuntu24")
        );
        assert!(app.current_distro().is_none());
        assert!(app.builder.get_container_file().is_empty());

        app.selected_distro = Some(0);
        app.remove_custom_distro();
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("not a user-added image"))
        );
    }

    // --- toolboxes ---------------------------------------------------------

    #[test]
    fn toolbox_scan_keeps_the_cursor_on_the_same_image() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.apply_toolbox_scan(toolbox_scan(vec![
            toolbox("localhost/toolshop:jazzy-aaaaaa", ToolboxStatus::Present),
            toolbox("localhost/toolshop:jazzy-bbbbbb", ToolboxStatus::Missing),
        ]));
        app.toolbox_state.select(Some(1));

        app.apply_toolbox_scan(toolbox_scan(vec![
            toolbox("localhost/toolshop:jazzy-bbbbbb", ToolboxStatus::Present),
            toolbox("localhost/toolshop:jazzy-aaaaaa", ToolboxStatus::Present),
        ]));
        assert_eq!(
            app.selected_toolbox()
                .map(|toolbox| toolbox.manifest.as_ref().unwrap().image.as_str()),
            Some("localhost/toolshop:jazzy-bbbbbb")
        );
    }

    #[test]
    fn movement_in_the_toolbox_tab_does_not_touch_the_builder_cursor() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.apply_toolbox_scan(toolbox_scan(vec![
            toolbox("localhost/toolshop:jazzy-aaaaaa", ToolboxStatus::Present),
            toolbox("localhost/toolshop:jazzy-bbbbbb", ToolboxStatus::Present),
        ]));
        app.switch_view(View::Toolboxes);

        let distro_cursor = app.distro_list_state.selected();
        app.move_down();
        assert_eq!(app.toolbox_state.selected(), Some(1));
        assert_eq!(app.distro_list_state.selected(), distro_cursor);

        app.toggle_option();
        app.run_build();
        assert!(!app.process_running);
    }

    #[test]
    fn forgetting_a_manifest_keeps_the_image_as_external() {
        std::fs::create_dir_all(&app(catalog_with(&[])).cache_dir).ok();
        let mut app = app(catalog_with(&[("htop", &[])]));
        std::fs::create_dir_all(&app.cache_dir).unwrap();
        let manifest = Manifest::from_selection(
            &Distro::predefined()[0],
            &[],
            "localhost/toolshop:jazzy-dddddd".to_string(),
        );
        crate::manifest::save_to(&app.cache_dir, &manifest).unwrap();

        let mut built = toolbox("localhost/toolshop:jazzy-dddddd", ToolboxStatus::Present);
        built.containers = vec!["dev".to_string()];
        app.apply_toolbox_scan(toolbox_scan(vec![built]));
        assert!(app.selected_toolbox().unwrap().manifest.is_some());

        app.forget_selected_toolbox();
        assert_eq!(app.toolboxes.len(), 1);
        assert!(!app.toolboxes[0].is_toolshop_build());
        assert_eq!(app.toolboxes[0].containers, vec!["dev".to_string()]);
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("manifest removed"))
        );
    }

    #[test]
    fn forgetting_an_external_toolbox_only_reports_it() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.apply_toolbox_scan(toolbox_scan(vec![Toolbox::external(
            "localhost/dev-sandbox:ubuntu24",
            ToolboxStatus::Present,
            vec!["dev".to_string()],
        )]));

        app.forget_selected_toolbox();
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("not built by toolshop"))
        );
        assert_eq!(app.toolboxes.len(), 1);
    }

    #[test]
    fn toolbox_actions_need_a_selected_toolbox() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.create_toolbox();
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("No toolbox selected"))
        );

        app.apply_toolbox_scan(toolbox_scan(vec![toolbox(
            "localhost/toolshop:jazzy-aaaaaa",
            ToolboxStatus::Present,
        )]));
        app.remove_toolboxes();
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("No toolbox containers")),
            "{:?}",
            app.logs
        );
    }

    #[test]
    fn the_builder_can_name_the_toolbox_created_after_a_build() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.open_pending_toolbox_prompt();
        assert!(!app.prompt_is_open(), "needs a distro first");
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("Select a distro first"))
        );

        select_distro(&mut app, "jazzy");
        select_package(&mut app, "htop");
        app.toggle_option();
        app.open_pending_toolbox_prompt();
        assert!(
            app.prompt
                .as_ref()
                .unwrap()
                .input
                .starts_with("toolshop-jazzy-")
        );

        app.prompt_insert('d');
        app.prompt_insert('e');
        app.prompt_insert('v');
        app.prompt_submit();
        assert_eq!(app.pending_toolbox.as_deref(), Some("dev"));
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("Build to create toolbox dev"))
        );

        app.set_pending_toolbox("bad name");
        assert_eq!(app.pending_toolbox.as_deref(), Some("dev"));
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("is not a valid toolbox name"))
        );
    }

    #[tokio::test]
    async fn a_successful_build_consumes_the_pending_name() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        app.pending_toolbox = Some("dev".to_string());
        app.build_ok = Some(Arc::new(AtomicBool::new(true)));
        app.build_handle = Some(tokio::spawn(async {}));
        tokio::time::sleep(Duration::from_millis(20)).await;

        app.on_tick();
        assert!(app.pending_toolbox.is_none());
        assert!(
            app.logs
                .iter()
                .any(|line| line.contains("Toolbox name used"))
        );

        // A failed build keeps the name for the next attempt.
        app.pending_toolbox = Some("dev".to_string());
        app.build_ok = Some(Arc::new(AtomicBool::new(false)));
        app.build_handle = Some(tokio::spawn(async {}));
        tokio::time::sleep(Duration::from_millis(20)).await;
        app.on_tick();
        assert_eq!(app.pending_toolbox.as_deref(), Some("dev"));
    }

    // --- mouse -------------------------------------------------------------

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click_at(app: &mut App, column: u16, row: u16) {
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
    }

    /// Plausible hit areas for a 100x24 terminal.
    fn layout(app: &mut App) {
        app.hit.tabs = vec![
            (View::Builder, Rect::new(0, 0, 9, 1)),
            (View::Toolboxes, Rect::new(10, 0, 11, 1)),
            (View::Logs, Rect::new(22, 0, 6, 1)),
        ];
        app.hit.options = Rect::new(0, 1, 20, 7);
        app.hit.distros = Rect::new(0, 8, 20, 13);
        app.hit.packages = Rect::new(21, 1, 30, 20);
        app.hit.containerfile = Rect::new(52, 1, 48, 20);
        app.hit.toolboxes = Rect::new(0, 1, 38, 20);
        app.hit.toolbox_details = Rect::new(39, 1, 61, 20);
        app.hit.logs = Rect::new(0, 1, 100, 20);
    }

    #[test]
    fn clicking_a_tab_switches_views() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        layout(&mut app);

        click_at(&mut app, 15, 0);
        assert_eq!(app.view, View::Toolboxes);
        click_at(&mut app, 2, 0);
        assert_eq!(app.view, View::Builder);
        click_at(&mut app, 24, 0);
        assert_eq!(app.view, View::Logs);
    }

    #[test]
    fn clicking_a_package_moves_the_cursor_then_toggles_it() {
        let mut app = app(categorised(&[("htop", "Shell"), ("ripgrep", "Files")]));
        layout(&mut app);
        app.selected_distro = Some(0);
        app.builder.set_distro(Distro::predefined()[0].clone());

        // Inner rows: "Shell"=2, htop=3, "Files"=4, ripgrep=5.
        click_at(&mut app, 25, 5);
        assert_eq!(app.focus, Focus::Apps);
        assert_eq!(app.apps_list_state.selected(), Some(3));
        assert!(app.selected_packages.is_empty(), "first click only selects");

        click_at(&mut app, 25, 5);
        assert!(app.selected_packages.contains("ripgrep"));

        // Clicking a heading folds that category instead of toggling a package.
        click_at(&mut app, 25, 4);
        assert!(app.collapsed_categories.contains("Files"));
        assert_eq!(
            app.package_header_under_cursor().as_deref(),
            Some("Files"),
            "the cursor parks on the heading that was folded"
        );
        assert_eq!(app.selected_packages.len(), 1, "the selection is untouched");

        // … and clicking it again unfolds it.
        click_at(&mut app, 25, 4);
        assert!(app.collapsed_categories.is_empty());
    }

    #[test]
    fn clicking_a_distro_row_selects_it_on_the_second_click() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        layout(&mut app);

        // Distro pane starts at y=8: "ROS" heading=9, jazzy=10, kilted=11.
        click_at(&mut app, 3, 11);
        assert_eq!(app.focus, Focus::Distro);
        assert_eq!(app.distro_list_state.selected(), Some(2));
        assert_eq!(app.selected_distro, None);

        click_at(&mut app, 3, 11);
        assert_eq!(
            app.current_distro().map(|distro| distro.name.as_str()),
            Some("kilted")
        );
    }

    #[test]
    fn clicking_uses_the_list_scroll_offset() {
        let mut app = app(catalog_with(&[
            ("a", &[]),
            ("b", &[]),
            ("c", &[]),
            ("d", &[]),
            ("e", &[]),
            ("f", &[]),
        ]));
        layout(&mut app);
        app.apps_list_state = ListState::default().with_offset(5);

        click_at(&mut app, 25, 2);
        assert_eq!(app.apps_list_state.selected(), Some(5));
        click_at(&mut app, 25, 3);
        assert_eq!(app.apps_list_state.selected(), Some(6));
    }

    #[test]
    fn wheel_scrolls_whatever_is_under_the_pointer() {
        let mut app = app(categorised(&[("htop", "Shell"), ("ripgrep", "Files")]));
        layout(&mut app);
        app.log_view_height = 5;
        app.selected_distro = Some(0);
        app.builder.set_distro(Distro::predefined()[0].clone());
        for i in 0..20 {
            app.append_log(format!("line {i}"));
        }

        assert_eq!(app.apps_list_state.selected(), Some(1), "htop");
        // The wheel walks the same rows as the arrows: packages and headings.
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 25, 5));
        assert_eq!(app.apps_list_state.selected(), Some(2), "Files heading");
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 25, 5));
        assert_eq!(app.apps_list_state.selected(), Some(3), "ripgrep");
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 25, 5));
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 25, 5));
        assert_eq!(app.apps_list_state.selected(), Some(1));

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 60, 5));
        assert_eq!(app.container_scroll, 3);
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 60, 5));
        assert_eq!(app.container_scroll, 0);

        app.switch_view(View::Logs);
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 10, 5));
        assert_eq!(app.log_scroll, 1);
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, 5));
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn wheel_over_the_toolbox_details_scrolls_them() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        layout(&mut app);
        app.apply_toolbox_scan(toolbox_scan(vec![toolbox(
            "localhost/toolshop:jazzy-aaaaaa",
            ToolboxStatus::Present,
        )]));
        app.switch_view(View::Toolboxes);

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 50, 5));
        assert_eq!(app.toolbox_scroll, 1);
        app.handle_mouse(mouse(MouseEventKind::ScrollUp, 50, 5));
        assert_eq!(app.toolbox_scroll, 0);

        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 5));
        assert_eq!(app.toolbox_scroll, 0);
        assert_eq!(app.toolbox_state.selected(), Some(0));
    }

    #[test]
    fn the_prompt_swallows_mouse_events() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        layout(&mut app);
        app.open_image_prompt();

        click_at(&mut app, 15, 0);
        assert_eq!(app.view, View::Builder);
        assert!(app.prompt_is_open());

        app.prompt_cancel();
        click_at(&mut app, 15, 0);
        assert_eq!(app.view, View::Toolboxes);
    }

    #[test]
    fn clicking_outside_the_info_popup_closes_it() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        layout(&mut app);
        app.hit.info_popup = Rect::new(20, 5, 40, 10);
        app.toggle_info();
        assert!(app.info_open);

        click_at(&mut app, 30, 8);
        assert!(app.info_open, "clicks inside are ignored");

        click_at(&mut app, 2, 22);
        assert!(!app.info_open);
    }
}
