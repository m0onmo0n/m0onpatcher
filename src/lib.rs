//! M0on's patcher: a small standalone GUI (and command line) around the `kitty-engine` crate.
//!
//! This is the library behind two programs: `m0onpatcher` (the window, `src/main.rs`) and `m0onpatcher-cli`
//! (the terminal, `src/bin/m0onpatcher-cli.rs`, see `cli`).
//!
//! * **Patcher** tab: pick the game HTML and the mods folder, press Patch.
//! * **Modding tools** tab: extract the game HTML into editable sources and compile them back into an HTML
//!   (ScRepacker's extractor and merger).
//! * **About** tab.
//!
//! The log window can be shown or hidden; hidden, a progress bar is shown instead.

#[allow(dead_code)]
mod encoding;
#[allow(dead_code)]
mod extraction_naming;
#[allow(dead_code)]
mod game_data;
#[allow(dead_code)]
mod game_data_extractor;
#[allow(dead_code)]
mod game_data_merger;
#[allow(dead_code)]
mod game_data_reader;
pub mod cli;
mod normalize;
mod stages;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use eframe::egui;
use log::{Level, LevelFilter, Log, Metadata, Record};
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "m0onpatcher-config.json";
const SIZE_COMPACT: [f32; 2] = [680.0, 470.0];
const SIZE_WITH_LOG: [f32; 2] = [900.0, 650.0];
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");
const REPO_URL: &str = "https://github.com/m0onmo0n/m0onpatcher";

fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------- config

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    html: Option<PathBuf>,
    mods: Option<PathBuf>,
    /// Output file chosen with "Save as"; `None` means `<name>.modded.html` next to the game HTML.
    output: Option<PathBuf>,
    show_log: bool,
    /// legacy phase: `~` / `~~` blocks run first and replace every match (see `kitty_engine::load::LoadOptions`)
    legacy_replace_all: bool,
    /// the Stage builder list (Modding tools tab)
    pipeline: Vec<stages::Item>,
}

impl Default for Config {
    fn default() -> Self {
        Self { html: None, mods: None, output: None, show_log: true, legacy_replace_all: false, pipeline: Vec::new() }
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn load_config() -> Config {
    let Some(path) = exe_dir().map(|d| d.join(CONFIG_FILE)) else { return Config::default() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    let Some(path) = exe_dir().map(|d| d.join(CONFIG_FILE)) else { return };
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, text);
    }
}

// ---------------------------------------------------------------- shared state and background tasks

#[derive(Clone, PartialEq)]
enum Status {
    Idle,
    Running,
    Done(String),
    Failed(String),
}

struct Shared {
    log: Vec<String>,
    progress: f32,
    /// no measurable progress (extract / compile): the UI shows a spinner instead of a bar
    indeterminate: bool,
    /// name of the running task, for the status label ("Patching", "Extracting", ...)
    task: String,
    status: Status,
}

type SharedRef = Arc<Mutex<Shared>>;

/// Sends the `log` records of the extractor / merger (and any other code of this program) to the log window.
struct GuiLogger {
    shared: SharedRef,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl Log for GuiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info && metadata.target().starts_with("m0onpatcher")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        {
            let mut s = self.shared.lock().unwrap();
            for l in record.args().to_string().lines() {
                s.log.push(format!("[{}] {l}", record.level()));
            }
        }
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    fn flush(&self) {}
}

/// Whether (and how much of) the log is also printed to the terminal. The window uses `Off`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Echo {
    Off,
    /// only warnings and errors
    Quiet,
    /// stage messages, warnings and errors
    Normal,
    /// everything, including the per-block patch log
    Verbose,
}

struct Reporter {
    shared: SharedRef,
    ctx: egui::Context,
    echo: Echo,
}

impl Reporter {
    fn line(&self, level: &str, msg: impl AsRef<str>) {
        {
            let mut s = self.shared.lock().unwrap();
            for l in msg.as_ref().lines() {
                s.log.push(format!("[{level}] {l}"));
            }
        }
        match (self.echo, level) {
            (Echo::Off, _) => {}
            (_, "WARN" | "ERROR") => {
                for l in msg.as_ref().lines() {
                    eprintln!("[{level}] {l}");
                }
            }
            (Echo::Normal | Echo::Verbose, _) => {
                for l in msg.as_ref().lines() {
                    println!("[{level}] {l}");
                }
            }
            (Echo::Quiet, _) => {}
        }
        self.ctx.request_repaint();
    }

    /// A line of the per-block patch log: always in the window's log, on the terminal only with `--verbose`.
    fn detail(&self, msg: impl AsRef<str>) {
        {
            let mut s = self.shared.lock().unwrap();
            for l in msg.as_ref().lines() {
                s.log.push(format!("[INFO] {l}"));
            }
        }
        if self.echo == Echo::Verbose {
            for l in msg.as_ref().lines() {
                println!("[INFO] {l}");
            }
        }
        self.ctx.request_repaint();
    }

    /// Progress only moves forward.
    fn progress(&self, p: f32) {
        {
            let mut s = self.shared.lock().unwrap();
            if p > s.progress {
                s.progress = p;
            }
        }
        self.ctx.request_repaint();
    }
}

