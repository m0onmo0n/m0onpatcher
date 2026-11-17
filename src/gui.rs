use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use eframe::egui;
use log::{LevelFilter, Log, Metadata, Record};

use crate::config::Config;
use crate::logs;
use crate::mod_converter::{ConverterModIndex, ConverterVersionIndex};
use crate::tool_paths::{self, ToolPath};

/// Entry point when the binary is invoked with no arguments. Sets up a
/// log sink that forwards to the GUI text box (and also to stderr, so a
/// terminal invocation still sees output) and opens the window.
pub fn run(initial_html: Option<PathBuf>, initial_config: Config) -> anyhow::Result<()> {
    let buffer = LogBuffer::default();
    // File-logging is best-effort: if the logs dir can't be created we
    // still want the GUI to come up with a working in-memory log.
    let file_sink = match logs::open_log_file() {
        Ok((_, file)) => Some(logs::FileSink::start(file)),
        Err(err) => {
            eprintln!("warning: file logging disabled: {err:#}");
            None
        }
    };
    // Shared Debug toggle — flipped by the UI checkbox. The file always
    // gets every record; this flag decides whether Debug/Trace records
    // also surface in the on-screen log. A global max_level of Debug
    // lets those records reach our logger in the first place; our
    // `enabled()` filter still drops foreign targets to avoid winit /
    // eframe per-frame flood.
    let show_debug = Arc::new(AtomicBool::new(false));
    let logger = Box::new(GuiLogger {
        buffer: buffer.clone(),
        file: file_sink,
        show_debug: show_debug.clone(),
    });
    log::set_boxed_logger(logger).map_err(|e| anyhow!("log init failed: {e}"))?;
    log::set_max_level(LevelFilter::Debug);

    // winit's Wayland backend doesn't implement DnD, so Dolphin (and other
    // file managers) refuse to drop onto our window under Wayland. Force
    // the X11 backend — XWayland handles DnD transparently, and on pure
    // X11 sessions this is a no-op. Windows/macOS aren't affected by the
    // cfg gate since the X11 trait only exists on Unix.
    #[cfg(all(unix, not(target_os = "macos")))]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }));
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = None;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 650.0])
            .with_min_inner_size([500.0, 350.0]),
        event_loop_builder,
        ..Default::default()
    };

    let title = format!("M0on's packer & patcher {}", app_version());
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                buffer,
                show_debug,
                initial_html,
                initial_config,
            )))
        }),
    )
    .map_err(|e| anyhow!("eframe error: {e}"))
}

#[derive(Clone, Default)]
struct LogBuffer {
    inner: Arc<Mutex<Vec<String>>>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl LogBuffer {
    fn push(&self, line: String) {
        self.inner.lock().unwrap().push(line);
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.inner.lock().unwrap().clone()
    }

    fn set_ctx(&self, ctx: egui::Context) {
        *self.ctx.lock().unwrap() = Some(ctx);
    }
}

struct GuiLogger {
    buffer: LogBuffer,
    /// Optional file sink — None when the logs dir couldn't be created
    /// at startup. Lines are handed off to a background worker thread
    /// that batches writes and flushes on idle (see logs::FileSink).
    file: Option<logs::FileSink>,
    /// When false, Debug/Trace records are written to the file only and
    /// hidden from the on-screen log. Flipped by the GUI's Debug toggle.
    show_debug: Arc<AtomicBool>,
}

impl GuiLogger {
    /// Only accept records from our own crate. Switching the global level
    /// filter to `Debug` for the GUI would otherwise let winit/eframe flood
    /// the log with per-frame events — and since each push requests a
    /// repaint, the flood becomes a feedback loop that stalls the UI.
    fn is_own_target(target: &str) -> bool {
        target == "m0on_packer" || target.starts_with("m0on_packer::")
    }
}

impl Log for GuiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        Self::is_own_target(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !Self::is_own_target(record.target()) {
            return;
        }
        let line = format!("[{}] {}", record.level(), record.args());
        // File always gets every record so the on-disk trail stays
        // complete regardless of the UI debug toggle. Prepend a
        // per-line timestamp; the in-UI buffer keeps the original
        // (untimestamped) text so the on-screen layout stays compact.
        if let Some(file) = &self.file {
            file.write_line(format!(
                "{} {}",
                logs::format_log_line_timestamp(),
                line
            ));
        }
        let show_on_screen = record.level() <= log::Level::Info
            || self.show_debug.load(Ordering::Relaxed);
        if show_on_screen {
            eprintln!("{line}");
            self.buffer.push(line);
        }
    }

