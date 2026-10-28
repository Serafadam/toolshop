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
    container_builder::ContainerBuilder,
    distro::{self, Distro, DistroFamily, DistroSource},
    manifest::{self, Manifest},
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

#[derive(Debug, PartialEq, Eq)]
pub enum Focus {
    Distro,
    Apps,
    Container,
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
    },
    /// `visible` is an index into [`App::visible_packages`].
    Package {
        visible: usize,
        label: String,
    },
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
    /// Name for the toolbox created after the next successful build.
    PendingToolbox {
        image: String,
        #[allow(dead_code)]
        suggested: String,
    },
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

impl App {
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
            apps_list_state: ListState::default().with_selected(Some(0)),
            focus: Focus::Distro,
            should_quit: false,
            token: CancellationToken::new(),
            process_running: false,
            logs: VecDeque::new(),
            log_scroll: 0,
            log_view_height: 0,
            info_open: false,
            overrides: BTreeMap::new(),
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
        self.should_quit = true;
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
        self.visible_packages = self
            .catalog
            .packages
            .iter()
            .enumerate()
            .filter(|(_, package)| match &distro {
                Some(distro) => package.is_available_on(distro),
                None => true,
            })
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
            rows.push(PackageRow::Header {
                label: category.clone(),
                accent,
            });
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

        // Keep the cursor on the row it was on, unless that was a heading.
        if self.package_rows.is_empty() {
            self.apps_list_state.select(None);
        } else {
            let index = self
                .apps_list_state
                .selected()
                .unwrap_or(0)
                .min(self.package_rows.len() - 1);
            let index = if self.package_rows[index].is_header() {
                (0..self.package_rows.len())
                    .find(|row| !self.package_rows[*row].is_header())
                    .unwrap_or(index)
            } else {
                index
            };
            self.apps_list_state.select(Some(index));
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
        let value = match &spec {
            Some(InstallSpec::PackageManager { pkg }) => pkg.clone(),
            Some(InstallSpec::Script { script }) => script.clone(),
            None => String::new(),
        };

        Some(PackageInfo {
            name: package.name.clone(),
            category: catalog::category_of(package).to_string(),
            version: package.version.clone(),
            summary: package.summary.clone(),
            requires: package.requires.clone(),
            install,
            method,
            value,
            overridden,
            script: match &spec {
                Some(InstallSpec::Script { script }) => Some(script.clone()),
                _ => None,
            },
            available: distro.is_none_or(|distro| package.is_available_on(distro)),
            distro: distro.map(|distro| distro.name.clone()),
            selected: self.selected_packages.contains(&package.name),
        })
    }

    /// Install instruction in effect for `name` on the current distro.
    fn resolved_spec(&self, name: &str) -> Option<InstallSpec> {
        let distro = self.current_distro()?;
        let package = self.catalog.get(name)?;
        catalog::resolve_install(package, distro, &self.overrides)
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
    }

    /// Drop the override so the catalog entry applies again.
    pub fn reset_install_override(&mut self) {
        let Some(package) = self.package_under_cursor().map(|p| p.name.clone()) else {
            return;
        };
        if self.overrides.remove(&package).is_some() {
            self.append_log(format!("{package}: install reset to the catalog entry"));
            self.rebuild_container_file();
        }
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
    }

    pub fn prompt_backspace(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.pristine = false;
            prompt.input.pop();
        }
    }

    /// Ctrl+U: empty the field.
    pub fn prompt_clear(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.input.clear();
            prompt.pristine = false;
        }
    }

    pub fn prompt_cycle_family(&mut self, forward: bool) {
        if let Some(prompt) = &mut self.prompt {
            prompt.cycle_family(forward);
        }
    }

    pub fn prompt_cancel(&mut self) {
        self.prompt = None;
    }

    /// Apply the overlay input: add an image, set an override, or name a toolbox.
    pub fn prompt_submit(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let input = prompt.input.trim().to_string();
        if input.is_empty() {
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
            }
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
        let tx = self.log_tx.clone();
        self.toolbox_action = Some(runtime.spawn(async move {
            let message = match toolbox::create_container(&image, Some(&name)).await {
                Ok(message) => message,
                Err(err) => format!("toolbox create failed: {err:#}"),
            };
            let _ = tx.send(message).await;
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

    pub fn focus_right(&mut self) {
        self.focus = match self.focus {
            Focus::Distro => Focus::Apps,
            Focus::Apps => Focus::Container,
            Focus::Container => Focus::Distro,
        };
    }

    pub fn focus_left(&mut self) {
        self.focus = match self.focus {
            Focus::Distro => Focus::Container,
            Focus::Container => Focus::Apps,
            Focus::Apps => Focus::Distro,
        };
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
            Focus::Apps => self.step_package_cursor(true),
            Focus::Container => self.scroll_container(true),
        }
    }

    /// Move the package cursor, skipping category headings.
    fn step_package_cursor(&mut self, forward: bool) {
        let Some(current) = self.apps_list_state.selected() else {
            let first = self.package_rows.iter().position(|row| !row.is_header());
            self.apps_list_state.select(first);
            return;
        };

        let next = if forward {
            (current + 1..self.package_rows.len())
                .find(|index| !self.package_rows[*index].is_header())
        } else {
            (0..current)
                .rev()
                .find(|index| !self.package_rows[*index].is_header())
        };

        if let Some(index) = next {
            self.apps_list_state.select(Some(index));
        }
    }

    pub fn toggle_option(&mut self) {
        if self.view != View::Builder {
            return;
        }
        match self.focus {
            Focus::Distro => self.select_distro_under_cursor(),
            Focus::Apps => self.toggle_package_under_cursor(),
            Focus::Container => {}
        }
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
                        if self.package_rows[index].is_header() {
                            return;
                        }
                        if self.apps_list_state.selected() == Some(index) {
                            self.toggle_package_under_cursor();
                        } else {
                            self.apps_list_state.select(Some(index));
                        }
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
            image,
        ))
    }
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
    fn category_navigation_skips_headings() {
        let mut app = app(categorised(&[
            ("htop", "Shell"),
            ("bat", "Shell"),
            ("ripgrep", "Files"),
        ]));
        app.focus = Focus::Apps;

        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(app.package_under_cursor().unwrap().name.clone());
            app.move_down();
        }
        assert_eq!(seen, vec!["htop", "bat", "ripgrep", "ripgrep"]);

        app.move_up();
        assert_eq!(app.package_under_cursor().unwrap().name, "bat");
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
        app.hit.distros = Rect::new(0, 1, 20, 20);
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

        click_at(&mut app, 25, 4); // heading
        assert_eq!(app.apps_list_state.selected(), Some(3));
        assert_eq!(app.selected_packages.len(), 1);
    }

    #[test]
    fn clicking_a_distro_row_selects_it_on_the_second_click() {
        let mut app = app(catalog_with(&[("htop", &[])]));
        layout(&mut app);

        // "ROS" heading=2, jazzy=3, kilted=4.
        click_at(&mut app, 3, 4);
        assert_eq!(app.focus, Focus::Distro);
        assert_eq!(app.distro_list_state.selected(), Some(2));
        assert_eq!(app.selected_distro, None);

        click_at(&mut app, 3, 4);
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
        app.handle_mouse(mouse(MouseEventKind::ScrollDown, 25, 5));
        assert_eq!(app.apps_list_state.selected(), Some(3), "ripgrep");
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