/// Run `work` on a worker thread; its result becomes the Done / Failed status and the last log line.
fn spawn_task<F>(shared: SharedRef, ctx: egui::Context, task: &str, indeterminate: bool, work: F)
where
    F: FnOnce(&Reporter) -> anyhow::Result<String> + Send + 'static,
{
    {
        let mut s = shared.lock().unwrap();
        s.log.clear();
        s.progress = 0.0;
        s.indeterminate = indeterminate;
        s.task = task.to_string();
        s.status = Status::Running;
    }
    // generous stack: the engine recurses a little while folding replacements
    let spawned = std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
        let reporter = Reporter { shared: shared.clone(), ctx: ctx.clone(), echo: Echo::Off };
        let result = work(&reporter);
        {
            let mut s = shared.lock().unwrap();
            match result {
                Ok(msg) => {
                    s.progress = 1.0;
                    s.log.push(format!("[INFO] {msg}"));
                    s.status = Status::Done(msg);
                }
                Err(err) => {
                    let msg = format!("{err:#}");
                    s.log.push(format!("[ERROR] {msg}"));
                    s.status = Status::Failed(msg);
                }
            }
        }
        ctx.request_repaint();
    });
    if let Err(err) = spawned {
        eprintln!("cannot start the worker thread: {err}");
    }
}

// ---------------------------------------------------------------- patch

struct PatchJob {
    html: PathBuf,
    mods: PathBuf,
    output: PathBuf,
    legacy_replace_all: bool,
}

/// Rough position of the engine's progress messages on the bar (the engine only reports stages,
/// and "Applying replacements: n/total" while it works through the Replace blocks).
fn stage_progress(msg: &str) -> f32 {
    if let Some(rest) = msg.strip_prefix("Applying legacy blocks: ") {
        if let Some((n, t)) = rest.split_once('/') {
            if let (Ok(n), Ok(t)) = (n.trim().parse::<f32>(), t.trim().parse::<f32>()) {
                if t > 0.0 {
                    return 0.12 + 0.03 * (n / t).min(1.0);
                }
            }
        }
        return 0.12;
    }
    if let Some(rest) = msg.strip_prefix("Applying replacements: ") {
        if let Some((n, t)) = rest.split_once('/') {
            if let (Ok(n), Ok(t)) = (n.trim().parse::<f32>(), t.trim().parse::<f32>()) {
                if t > 0.0 {
                    return 0.15 + 0.5 * (n / t).min(1.0);
                }
            }
        }
        return 0.15;
    }
    match msg {
        "Applying legacy blocks" => 0.12,
        "Applying replacements" => 0.15,
        "Applying passage and function directives" => 0.65,
        "Adding passages, scripts and styles" => 0.85,
        "Renumbering passage ids" => 0.95,
        _ => 0.0,
    }
}

fn run_patch(job: &PatchJob, r: &Reporter) -> anyhow::Result<String> {
    let started = Instant::now();
    if job.output == job.html {
        bail!("the output file would overwrite the game HTML; choose a different output file");
    }

    r.line("INFO", format!("Loading mods from {}", job.mods.display()));
    let opts = kitty_engine::load::LoadOptions { legacy_replace_all: job.legacy_replace_all };
    let loaded = kitty_engine::load::load_mods_with(&job.mods, &opts).context("load mods")?;
    for w in &loaded.warnings {
        r.line("WARN", w);
    }
    if loaded.loaded_mods.is_empty() {
        bail!("no .mod files found in {}", job.mods.display());
    }
    r.line("INFO", format!("Applying {} mod file(s) with the internal engine", loaded.loaded_mods.len()));
    log_legacy_summary(&loaded, r);
    r.progress(0.05);

    // Read the HTML the way KittyPatcher does (Python text mode: universal newlines).
    let bytes = std::fs::read(&job.html).with_context(|| format!("read {}", job.html.display()))?;
    let html = String::from_utf8(bytes).map_err(|_| anyhow!("{} is not valid UTF-8", job.html.display()))?;
    let html = kitty_engine::pytext::universal_newlines(&html);
    r.progress(0.10);

    let log_dir = job.html.parent().map(|p| p.join("logs")).unwrap_or_else(|| PathBuf::from("logs"));
    let outcome = apply_loaded(&html, &loaded, &log_dir, r, 0.0, 1.0)?;
    r.progress(0.96);

    write_html(&job.output, &outcome.html)?;
    Ok(format!(
        "Done: {} mod file(s), {} replacements made in {:.1} s. Wrote {}",
        loaded.loaded_mods.len(),
        outcome.replacements_made,
        started.elapsed().as_secs_f64(),
        job.output.display()
    ))
}

/// One line about the legacy phase when some mod uses it.
fn log_legacy_summary(loaded: &kitty_engine::load::LoadResult, r: &Reporter) {
    if loaded.state.legacy_list.is_empty() {
        return;
    }
    let mut mods: Vec<&str> = loaded.state.legacy_list.iter().map(|e| e.mod_file.as_str()).collect();
    mods.dedup();
    r.line(
        "INFO",
        format!(
            "Legacy phase: {} ~ / ~~ block(s) of {} mod file(s) are applied first and replace every match",
            loaded.state.legacy_list.len(),
            mods.len()
        ),
    );
}

/// Patch `html` with the loaded mods: progress on the bar between `base` and `base + span`, the three logs written
/// to `log_dir`, the patch log copied to the window, and an error when any block failed (nothing is written then).
fn apply_loaded(
    html: &str,
    loaded: &kitty_engine::load::LoadResult,
    log_dir: &Path,
    r: &Reporter,
    base: f32,
    span: f32,
) -> anyhow::Result<kitty_engine::apply::Outcome> {
    let outcome = kitty_engine::apply::patch_with_progress(html, &loaded.state, &loaded.loaded_mods, &mut |m| {
        r.line("INFO", m);
        r.progress(base + span * stage_progress(m));
    })
    .map_err(|e| anyhow!("{e:#}"))?;

    write_engine_logs(log_dir, &outcome, r);
    for l in &outcome.log {
        r.detail(l);
    }
    if outcome.replacements_failed > 0 || !outcome.fail_log.is_empty() {
        for l in &outcome.fail_log {
            r.line("ERROR", l);
        }
        bail!(
            "{} replacement(s) failed, so no output file was written (details in FailsPatchLog.txt)",
            outcome.replacements_failed.max(outcome.fail_log.len())
        );
    }
    Ok(outcome)
}