    fn flush(&self) {}
}

// `KittyStatus`, `ModListState`, and the modding-tab rendering /
// action methods all live in `gui_modding`. We re-export the type so
// the App struct field can name it without a path.
pub(crate) use crate::gui_modding::ModListState;

pub(crate) struct App {
    pub(crate) html_path: Option<PathBuf>,
    pub(crate) buffer: LogBuffer,
    pub(crate) running: Arc<Mutex<Option<String>>>,
    pub(crate) auto_scroll: bool,
    pub(crate) debug_logs: bool,
    /// Shared with `GuiLogger` — toggling this changes what shows up in
    /// the on-screen log while the file keeps receiving everything.
    pub(crate) show_debug: Arc<AtomicBool>,
    /// Shared flag flipped by the Stop button. Livemerge's watch loop and
    /// WebSocket accept thread both poll this and exit when it's true.
    pub(crate) livemerge_cancel: Arc<AtomicBool>,
    /// Per-command last-output paths. Split so each command's "Open" button
    /// reflects only that command's target and the state doesn't get
    /// clobbered across tasks.
    pub(crate) merge_path: Arc<Mutex<Option<PathBuf>>>,
    pub(crate) patch_path: Arc<Mutex<Option<PathBuf>>>,
    pub(crate) livemerge_path: Arc<Mutex<Option<PathBuf>>>,
    /// Tool paths loaded from `<exe_dir>/screpacker-config.json` at startup
    /// and written back whenever the user changes one through the Config panel.
    pub(crate) config: Config,
    /// Cached `tool_paths::*` results. Recomputed only at startup, when
    /// the HTML selection changes, or when config changes — never on every
    /// UI frame, since resolving involves disk scans.
    pub(crate) python_tool: ToolPath,
    pub(crate) kittypatcher_tool: ToolPath,
    /// Cached mods-directory snapshot shown under the Patch buttons.
    /// Same refresh cadence as the tool paths — any edit to HTML or
    /// config triggers a recompute.
    pub(crate) mod_list: ModListState,
    /// Top-level tab selector. Output / log section is rendered in
    /// every tab; tab-specific sections live above it.
    pub(crate) tab: Tab,
    /// Base directory the Mod-converter tab operates on. Distinct from
    /// `html_path`: the converter walks version-specific subdirectories
    /// rather than a single HTML file.
    pub(crate) converter_directory: Option<PathBuf>,
    /// In-memory snapshot of the converter's known game versions.
    /// Populated by `ConverterVersionIndex::scan_versions` on first
    /// tab open and refreshed whenever the base directory changes.
    pub(crate) converter_version_index: ConverterVersionIndex,
    /// In-memory snapshot of every `.mod` / `.diff` file living in
    /// the converter's `from-v<version>/` subdirectories. Refreshed
    /// alongside the version index whenever the base directory is set
    /// or after a successful Mods import.
    pub(crate) converter_mod_index: ConverterModIndex,
    /// Tracks whether the converter tab has run its initial scan; we
    /// defer the first scan until the user actually opens the tab so
    /// startup time isn't paying for a directory walk that may go
    /// unused.
    pub(crate) converter_index_initialized: bool,
    /// Pending Mod-converter import dialogs. Each zip drop pushes one
    /// entry to the back; the front entry is the active modal. Add
    /// (on success) or Cancel pops it, exposing the next queued one.
    /// Multiple zips dropped in one gesture get walked through in
    /// drop order rather than silently overwriting each other.
    pub(crate) converter_import_dialogs: VecDeque<ConverterImportDialog>,
    /// Set when the user clicks "Add all" on a Mods import dialog —
    /// every subsequent queued Mods dialog auto-commits with this
    /// version instead of prompting again. Cleared the moment the
    /// dialog queue runs dry, so a fresh batch always starts in
    /// manual mode. GameVersion (HTML) imports ignore this and always
    /// prompt.
    pub(crate) converter_import_auto_version: Option<String>,
    /// Human-readable rollup of "what's wrong with the converter
    /// state right now" — empty when everything's fine. Rebuilt by
    /// `App::finalize_converter_indexes` whenever an index changes.
    pub(crate) converter_status: String,
    /// Cooperative cancel flag for the Mod-converter Start/Stop
    /// buttons. The worker thread reads it between mods. A fresh
    /// instance is allocated for each Start so that a cancel from a
    /// previous run doesn't poison the next.
    pub(crate) convert_cancel: Arc<AtomicBool>,
    /// Trip-flag set by the converter worker when it exits, polled
    /// by `render_mod_converter` so the next frame can re-scan and
    /// surface the new `to-v*` contents.
    pub(crate) convert_finished: Arc<AtomicBool>,
    /// Last conversion's outcome, written by the worker thread
    /// before it sets `convert_finished`. Polled and consumed by the
    /// GUI to flip Failed-mod statuses in the index ahead of the
    /// follow-up `scan_mods`.
    pub(crate) convert_outcome:
        Arc<Mutex<Option<crate::mod_converter::ConvertOutcome>>>,
    /// Active "edit a command + args" modal, if any. The Edit button
    /// in the Config tab opens this; OK persists into the config
    /// field indicated by `target`, Cancel discards.
    pub(crate) command_edit_dialog: Option<crate::gui_config::CommandEditDialog>,
    /// Active "change game version of a mod" modal, if any. Opened
    /// by the pencil icon in the Mod-converter mods list; commits
    /// either move (Change) or remove (Delete) the underlying file
    /// and re-scan the mods index.
    pub(crate) change_version_dialog:
        Option<crate::gui_mod_converter::ChangeVersionDialog>,
    /// Cached existence flag for `<context>/src/` (the Modding tab's
    /// extracted-source directory). Populated by `set_html_path`,
    /// `App::new`, and on every spawn_task* completion via
    /// `task_finished` — we don't want to stat the disk every UI
    /// frame.
    pub(crate) src_dir_exists: bool,
    /// One-shot flag set by every spawn_task* worker thread when it
    /// exits, polled by the modding tab's render fn so the cached
    /// `src_dir_exists` (and any other completion-derived state) can
    /// be refreshed without touching the disk on every frame.
    pub(crate) task_finished: Arc<AtomicBool>,
}

// `ConverterImportDialog` and `ConverterImportKind` live in
// `gui_mod_converter` since they're only consumed by that tab's
// rendering and drop logic.
pub(crate) use crate::gui_mod_converter::ConverterImportDialog;

/// Version shown in the title bar and the About tab: the crate version without a trailing ".0" (0.1.0 -> 0.1).
pub(crate) fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION").trim_end_matches(".0")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Modding,
    ModConverter,
    Config,
    About,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        buffer: LogBuffer,
        show_debug: Arc<AtomicBool>,
        initial_html: Option<PathBuf>,
        initial_config: Config,
    ) -> Self {
        buffer.set_ctx(cc.egui_ctx.clone());
        // Lock the theme to dark so eframe doesn't swap in
        // Visuals::light() on a light-mode system — the UI mixes in a
        // few hardcoded colours (near-black frames, muted kitty-status
        // tags) that only match the dark palette.
        cc.egui_ctx
            .options_mut(|opt| opt.theme_preference = egui::ThemePreference::Dark);
        let mut visuals = egui::Visuals::dark();
        // Default panel_fill is gray(27) ≈ #1B1B1B. Bump it up a bit so the
        // black log/path frames below stand out against the chrome.
        visuals.panel_fill = egui::Color32::from_gray(27);
        cc.egui_ctx.set_visuals(visuals);
        let initial = initial_html.or_else(crate::gui_modding::detect_starting_html);
        let (merge, patch) = crate::gui_modding::detect_existing_outputs(initial.as_deref());
        let src_dir_exists = initial
            .as_deref()
            .and_then(crate::context_paths::context_locate_source)
            .map(|d| d.is_dir())
            .unwrap_or(false);
        let python_tool = tool_paths::tool_locate_python(&initial_config);
        let kittypatcher_tool =
            tool_paths::tool_locate_kittypatcher(&initial_config, initial.as_deref());
        let mod_list = ModListState::compute(initial.as_deref(), &initial_config);
        Self {
            html_path: initial,
            buffer,
            running: Arc::new(Mutex::new(None)),
            auto_scroll: true,
            debug_logs: false,
            show_debug,
            livemerge_cancel: Arc::new(AtomicBool::new(false)),
            merge_path: Arc::new(Mutex::new(merge)),
            patch_path: Arc::new(Mutex::new(patch)),
            livemerge_path: Arc::new(Mutex::new(None)),
            config: initial_config,
            python_tool,
            kittypatcher_tool,
            mod_list,
            tab: Tab::Modding,
            converter_directory: None,
            converter_version_index: ConverterVersionIndex::new(),
            converter_mod_index: ConverterModIndex::new(),
            converter_index_initialized: false,
            converter_import_dialogs: VecDeque::new(),
            converter_import_auto_version: None,
            converter_status: String::new(),
            convert_cancel: Arc::new(AtomicBool::new(false)),
            convert_finished: Arc::new(AtomicBool::new(false)),
            convert_outcome: Arc::new(Mutex::new(None)),
            command_edit_dialog: None,
            change_version_dialog: None,
            src_dir_exists,
            task_finished: Arc::new(AtomicBool::new(false)),
        }
    }


    fn clear_log(&self) {
        self.buffer.inner.lock().unwrap().clear();
    }

    /// Toggle whether Debug/Trace records show up in the on-screen log.
    /// The global log filter stays at Debug and the file keeps receiving
    /// everything — this only flips the shared flag that `GuiLogger`
    /// consults before pushing a record to the UI buffer.
    fn set_debug_logs(&mut self, enabled: bool) {
        self.debug_logs = enabled;
        self.show_debug.store(enabled, Ordering::Relaxed);
    }
}