/// Write an HTML file. KittyPatcher writes Windows line endings on Windows; keep that so the result is identical.
fn write_html(path: &Path, html: &str) -> anyhow::Result<()> {
    let text = if cfg!(windows) { html.replace('\n', "\r\n") } else { html.to_string() };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// The three KittyPatcher-style logs, written to `dir` (the place to look when a block fails).
fn write_engine_logs(dir: &Path, outcome: &kitty_engine::apply::Outcome, r: &Reporter) {
    let dir = dir.to_path_buf();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        r.line("WARN", format!("cannot create {}: {err}", dir.display()));
        return;
    }
    let join = |lines: &[String]| {
        let mut s = lines.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s
    };
    let main = format!(
        "M0on's patcher (kitty-engine)\nTotal replacements made: {}\nTotal replacements failed: {}\n",
        outcome.replacements_made, outcome.replacements_failed
    );
    for (name, text) in [
        ("FailsPatchLog.txt", join(&outcome.fail_log)),
        ("ModPatchLog.txt", format!("Mod patching started...\n{}", join(&outcome.log))),
        ("MainPatchLog.txt", main),
    ] {
        if let Err(err) = std::fs::write(dir.join(name), text) {
            r.line("WARN", format!("cannot write {}: {err}", dir.join(name).display()));
        }
    }
    r.line("INFO", format!("Logs written to {}", dir.display()));
}

// ---------------------------------------------------------------- modding tools: extract / compile

fn source_dir(html: &Path) -> Option<PathBuf> {
    html.parent().map(|p| p.join("src"))
}

fn compiled_path(html: &Path) -> Option<PathBuf> {
    let stem = html.file_stem()?.to_string_lossy().into_owned();
    Some(html.with_file_name(format!("{stem}.merged.html")))
}

fn run_extract(html: &Path, src: &Path) -> anyhow::Result<String> {
    let started = Instant::now();
    game_data_extractor::extract(html, src)?;
    Ok(format!("Done: extracted the sources to {} in {:.1} s", src.display(), started.elapsed().as_secs_f64()))
}

fn run_compile(html: &Path, src: &Path, output: &Path) -> anyhow::Result<String> {
    let started = Instant::now();
    if !src.is_dir() {
        bail!("there is no source folder at {}; extract the game HTML first", src.display());
    }
    if output == html {
        bail!("the output file would overwrite the game HTML");
    }
    game_data_merger::merge(html, output, src)?;
    Ok(format!("Done: compiled {} in {:.1} s", output.display(), started.elapsed().as_secs_f64()))
}

fn open_folder(path: &Path) {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(path).spawn();
}

// ---------------------------------------------------------------- GUI

fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width, height })
}

pub fn run_gui() -> anyhow::Result<()> {
    let cfg = load_config();
    let size = if cfg.show_log { SIZE_WITH_LOG } else { SIZE_COMPACT };

    let shared: SharedRef = Arc::new(Mutex::new(Shared {
        log: Vec::new(),
        progress: 0.0,
        indeterminate: false,
        task: String::new(),
        status: Status::Idle,
    }));
    let log_ctx: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
    let logger = Box::new(GuiLogger { shared: shared.clone(), ctx: log_ctx.clone() });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }

    // winit's Wayland backend has no drag-and-drop; force X11 (XWayland) on Linux like the packer does.
    #[cfg(all(unix, not(target_os = "macos")))]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }));
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = None;

    let mut viewport = egui::ViewportBuilder::default().with_inner_size(size).with_min_inner_size([520.0, 240.0]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, event_loop_builder, ..Default::default() };
    eframe::run_native(
        &format!("M0on's patcher {}", app_version()),
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, cfg, shared, log_ctx)))),
    )
    .map_err(|e| anyhow!("eframe error: {e}"))
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Patcher,
    Tools,
    About,
}

/// The two halves of the Modding tools tab.
#[derive(Clone, Copy, PartialEq)]
enum ToolsMode {
    ExtractCompile,
    Stages,
}