impl eframe::App for App {
    /// eframe 0.34's default `clear_color` returns a hard-coded near-black
    /// and ignores `visuals.panel_fill`, so overriding it is the only way
    /// to control the actual window background on native. The black-filled
    /// path and log frames inside `ui()` sit on top of this.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Drag-and-drop files accepted anywhere in the window. Route
        // each one by kind: directory (pick HTML inside), .html (use
        // directly), .mod/.diff (copy into the current project's mods
        // dir), anything else logs a warning. Multiple mod files dropped
        // together are all installed in order.
        let dropped_paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|d| d.path.clone())
                .collect()
        });
        for path in dropped_paths {
            match self.tab {
                Tab::Modding => self.handle_modding_drop(&path),
                Tab::ModConverter => self.handle_converter_drop(&path),
                Tab::Config => self.handle_config_drop(&path),
                Tab::About => {}
            }
        }

        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
            // Use the same stroke the separator widget picks up so the
            // frame borders line up visually with the <hr>-style divider
            // between the buttons and the log.
            // Tab selector. Output (log) is always shown below; the
            // chosen tab decides what gets rendered above it.
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Modding, "Modding");
                ui.selectable_value(&mut self.tab, Tab::ModConverter, "Mod converter");
                ui.selectable_value(&mut self.tab, Tab::Config, "Config");
                ui.selectable_value(&mut self.tab, Tab::About, "About");
            });
            ui.separator();

            match self.tab {
                Tab::Modding => self.render_modding(ui),
                Tab::ModConverter => self.render_mod_converter(ui),
                Tab::Config => self.render_config(ui),
                Tab::About => self.render_about(ui),
            }

            self.render_output(ui);
            // The import-version dialog (Mod converter tab) is an
            // egui::Window; it's drawn here so it overlays whichever
            // tab is currently active and persists across tab switches
            // until Add/Cancel.
            self.render_converter_import_dialog(&ctx);
            self.render_command_edit_dialog(&ctx);
            self.render_change_version_dialog(&ctx);
            // Force periodic repaints while a task is running so log
            // lines surface even without user input.
            if self.is_running() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
    }
}

impl App {
    fn render_output(&mut self, ui: &mut egui::Ui) {
        let border = ui.visuals().widgets.noninteractive.bg_stroke;

        titled_box(ui, "Output", TITLE_GREY, &mut |ui| {
            ui.horizontal(|ui| {
                ui.label("Log");
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.clear_log();
                        }
                        let mut debug_logs = self.debug_logs;
                        if ui.checkbox(&mut debug_logs, "Debug").changed() {
                            self.set_debug_logs(debug_logs);
                        }
                        ui.checkbox(&mut self.auto_scroll, "Auto-scroll");
                    },
                );
            });