struct App {
    cfg: Config,
    shared: SharedRef,
    tab: Tab,
    auto_scroll: bool,
    mod_count: Option<usize>,
    /// last window layout we asked for, so the size only changes when the log is toggled
    sized_for_log: bool,
    /// "extract again?" question is showing (the source folder already exists)
    confirm_extract: bool,
    tools_mode: ToolsMode,
    icon: Option<egui::TextureHandle>,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        shared: SharedRef,
        log_ctx: Arc<Mutex<Option<egui::Context>>>,
    ) -> Self {
        *log_ctx.lock().unwrap() = Some(cc.egui_ctx.clone());
        cc.egui_ctx.options_mut(|opt| opt.theme_preference = egui::ThemePreference::Dark);
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_gray(27);
        cc.egui_ctx.set_visuals(visuals);

        let icon = window_icon().map(|i| {
            let image = egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba);
            cc.egui_ctx.load_texture("app-icon", image, egui::TextureOptions::LINEAR)
        });
        let sized_for_log = cfg.show_log;
        let mut app = Self {
            cfg,
            shared,
            // M0ON_PATCHER_TAB=tools|about starts on that tab (used for screenshots)
            tab: match std::env::var("M0ON_PATCHER_TAB").as_deref() {
                Ok("tools") => Tab::Tools,
                Ok("about") => Tab::About,
                _ => Tab::Patcher,
            },
            auto_scroll: true,
            mod_count: None,
            sized_for_log,
            confirm_extract: false,
            // M0ON_PATCHER_TOOLS=stages starts the Modding tools tab on the Stage builder (used for screenshots)
            tools_mode: if std::env::var("M0ON_PATCHER_TOOLS").as_deref() == Ok("stages") { ToolsMode::Stages } else { ToolsMode::ExtractCompile },
            icon,
        };
        app.refresh_mod_count();
        app
    }

    fn is_running(&self) -> bool {
        self.shared.lock().unwrap().status == Status::Running
    }

    fn output_path(&self) -> Option<PathBuf> {
        if let Some(o) = &self.cfg.output {
            return Some(o.clone());
        }
        let html = self.cfg.html.as_ref()?;
        let stem = html.file_stem()?.to_string_lossy().into_owned();
        Some(html.with_file_name(format!("{stem}.modded.html")))
    }

    fn refresh_mod_count(&mut self) {
        self.mod_count = self.cfg.mods.as_deref().filter(|d| d.is_dir()).map(count_mod_files);
    }

    fn set_html(&mut self, path: PathBuf) {
        // a `mods` folder next to the game HTML is the usual layout
        if let Some(candidate) = path.parent().map(|p| p.join("mods")) {
            if candidate.is_dir() {
                self.cfg.mods = Some(candidate);
            }
        }
        self.cfg.html = Some(path);
        self.cfg.output = None;
        self.refresh_mod_count();
        save_config(&self.cfg);
    }

    fn set_mods(&mut self, path: PathBuf) {
        self.cfg.mods = Some(path);
        self.refresh_mod_count();
        save_config(&self.cfg);
    }

    fn html_is_file(&self) -> bool {
        self.cfg.html.as_deref().is_some_and(Path::is_file)
    }

    fn can_patch(&self) -> bool {
        self.html_is_file() && self.cfg.mods.as_deref().is_some_and(Path::is_dir) && !self.is_running()
    }

    fn start_patch(&mut self, ctx: &egui::Context) {
        let (Some(html), Some(mods), Some(output)) = (self.cfg.html.clone(), self.cfg.mods.clone(), self.output_path()) else {
            return;
        };
        self.auto_scroll = true;
        let job = PatchJob { html, mods, output, legacy_replace_all: self.cfg.legacy_replace_all };
        spawn_task(self.shared.clone(), ctx.clone(), "Patching", false, move |r| run_patch(&job, r));
    }

    fn start_extract(&mut self, ctx: &egui::Context) {
        let Some(html) = self.cfg.html.clone() else { return };
        let Some(src) = source_dir(&html) else { return };
        self.auto_scroll = true;
        spawn_task(self.shared.clone(), ctx.clone(), "Extracting", true, move |_| run_extract(&html, &src));
    }

    fn start_compile(&mut self, ctx: &egui::Context) {
        let Some(html) = self.cfg.html.clone() else { return };
        let (Some(src), Some(out)) = (source_dir(&html), compiled_path(&html)) else { return };
        self.auto_scroll = true;
        spawn_task(self.shared.clone(), ctx.clone(), "Compiling", true, move |_| run_compile(&html, &src, &out));
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|d| d.path.clone()).collect());
        if self.is_running() || self.tab == Tab::About {
            return;
        }
        for path in dropped {
            if self.tab == Tab::Tools && self.tools_mode == ToolsMode::Stages {
                self.stage_drop(path);
                continue;
            }
            if path.is_dir() {
                if self.tab == Tab::Patcher {
                    self.set_mods(path);
                }
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
            {
                self.set_html(path);
            }
        }
    }

    fn path_box_width(ui: &egui::Ui) -> f32 {
        (ui.available_width() - 90.0 - 110.0 - 3.0 * ui.spacing().item_spacing.x).max(150.0)
    }

    // ---- Patcher tab

    fn render_patcher(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let running = self.is_running();
        let output = self.output_path();
        let mods_hint = self.mod_count.map(|n| format!("{n} mod file(s)")).unwrap_or_default();
        let box_w = Self::path_box_width(ui);
        let (mut pick_html, mut pick_mods, mut pick_out) = (false, false, false);

        egui::Grid::new("patcher-paths").num_columns(3).spacing([8.0, 8.0]).show(ui, |ui| {
            pick_html = path_row(ui, "Game HTML", self.cfg.html.as_deref(), "drop or choose the game .html", box_w, Some(("Browse…", !running)));
            pick_mods = path_row(ui, "Mods folder", self.cfg.mods.as_deref(), "drop or choose the folder with the .mod files", box_w, Some(("Browse…", !running)));
            pick_out = path_row(ui, "Output", output.as_deref(), "", box_w, Some(("Save as…", !running && self.cfg.html.is_some())));
        });
        if !mods_hint.is_empty() {
            ui.weak(mods_hint);
        }

        if pick_html {
            if let Some(p) = rfd::FileDialog::new().add_filter("HTML", &["html", "htm"]).pick_file() {
                self.set_html(p);
            }
        }
        if pick_mods {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                self.set_mods(p);
            }
        }
        if pick_out {
            let mut dlg = rfd::FileDialog::new().add_filter("HTML", &["html"]);
            if let Some(o) = &output {
                if let Some(dir) = o.parent() {
                    dlg = dlg.set_directory(dir);
                }
                if let Some(name) = o.file_name() {
                    dlg = dlg.set_file_name(name.to_string_lossy());
                }
            }
            if let Some(p) = dlg.save_file() {
                self.cfg.output = Some(p);
                save_config(&self.cfg);
            }
        }

        self.render_legacy_checkbox(ui);

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let patch = ui.add_enabled(self.can_patch(), egui::Button::new("Patch").min_size(egui::vec2(110.0, 28.0)));
            if patch.clicked() {
                self.start_patch(ctx);
            }
            self.render_log_toggle_and_status(ui);
        });
    }

    /// The Legacy checkbox (shared by the Patcher tab and the Stage builder).
    fn render_legacy_checkbox(&mut self, ui: &mut egui::Ui) {
        let running = self.is_running();
        let mut legacy = self.cfg.legacy_replace_all;
        let legacy_box = ui
            .add_enabled(!running, egui::Checkbox::new(&mut legacy, "Legacy ~ blocks: replace every match, for mods without a # Legacy header"))
            .on_hover_text(
                "Default for the old ~ / ~~ blocks of mods that have no '# Legacy:' header line. On: they are                  applied before everything else and each one replaces every match in the file, like KittyPatcher                  0.1.5c did. Off: they replace only the first match, in load order, like the other blocks. A mod can                  decide for itself with '# Legacy: all' or '# Legacy: first' in its header. Replace: blocks are not                  affected. Replacing every match can change text that a later block needs; the log then names the                  block that did it.",
            );
        if legacy_box.changed() {
            self.cfg.legacy_replace_all = legacy;
            save_config(&self.cfg);
        }
    }

    // ---- Modding tools tab

    fn render_tools(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tools_mode, ToolsMode::ExtractCompile, "Extract / compile");
            ui.selectable_value(&mut self.tools_mode, ToolsMode::Stages, "Stage builder");
        });
        ui.add_space(4.0);
        match self.tools_mode {
            ToolsMode::ExtractCompile => self.render_extract_compile(ui, ctx),
            ToolsMode::Stages => self.render_stages(ui, ctx),
        }
    }

    // ---- Stage builder

    /// Add something dropped on the window (or chosen with a button) to the stage list.
    fn stage_drop(&mut self, path: PathBuf) {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
        if path.is_dir() || ["mod", "kdiff", "patch"].contains(&ext.as_str()) {
            self.cfg.pipeline.push(stages::Item::Mods(path));
        } else if ext == "html" || ext == "htm" {
            // the game HTML is always the first item: it replaces the current one
            match self.cfg.pipeline.first_mut() {
                Some(stages::Item::Input(p)) => *p = path,
                _ => self.cfg.pipeline.insert(0, stages::Item::Input(path)),
            }
        } else {
            return;
        }
        save_config(&self.cfg);
    }

    fn start_stages(&mut self, ctx: &egui::Context) {
        let Ok(plan) = stages::plan(&self.cfg.pipeline) else { return };
        self.auto_scroll = true;
        let legacy = self.cfg.legacy_replace_all;
        spawn_task(self.shared.clone(), ctx.clone(), "Running stages", false, move |r| stages::run(&plan, legacy, r));
    }

    fn render_stages(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use stages::Item;
        let running = self.is_running();
        let border = ui.visuals().widgets.noninteractive.bg_stroke;

        // buttons that add items (items can also be dropped on the window)
        let (mut add_input, mut add_files, mut add_folder, mut add_output, mut clear) = (false, false, false, false, false);
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!running, |ui| {
                add_input = ui.button("Add input HTML\u{2026}").clicked();
                add_files = ui.button("Add mod files\u{2026}").clicked();
                add_folder = ui.button("Add mods folder\u{2026}").clicked();
                add_output = ui.button("Add output HTML\u{2026}").clicked();
                clear = ui.button("Clear list").clicked();
            });
        });
        if add_input {
            if let Some(p) = rfd::FileDialog::new().add_filter("HTML", &["html", "htm"]).pick_file() {
                self.stage_drop(p);
            }
        }
        if add_files {
            if let Some(files) = rfd::FileDialog::new().add_filter("Mod files", &["mod", "kdiff", "patch"]).pick_files() {
                for f in files {
                    self.cfg.pipeline.push(Item::Mods(f));
                }
                save_config(&self.cfg);
            }
        }
        if add_folder {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                self.cfg.pipeline.push(Item::Mods(p));
                save_config(&self.cfg);
            }
        }
        if add_output {
            let n = self.cfg.pipeline.iter().filter(|i| matches!(i, Item::Output(_))).count() + 1;
            let mut dlg = rfd::FileDialog::new().add_filter("HTML", &["html"]).set_file_name(format!("stage{n}.html"));
            if let Some(Item::Input(input)) = self.cfg.pipeline.first() {
                if let Some(dir) = input.parent() {
                    dlg = dlg.set_directory(dir);
                }
            }
            if let Some(p) = dlg.save_file() {
                self.cfg.pipeline.push(Item::Output(p));
                save_config(&self.cfg);
            }
        }
        if clear {
            self.cfg.pipeline.clear();
            save_config(&self.cfg);
        }

        // the list: top to bottom is the order things run in
        let mut action: Option<(usize, u8)> = None; // (row, 0 = up, 1 = down, 2 = remove)
        let len = self.cfg.pipeline.len();
        egui::Frame::new().fill(egui::Color32::BLACK).stroke(border).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(190.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if len == 0 {
                    ui.weak("Drop the game .html, mod files and mod folders here, in the order they should run, or use the buttons above.");
                }
                for (i, item) in self.cfg.pipeline.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!running, |ui| {
                            if ui.add_enabled(i > 0, egui::Button::new("Up").small()).clicked() {
                                action = Some((i, 0));
                            }
                            if ui.add_enabled(i + 1 < len, egui::Button::new("Down").small()).clicked() {
                                action = Some((i, 1));
                            }
                            if ui.add(egui::Button::new("Remove").small()).clicked() {
                                action = Some((i, 2));
                            }
                        });
                        ui.label(format!("{:>2}.", i + 1));
                        let (label, color) = match item {
                            Item::Input(_) => ("Input HTML", egui::Color32::from_rgb(120, 160, 230)),
                            Item::Mods(p) if p.is_dir() => ("Mods folder", egui::Color32::from_gray(200)),
                            Item::Mods(_) => ("Mod file", egui::Color32::from_gray(200)),
                            Item::Output(_) => ("Output HTML", egui::Color32::from_rgb(110, 200, 120)),
                        };
                        ui.add_sized([92.0, 18.0], egui::Label::new(egui::RichText::new(label).color(color).strong()));
                        ui.add(egui::Label::new(egui::RichText::new(item.path().display().to_string()).monospace()).truncate());
                    });
                }
            });
        });
        if let Some((i, what)) = action {
            match what {
                0 if i > 0 => self.cfg.pipeline.swap(i, i - 1),
                1 if i + 1 < self.cfg.pipeline.len() => self.cfg.pipeline.swap(i, i + 1),
                2 => {
                    self.cfg.pipeline.remove(i);
                }
                _ => {}
            }
            save_config(&self.cfg);
        }

        // is the list runnable?
        let plan = stages::plan(&self.cfg.pipeline);
        let input_missing = match self.cfg.pipeline.first() {
            Some(Item::Input(p)) => !p.is_file(),
            _ => false,
        };
        match &plan {
            Ok(_) if input_missing => {
                ui.colored_label(egui::Color32::from_rgb(230, 170, 90), "The input HTML does not exist (any more).");
            }
            Ok(p) => {
                ui.weak(format!(
                    "{} stage(s). Mods between two outputs run as one patch, in list order (a folder: # Priority, then name). Each output is written the way ScRepacker writes it and is the input of the next stage.",
                    p.stages.len()
                ));
            }
            Err(msg) if len > 0 => {
                ui.colored_label(egui::Color32::from_rgb(230, 170, 90), msg);
            }
            Err(_) => {}
        }

        self.render_legacy_checkbox(ui);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let ok = plan.is_ok() && !input_missing && !running;
            if ui.add_enabled(ok, egui::Button::new("Run stages").min_size(egui::vec2(110.0, 28.0))).clicked() {
                self.start_stages(ctx);
            }
            self.render_log_toggle_and_status(ui);
        });
    }

    fn render_extract_compile(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let running = self.is_running();
        let box_w = Self::path_box_width(ui);
        let src = self.cfg.html.as_deref().and_then(source_dir);
        let compiled = self.cfg.html.as_deref().and_then(compiled_path);
        let mut pick_html = false;

        egui::Grid::new("tools-paths").num_columns(3).spacing([8.0, 8.0]).show(ui, |ui| {
            pick_html = path_row(ui, "Game HTML", self.cfg.html.as_deref(), "drop or choose the game .html", box_w, Some(("Browse…", !running)));
            path_row(ui, "Source folder", src.as_deref(), "src folder next to the game HTML", box_w, None);
            path_row(ui, "Compiled HTML", compiled.as_deref(), "", box_w, None);
        });
        ui.weak("Extract writes the editable sources to the src folder; Compile builds the HTML from them again, using the game HTML as the reference.");
        if pick_html {
            if let Some(p) = rfd::FileDialog::new().add_filter("HTML", &["html", "htm"]).pick_file() {
                self.set_html(p);
            }
        }

        let have_html = self.html_is_file();
        let src_exists = src.as_deref().is_some_and(Path::is_dir);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let size = egui::vec2(110.0, 28.0);
            if ui.add_enabled(have_html && !running, egui::Button::new("Extract source").min_size(size)).clicked() {
                if src_exists {
                    self.confirm_extract = true;
                } else {
                    self.start_extract(ctx);
                }
            }
            if ui.add_enabled(have_html && src_exists && !running, egui::Button::new("Compile HTML").min_size(size)).clicked() {
                self.start_compile(ctx);
            }
            if ui.add_enabled(src_exists, egui::Button::new("Open source folder")).clicked() {
                if let Some(dir) = &src {
                    open_folder(dir);
                }
            }
            self.render_log_toggle_and_status(ui);
        });

        if self.confirm_extract {
            let mut go = false;
            let mut cancel = false;
            egui::Window::new("Extract source")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("The source folder already exists:");
                    if let Some(dir) = &src {
                        ui.monospace(dir.display().to_string());
                    }
                    ui.label("Extracting again replaces its files, so changes you made there are lost.");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        go = ui.button("Extract again").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if go {
                self.confirm_extract = false;
                self.start_extract(ctx);
            } else if cancel {
                self.confirm_extract = false;
            }
        }
    }

    // ---- About tab

    fn render_about(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            titled_box(ui, "About", TITLE_GREY, &mut |ui| {
                ui.horizontal(|ui| {
                    if let Some(tex) = &self.icon {
                        ui.add(egui::Image::new((tex.id(), egui::vec2(56.0, 56.0))));
                    }
                    ui.vertical(|ui| {
                        ui.heading(format!("M0on's patcher {}", app_version()));
                        ui.label("A mod patcher for Course of Temptation (SugarCube / Twine).");
                    });
                });
                ui.add_space(6.0);
                ui.label(
                    "Applies .mod files (KittyPatcher format) to the game HTML with \"kitty-engine\", a Rust \
                     reimplementation of the m0on-kittypatcher v0.7.6 fork. No Python and no separate \
                     KittyPatcher are needed.",
                );
                ui.label(
                    "The Modding tools tab extracts the game HTML into editable sources and compiles them \
                     back into an HTML, using ScRepacker's extractor and merger.",
                );
                ui.horizontal_wrapped(|ui| {
                    ui.label("Source and releases:");
                    ui.hyperlink_to(REPO_URL, REPO_URL);
                });

                ui.add_space(10.0);
                ui.strong("Copyright");
                ui.label("Original parts: Copyright (c) 2026 m0on. All rights reserved.");
                ui.label(
                    "ScRepacker and KittyPatcher are published without a licence: their authors keep all \
                     rights in their work, and no licence for it is granted here. Credit is not a licence.",
                );

                ui.add_space(10.0);
                ui.strong("Credits");
                ui.label("\u{2022} LK / kitty: original creator of KittyPatcher (0.1.5c and 0.2.0a)");
                ui.label("\u{2022} CoderMaverick: author of KittyPatcher v0.4.0, v0.6.0 and the v0.7.x versions, and of the KittyPatcher ecosystem");
                ui.horizontal_wrapped(|ui| {
                    ui.label("\u{2022} nuqqur: ScRepacker, whose extractor, merger and window design are used here");
                    ui.hyperlink_to("(screpacker-rs)", "https://gitgud.io/nuqqur/screpacker-rs/");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("\u{2022} Icon: \"shield-moon\" from Material Design Icons by Pictogrammers (Apache 2.0)");
                    ui.hyperlink_to("(pictogrammers.com)", "https://pictogrammers.com/library/mdi/icon/shield-moon/");
                });
                ui.label("\u{2022} The Course of Temptation modding community");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Built with Rust, egui/eframe, regex and other open-source crates.").small());
            });
        });
    }

    // ---- shared: log / progress area

    fn render_log_toggle_and_status(&mut self, ui: &mut egui::Ui) {
        let mut show = self.cfg.show_log;
        if ui.checkbox(&mut show, "Show log").changed() {
            self.cfg.show_log = show;
            save_config(&self.cfg);
        }
        let (status, task) = {
            let s = self.shared.lock().unwrap();
            (s.status.clone(), s.task.clone())
        };
        match status {
            Status::Idle => {}
            Status::Running => {
                ui.label(format!("{task}…"));
            }
            Status::Done(_) => {
                ui.colored_label(egui::Color32::from_rgb(110, 200, 120), "Done");
            }
            Status::Failed(_) => {
                ui.colored_label(egui::Color32::from_rgb(230, 100, 100), "Failed");
            }
        }
    }

    fn render_progress(&mut self, ui: &mut egui::Ui) {
        let (progress, indeterminate, status) = {
            let s = self.shared.lock().unwrap();
            (s.progress, s.indeterminate, s.status.clone())
        };
        if status == Status::Idle {
            return;
        }
        ui.add_space(6.0);
        if indeterminate {
            if status == Status::Running {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label("Working…");
                });
            }
        } else {
            ui.add(egui::ProgressBar::new(progress).show_percentage().animate(status == Status::Running));
        }
        match status {
            Status::Done(msg) => {
                ui.colored_label(egui::Color32::from_rgb(110, 200, 120), msg);
            }
            Status::Failed(msg) => {
                ui.colored_label(egui::Color32::from_rgb(230, 100, 100), msg);
                ui.weak("Show the log for the details.");
            }
            _ => {}
        }
    }

    fn render_log(&mut self, ui: &mut egui::Ui) {
        let border = ui.visuals().widgets.noninteractive.bg_stroke;
        let shared = self.shared.clone();
        let mut clear = false;
        let mut auto_scroll = self.auto_scroll;

        titled_box(ui, "Output", TITLE_GREY, &mut |ui| {
            ui.horizontal(|ui| {
                ui.label("Log");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        clear = true;
                    }
                    ui.checkbox(&mut auto_scroll, "Auto-scroll");
                });
            });

            let (progress, indeterminate) = {
                let s = shared.lock().unwrap();
                (s.progress, s.indeterminate)
            };
            if !indeterminate {
                ui.add(egui::ProgressBar::new(progress).desired_height(6.0));
            }

            let guard = shared.lock().unwrap();
            let rows = guard.log.len();
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y;
            let stick = auto_scroll;
            let scroll = egui::Frame::new()
                .fill(egui::Color32::BLACK)
                .stroke(border)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false; 2])
                        .stick_to_bottom(stick)
                        .show_rows(ui, row_h, rows, |ui, range| {
                            ui.set_min_width(ui.available_width());
                            for i in range {
                                ui.monospace(&guard.log[i]);
                            }
                        })
                })
                .inner;
            // re-enable auto-scroll at the bottom, drop it as soon as the user scrolls up (same as the packer)
            let at_bottom = scroll.state.offset.y + scroll.inner_rect.height() >= scroll.content_size.y - 4.0;
            if at_bottom {
                auto_scroll = true;
            } else if scroll.content_size.y > scroll.inner_rect.height() {
                auto_scroll = false;
            }
        });

        self.auto_scroll = auto_scroll;
        if clear {
            self.shared.lock().unwrap().log.clear();
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_drops(&ctx);

        // resize the window when the log is shown or hidden
        if self.cfg.show_log != self.sized_for_log {
            self.sized_for_log = self.cfg.show_log;
            let size = if self.cfg.show_log { SIZE_WITH_LOG } else { SIZE_COMPACT };
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(size[0], size[1])));
        }

        egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Patcher, "Patcher");
                ui.selectable_value(&mut self.tab, Tab::Tools, "Modding tools");
                ui.selectable_value(&mut self.tab, Tab::About, "About");
            });
            ui.separator();

            match self.tab {
                Tab::Patcher | Tab::Tools => {
                    if self.tab == Tab::Patcher {
                        self.render_patcher(ui, &ctx);
                    } else {
                        self.render_tools(ui, &ctx);
                    }
                    ui.separator();
                    if self.cfg.show_log {
                        self.render_log(ui);
                    } else {
                        self.render_progress(ui);
                    }
                }
                Tab::About => self.render_about(ui),
            }
        });

        if self.is_running() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

// ---------------------------------------------------------------- small helpers

const TITLE_GREY: egui::Color32 = egui::Color32::from_gray(55);
const TITLED_BOX_CORNER: u8 = 6;

/// One row of the path grid: label, read-only path box, optional button (label, enabled). True when clicked.
fn path_row(ui: &mut egui::Ui, label: &str, path: Option<&Path>, hint: &str, width: f32, button: Option<(&str, bool)>) -> bool {
    ui.label(label);
    path_box(ui, path, hint, width);
    let clicked = match button {
        Some((text, enabled)) => ui.add_enabled(enabled, egui::Button::new(text)).clicked(),
        None => {
            ui.label("");
            false
        }
    };
    ui.end_row();
    clicked
}

/// A read-only black box showing a path (or a hint when empty).
fn path_box(ui: &mut egui::Ui, path: Option<&Path>, hint: &str, width: f32) {
    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    egui::Frame::new()
        .fill(egui::Color32::BLACK)
        .stroke(border)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(width);
            match path {
                Some(p) => ui.add(egui::Label::new(egui::RichText::new(p.display().to_string()).monospace()).truncate()),
                None => ui.add(egui::Label::new(egui::RichText::new(hint).weak()).truncate()),
            };
        });
}

/// Titled, rounded, full-width box (same look as the packer's Output box).
fn titled_box(ui: &mut egui::Ui, title: &str, title_fill: egui::Color32, body: &mut dyn FnMut(&mut egui::Ui)) {
    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    let corner = egui::CornerRadius::same(TITLED_BOX_CORNER);
    let title_row_height = ui.spacing().interact_size.y;
    egui::Frame::new().stroke(border).corner_radius(corner).inner_margin(egui::Margin::ZERO).show(ui, |ui| {
        let w = ui.available_width();
        let (title_rect, _) = ui.allocate_exact_size(egui::vec2(w, title_row_height), egui::Sense::hover());
        let rounding = egui::CornerRadius { nw: TITLED_BOX_CORNER, ne: TITLED_BOX_CORNER, sw: 0, se: 0 };
        let painter = ui.painter().clone();
        painter.rect_filled(title_rect, rounding, title_fill);
        painter.hline(title_rect.x_range(), title_rect.max.y, border);
        painter.text(
            title_rect.center(),
            egui::Align2::CENTER_CENTER,
            title,
            egui::FontId::default(),
            ui.visuals().strong_text_color(),
        );
        egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            body(ui);
        });
    });
}