            let lines = self.buffer.snapshot();
            let stick = self.auto_scroll;
            let scroll = egui::Frame::new()
                .fill(egui::Color32::BLACK)
                .stroke(border)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false; 2])
                        .stick_to_bottom(stick)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            for line in &lines {
                                ui.monospace(line);
                            }
                        })
                })
                .inner;

            // Re-enable auto-scroll when the user reaches the bottom
            // manually; disable it as soon as they scroll up.
            // `content_size.y` grows as new lines arrive, so the
            // comparison is stable across frames.
            let at_bottom = scroll.state.offset.y + scroll.inner_rect.height()
                >= scroll.content_size.y - 4.0;
            if at_bottom {
                self.auto_scroll = true;
            } else if scroll.content_size.y > scroll.inner_rect.height() {
                // Only consider the user "scrolled up" once there's
                // enough content that scrolling is even meaningful.
                self.auto_scroll = false;
            }
        });
    }
}

// Title-bar background colours shared by all titled boxes. Kept
// low-saturation so they work against the dark chrome without clashing
// with the rest of the UI.
pub(crate) const TITLE_GREY: egui::Color32 = egui::Color32::from_gray(55);
pub(crate) const TITLE_LEFT: egui::Color32 = egui::Color32::from_rgb(55, 65, 85);
pub(crate) const TITLE_RIGHT: egui::Color32 = egui::Color32::from_rgb(85, 55, 55);

pub(crate) const TITLED_BOX_CORNER: u8 = 6;

/// Paint a title bar at `title_rect`. The hosting Frame's rounded
/// corners meet the title at its top, so only the top two corners of
/// the title fill are rounded; a thin divider line under the title
/// separates it from the content below.
pub(crate) fn paint_title(ui: &mut egui::Ui, title_rect: egui::Rect, fill: egui::Color32, text: &str) {
    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    let painter = ui.painter().clone();
    let title_rounding = egui::CornerRadius {
        nw: TITLED_BOX_CORNER,
        ne: TITLED_BOX_CORNER,
        sw: 0,
        se: 0,
    };
    painter.rect_filled(title_rect, title_rounding, fill);
    painter.hline(title_rect.x_range(), title_rect.max.y, border);
    painter.text(
        title_rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::default(),
        ui.visuals().strong_text_color(),
    );
}

/// Render a titled, rounded, full-width outer box. The title bar
/// occupies a fixed strip at the top; `body` runs inside an inner
/// Frame with standard padding so its widgets don't hug the rounded
/// stroke.
pub(crate) fn titled_box(
    ui: &mut egui::Ui,
    title: &str,
    title_fill: egui::Color32,
    body: &mut dyn FnMut(&mut egui::Ui),
) {
    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    let corner = egui::CornerRadius::same(TITLED_BOX_CORNER);
    let title_row_height = ui.spacing().interact_size.y;
    egui::Frame::new()
        .stroke(border)
        .corner_radius(corner)
        .inner_margin(egui::Margin::ZERO)
        .show(ui, |ui| {
            let w = ui.available_width();
            let (title_rect, _) = ui.allocate_exact_size(
                egui::vec2(w, title_row_height),
                egui::Sense::hover(),
            );
            paint_title(ui, title_rect, title_fill, title);
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    body(ui);
                });
        });
}

/// Centre a row of widgets both vertically and horizontally inside
/// `rect`. `scope_builder` with `max_rect` gives the child Ui a
/// pixel-exact region to work in so `Align::Center` can anchor to the
/// cell centre. Shrinks horizontally so the buttons don't flush
/// against the outer-box stroke or the internal divider lines.
pub(crate) fn centred_row(ui: &mut egui::Ui, rect: egui::Rect, content: &mut dyn FnMut(&mut egui::Ui)) {
    const CELL_H_PADDING: f32 = 8.0;
    let padded = rect.shrink2(egui::vec2(CELL_H_PADDING, 0.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(padded), |ui| {
        ui.with_layout(
            egui::Layout::centered_and_justified(egui::Direction::TopDown),
            |ui| {
                ui.horizontal(|ui| content(ui));
            },
        );
    });
}