/// Number of mod files the engine will pick up (`.mod`, `.kdiff`, `.patch`; `logs` folders are skipped).
fn count_mod_files(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let path = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if e.file_name() != "logs" {
                n += count_mod_files(&path);
            }
        } else if ft.is_file() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if [".mod", ".kdiff", ".patch"].iter().any(|ext| name.ends_with(ext)) {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod patcher_tests {
    use super::*;

    fn reporter() -> (Reporter, SharedRef) {
        let shared: SharedRef = Arc::new(Mutex::new(Shared {
            log: Vec::new(),
            progress: 0.0,
            indeterminate: false,
            task: String::new(),
            status: Status::Idle,
        }));
        (Reporter { shared: shared.clone(), ctx: egui::Context::default(), echo: Echo::Off }, shared)
    }

    #[test]
    fn the_patcher_tab_job_writes_the_output_and_the_logs_next_to_the_html() {
        let dir = std::env::temp_dir().join(format!("m0on-run-patch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        let html = concat!(
            "<html><body><tw-storydata name=\"T\" startnode=\"1\" creator=\"Twine\" creator-version=\"2.6.2\" ifid=\"X\" zoom=\"1\" ",
            "format=\"SugarCube\" format-version=\"2.36.1\" options=\"\" hidden>",
            "<style role=\"stylesheet\" id=\"twine-user-stylesheet\" type=\"text/twine-css\"></style>",
            "<script role=\"script\" id=\"twine-user-script\" type=\"text/twine-javascript\">var a = 1;</script>",
            "<tw-passagedata pid=\"1\" name=\"Start\" tags=\"\" position=\"0,0\" size=\"100,100\">cat sat</tw-passagedata>",
            "</tw-storydata></body></html>"
        );
        std::fs::write(dir.join("g.html"), html).unwrap();
        std::fs::write(dir.join("mods").join("a.mod"), "Replace:\ncat sat\nWith:\ndog sat\n").unwrap();
        let job = PatchJob { html: dir.join("g.html"), mods: dir.join("mods"), output: dir.join("g.modded.html"), legacy_replace_all: false };
        let (r, shared) = reporter();
        let msg = run_patch(&job, &r).unwrap_or_else(|e| panic!("{e:#}\n{:?}", shared.lock().unwrap().log));
        assert!(msg.contains("1 mod file(s)"), "{msg}");
        assert!(std::fs::read_to_string(dir.join("g.modded.html")).unwrap().contains(">dog sat</tw-passagedata>"));
        assert!(dir.join("logs").join("ModPatchLog.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real data: `M0ON_REAL_HTML`, `M0ON_REAL_MODS1` (folder), `M0ON_REAL_OUT` (file). The output must be the same
    /// bytes the command-line engine writes (stage 1: md5 ab6ee3c1...).
    #[test]
    #[ignore]
    fn real_patcher_tab_job() {
        let get = |k: &str| PathBuf::from(std::env::var(k).unwrap_or_else(|_| panic!("set {k}")));
        let job = PatchJob { html: get("M0ON_REAL_HTML"), mods: get("M0ON_REAL_MODS1"), output: get("M0ON_REAL_OUT"), legacy_replace_all: false };
        let (r, _shared) = reporter();
        println!("{}", run_patch(&job, &r).unwrap());
    }
}
