//! Modding-tab specific GUI code: render functions, button actions,
//! drop handler, and helper types/free functions used only when the
//! Modding tab is active. Lives alongside `gui.rs`, which keeps the
//! shared App state, common widget helpers, and the Output panel.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use eframe::egui;

use crate::commands;
use crate::context_paths;
use crate::gui::{
    App, TITLED_BOX_CORNER, TITLE_GREY, TITLE_LEFT, TITLE_RIGHT, centred_row, paint_title,
    titled_box,
};
use crate::mod_data_reader;
use crate::mod_error_registry::ModErrorRegistry;
use crate::tool_paths::{self, KittypatcherCommandError};

/// Availability of KittyPatcher given the current config and HTML context.
/// `Unused` means either no `.mod` files are present so the patcher isn't
/// needed, or we haven't even looked yet. The other variants mirror
/// `KittypatcherCommandError` so the UI can pick a targeted message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KittyStatus {
    Unused,
    MissingPatcherPath,
    MissingPythonPath,
    Present,
    /// The internal engine is selected; KittyPatcher is not used.
    Internal,
    /// The internal engine is selected with KittyPatcher as fallback; the flag says whether
    /// a KittyPatcher is currently resolvable.
    InternalFallback { patcher_found: bool },
}

/// Snapshot of the mods directory displayed under the Patch buttons.
/// Recomputed when the HTML selection or config changes (both can shift
/// which mods dir we read from and whether KittyPatcher is resolvable).
#[derive(Debug, Clone)]
pub(crate) struct ModListState {
    pub(crate) mod_count: usize,
    pub(crate) mod_kitty_count: usize,
    pub(crate) mod_error_count: usize,
    pub(crate) kitty_status: KittyStatus,
}

impl ModListState {
    pub(crate) fn empty() -> Self {
        Self {
            mod_count: 0,
            mod_kitty_count: 0,
            mod_error_count: 0,
            kitty_status: KittyStatus::Unused,
        }
    }

    pub(crate) fn compute(html_path: Option<&Path>, config: &crate::config::Config) -> Self {
        let Some(html) = html_path else {
            return Self::empty();
        };
        let Some(mods_dir) = context_paths::context_locate_mods(html) else {
            return Self::empty();
        };
        if !mods_dir.is_dir() {
            return Self::empty();
        }

        let mut registry = ModErrorRegistry::new();
        let mods = match mod_data_reader::read_mods(&mods_dir, &mut registry) {
            Ok(v) => v,
            Err(_) => return Self::empty(),
        };
        let mod_count = mods.len();
        let mod_kitty_count = count_kitty_mod_files(&mods_dir);
        let mod_error_count = registry.failures().len();

        let patcher = tool_paths::tool_command_kittypatcher(config, Some(html));
        let kitty_status = if mod_kitty_count == 0 {
            KittyStatus::Unused
        } else {
            match config.engine_mode() {
                crate::config::EngineMode::Internal => KittyStatus::Internal,
                crate::config::EngineMode::InternalFallback => {
                    KittyStatus::InternalFallback { patcher_found: patcher.is_ok() }
                }
                crate::config::EngineMode::External => match patcher {
                    Ok(_) => KittyStatus::Present,
                    Err(KittypatcherCommandError::MissingPatcherPath) => {
                        KittyStatus::MissingPatcherPath
                    }
                    Err(KittypatcherCommandError::MissingPythonPath) => {
                        KittyStatus::MissingPythonPath
                    }
                },
            }
        };

        Self {
            mod_count,
            mod_kitty_count,
            mod_error_count,
            kitty_status,
        }
    }
}

fn count_kitty_mod_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|s| s.to_ascii_lowercase().ends_with(".mod"))
                .unwrap_or(false)
        })
        .count()
}

impl App {
    /// Recompute cached derived state that depends on HTML selection +
    /// config: tool paths and the mods-directory snapshot.
    pub(crate) fn refresh_tool_paths(&mut self) {
        self.python_tool = tool_paths::tool_locate_python(&self.config);
        self.kittypatcher_tool =
            tool_paths::tool_locate_kittypatcher(&self.config, self.html_path.as_deref());
        self.mod_list = ModListState::compute(self.html_path.as_deref(), &self.config);
    }

    /// Switch the source HTML and re-initialise the Merge/Patch output
    /// paths from whatever exists on disk for that source. Livemerge is
    /// deliberately NOT refreshed here — its output only becomes valid
    /// after a live-merge run actually starts and is re-derived then.
    pub(crate) fn set_html_path(&mut self, path: Option<PathBuf>) {
        let (merge, patch) = detect_existing_outputs(path.as_deref());
        *self.merge_path.lock().unwrap() = merge;
        *self.patch_path.lock().unwrap() = patch;
        if let Some(p) = path.as_deref() {
            let as_string = p.display().to_string();
            if self.config.last_html_path != as_string {
                self.config.last_html_path = as_string;
                if let Err(err) = crate::config::save(&self.config) {
                    log::error!("Failed to persist last HTML path: {err:#}");
                }
            }
        }
        self.html_path = path;
        self.refresh_tool_paths();
        self.recheck_src_dir();
    }

    /// Restore the last-used HTML path from config. Used by the "Use
    /// last" button — only meaningful when nothing is currently
    /// selected (the button is greyed out otherwise).
    pub(crate) fn use_last_html_path(&mut self) {
        let last = self.config.last_html_path.clone();
        if last.is_empty() {
            return;
        }
        let path = PathBuf::from(&last);
        if !path.is_file() {
            log::warn!("Last HTML path no longer exists: {last}");
            return;
        }
        self.set_html_path(Some(path));
    }

    /// Recompute `src_dir_exists` from the current `html_path`. Called
    /// from `set_html_path` and after every spawn_task* completion so
    /// the cached flag never lags behind the on-disk state without
    /// forcing a per-frame stat.
    pub(crate) fn recheck_src_dir(&mut self) {
        self.src_dir_exists = self
            .html_path
            .as_deref()
            .and_then(context_paths::context_locate_source)
            .map(|d| d.is_dir())
            .unwrap_or(false);
    }

    /// Drag-and-drop handler used while the Modding tab is active.
    /// Directories resolve to their sole HTML; .html files set the
    /// HTML path directly; .mod / .diff files are copied into the
    /// current project's mods directory; .zip archives are unpacked
    /// into mods/; a KittyPatcher .py / .exe sets the config entry.
    /// Anything else is rejected with a warning.
    pub(crate) fn handle_modding_drop(&mut self, path: &Path) {
        if path.is_dir() {
            match detect_html_in_dir(path) {
                Some(html) => self.set_html_path(Some(html)),
                None => log::warn!(
                    "Directory selected, but it doesn't contain any matching HTML files"
                ),
            }
            return;
        }

        let lower_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();

        if lower_name.ends_with(".html") || lower_name.ends_with(".htm") {
            self.set_html_path(Some(path.to_path_buf()));
            return;
        }

        if lower_name.ends_with(".mod") || lower_name.ends_with(".diff") {
            self.install_mod_file(path);
            return;
        }

        if lower_name.ends_with(".zip") {
            self.install_zip_archive(path);
            return;
        }

        log::warn!(
            "Drag&drop file {} is neither a directory, HTML file nor a mod file",
            path.display()
        );
    }

    fn install_mod_file(&mut self, mod_file: &Path) {
        let Some(html_path) = self.html_path.clone() else {
            log::warn!("Cannot copy mod file, no HTML selected");
            return;
        };
        let Some(mods_dir) = context_paths::context_locate_mods(&html_path) else {
            log::warn!("Cannot copy mod file, no HTML selected");
            return;
        };
        if !mods_dir.is_dir() {
            if let Err(err) = std::fs::create_dir_all(&mods_dir) {
                log::warn!(
                    "Unable to create {} directory: {err}",
                    mods_dir.display()
                );
                return;
            }
        }

        if let Some(parent) = mod_file.parent() {
            if same_dir(parent, &mods_dir) {
                log::info!("Mod file is already in mods directory");
                return;
            }
        }

        let Some(file_name) = mod_file.file_name() else {
            log::warn!("Failed to {} to mods directory", mod_file.display());
            return;
        };
        let dest = mods_dir.join(file_name);
        match std::fs::copy(mod_file, &dest) {
            Ok(_) => {
                log::info!(
                    "Mod file at {} successfully copied to mods directory",
                    mod_file.display()
                );
                self.mod_list =
                    ModListState::compute(self.html_path.as_deref(), &self.config);
            }
            Err(err) => {
                log::warn!(
                    "Failed to {} to mods directory: {err}",
                    mod_file.display()
                );
            }
        }
    }

    fn install_zip_archive(&mut self, zip_path: &Path) {
        let Some(html_path) = self.html_path.clone() else {
            log::warn!("Cannot extract zip, no HTML selected");
            return;
        };
        let Some(mods_dir) = context_paths::context_locate_mods(&html_path) else {
            log::warn!("Cannot extract zip, no HTML selected");
            return;
        };
        if !mods_dir.is_dir() {
            if let Err(err) = std::fs::create_dir_all(&mods_dir) {
                log::warn!("Unable to create {} directory: {err}", mods_dir.display());
                return;
            }
        }
        let conflicts_dir = mods_dir.join("conflicts");

        let file = match std::fs::File::open(zip_path) {
            Ok(f) => f,
            Err(err) => {
                log::warn!("Failed to open zip {}: {err}", zip_path.display());
                return;
            }
        };
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(err) => {
                log::warn!("Failed to read zip {}: {err}", zip_path.display());
                return;
            }
        };

        if is_unmatched_mod_bundle(&mut archive) {
            let zip_name = zip_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?");
            let dest = mods_dir.join(zip_name);
            match std::fs::copy(zip_path, &dest) {
                Ok(_) => {
                    log::info!("Copied mod zip {zip_name} to mods directory");
                    self.mod_list =
                        ModListState::compute(self.html_path.as_deref(), &self.config);
                }
                Err(err) => {
                    log::warn!(
                        "Failed to copy zip {} to mods directory: {err}",
                        zip_path.display()
                    );
                }
            }
            return;
        }

        let mut diff_count = 0usize;
        let mut mod_count = 0usize;
        let mut resolve_count = 0usize;
        let mut diff_stems: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut conflicts_dir_ready = false;

        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(e) => e,
                Err(err) => {
                    log::warn!("Failed to read zip entry #{i}: {err}");
                    continue;
                }
            };
            if entry.is_dir() {
                continue;
            }
            let Some(entry_path) = entry.enclosed_name() else {
                continue;
            };
            let Some(name) = entry_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
            else {
                continue;
            };
            let lower = name.to_ascii_lowercase();

            if lower.ends_with(".diff") {
                let dest = mods_dir.join(&name);
                if copy_zip_entry(&mut entry, &dest).is_ok() {
                    diff_count += 1;
                    diff_stems.insert(name[..name.len() - ".diff".len()].to_string());
                    log::info!("Copied diff file {name} from zip");
                } else {
                    log::warn!("Failed to copy {name} from zip to mods dir");
                }
            } else if lower.ends_with(".mod") {
                let dest = mods_dir.join(&name);
                if copy_zip_entry(&mut entry, &dest).is_ok() {
                    mod_count += 1;
                    log::info!("Copied mod file {name} from zip");
                } else {
                    log::warn!("Failed to copy {name} from zip to mods dir");
                }
            } else if lower.ends_with(".resolve") {
                if !conflicts_dir_ready {
                    if let Err(err) = std::fs::create_dir_all(&conflicts_dir) {
                        log::warn!(
                            "Unable to create {} directory: {err}",
                            conflicts_dir.display()
                        );
                        return;
                    }
                    conflicts_dir_ready = true;
                }
                let dest = conflicts_dir.join(&name);
                if copy_zip_entry(&mut entry, &dest).is_ok() {
                    resolve_count += 1;
                    log::info!("Copied resolve file {name} from zip");
                } else {
                    log::warn!("Failed to copy {name} from zip to conflicts dir");
                }
            }
        }

        if !diff_stems.is_empty() {
            for i in 0..archive.len() {
                let mut entry = match archive.by_index(i) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if entry.is_dir() {
                    continue;
                }
                let Some(entry_path) = entry.enclosed_name() else {
                    continue;
                };
                let Some(name) = entry_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
                else {
                    continue;
                };
                if !name.to_ascii_lowercase().ends_with(".json") {
                    continue;
                }
                let stem = &name[..name.len() - ".json".len()];
                if !diff_stems.contains(stem) {
                    continue;
                }
                let dest = mods_dir.join(&name);
                if copy_zip_entry(&mut entry, &dest).is_ok() {
                    log::info!("Copied json manifest {name} from zip");
                } else {
                    log::warn!("Failed to copy {name} from zip to mods dir");
                }
            }
        }

        if diff_count == 0 && mod_count == 0 && resolve_count == 0 {
            log::warn!("Zip with no recognized mod files provided");
        } else {
            log::info!(
                "{diff_count} diff files copied, {mod_count} mod files copied, {resolve_count} resolve files copied"
            );
            self.mod_list =
                ModListState::compute(self.html_path.as_deref(), &self.config);
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.lock().unwrap().is_some()
    }

    pub(crate) fn current_task(&self) -> Option<String> {
        self.running.lock().unwrap().clone()
    }

    /// Spawn a background task. Buttons are disabled while anything is
    /// running (via `is_running`) so we don't worry about concurrent tasks.
    fn spawn_task<F>(&mut self, name: &str, task: F)
    where
        F: FnOnce(&Path) -> Result<()> + Send + 'static,
    {
        self.spawn_task_with_output(name, None, task);
    }

    /// Variant used by Merge / Patch: on successful completion, records
    /// the output path into the specified slot so the matching Open button
    /// can launch it. A failed task leaves the previous slot contents alone.
    fn spawn_task_with_output<F>(
        &mut self,
        name: &str,
        on_success: Option<(PathBuf, Arc<Mutex<Option<PathBuf>>>)>,
        task: F,
    ) where
        F: FnOnce(&Path) -> Result<()> + Send + 'static,
    {
        let Some(html_path) = self.html_path.clone() else {
            return;
        };
        let running = self.running.clone();
        *running.lock().unwrap() = Some(name.to_string());
        let name_owned = name.to_string();
        let task_finished = self.task_finished.clone();
        thread::spawn(move || {
            log::info!("--- Starting {name_owned} ---");
            match task(&html_path) {
                Ok(()) => {
                    log::info!("--- {name_owned} finished ---");
                    if let Some((path, slot)) = on_success {
                        *slot.lock().unwrap() = Some(path);
                    }
                }
                Err(err) => log::error!("{name_owned} failed: {err:#}"),
            }
            *running.lock().unwrap() = None;
            task_finished.store(true, Ordering::Relaxed);
        });
    }

    fn spawn_livemerge(&mut self) {
        let Some(html_path) = self.html_path.clone() else {
            return;
        };
        self.livemerge_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.livemerge_cancel.clone();
        let running = self.running.clone();
        *running.lock().unwrap() = Some("Livemerge".to_string());

        if let Ok(output) = commands::derive_output(&html_path, "livemerged") {
            *self.livemerge_path.lock().unwrap() = Some(output);
        }

        let task_finished = self.task_finished.clone();
        thread::spawn(move || {
            log::info!("--- Starting Livemerge ---");
            match commands::run_livemerge_with_cancel(&html_path, cancel) {
                Ok(()) => log::info!("--- Livemerge finished ---"),
                Err(err) => log::error!("Livemerge failed: {err:#}"),
            }
            *running.lock().unwrap() = None;
            task_finished.store(true, Ordering::Relaxed);
        });
    }

    fn stop_livemerge(&self) {
        self.livemerge_cancel.store(true, Ordering::Relaxed);
    }

    fn browse_html(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("HTML", &["html", "htm"])
            .pick_file()
        {
            self.set_html_path(Some(path));
        }
    }

    fn trigger_extract(&mut self) {
        self.spawn_task("Extract", commands::run_extract);
    }

    fn trigger_edit_source(&self) {
        let Some(html_path) = self.html_path.as_deref() else {
            return;
        };
        let Some(src_dir) = context_paths::context_locate_source(html_path) else {
            return;
        };
        crate::tool_execute::open_directory_editor(&self.config, &src_dir);
    }

    fn trigger_diff(&mut self) {
        self.spawn_task("Diff", commands::run_diff);
    }

    fn trigger_merge(&mut self) {
        let on_success = self
            .html_path
            .as_deref()
            .and_then(|p| commands::derive_output(p, "merged").ok())
            .map(|p| (p, self.merge_path.clone()));
        self.spawn_task_with_output("Merge", on_success, commands::run_merge);
    }

    fn trigger_patch(&mut self) {
        let on_success = self
            .html_path
            .as_deref()
            .and_then(|p| commands::derive_output(p, "modded").ok())
            .map(|p| (p, self.patch_path.clone()));
        self.spawn_task_with_output("Patch", on_success, commands::run_patch);
    }

    fn open_in_configured_browser(&self, path: &Path) {
        if let Err(err) =
            crate::tool_execute::open_in_browser(path, &self.config.browser_path)
        {
            log::error!("Failed to open {}: {err:#}", path.display());
        }
    }

    pub(crate) fn render_modding(&mut self, ui: &mut egui::Ui) {
        if self.task_finished.swap(false, Ordering::Relaxed) {
            self.recheck_src_dir();
        }
        let ctx = ui.ctx().clone();
        let border = ui.visuals().widgets.noninteractive.bg_stroke;
        let corner = egui::CornerRadius::same(TITLED_BOX_CORNER);
        let title_row_height = ui.spacing().interact_size.y;

        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        titled_box(ui, "Input", TITLE_GREY, &mut |ui| {
                ui.horizontal(|ui| {
                    ui.label("HTML:");
                    let path_text = if hovering {
                        egui::RichText::new("Drop the HTML file here…")
                            .color(egui::Color32::LIGHT_GREEN)
                    } else if let Some(p) = &self.html_path {
                        egui::RichText::new(p.display().to_string())
                    } else {
                        egui::RichText::new("(drop a file or click Browse…)")
                    };

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button("Browse…").clicked() {
                                self.browse_html();
                            }
                            let use_last_enabled = self.html_path.is_none()
                                && !self.config.last_html_path.is_empty();
                            if ui
                                .add_enabled(use_last_enabled, egui::Button::new("Use last"))
                                .on_hover_text(self.config.last_html_path.as_str())
                                .clicked()
                            {
                                self.use_last_html_path();
                            }
                            let row_height = ui.spacing().interact_size.y;
                            egui::Frame::new()
                                .fill(egui::Color32::BLACK)
                                .stroke(border)
                                .inner_margin(egui::Margin::symmetric(6, 0))
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.set_min_height(row_height);
                                    ui.with_layout(
                                        egui::Layout::left_to_right(
                                            egui::Align::Center,
                                        ),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(path_text).truncate(),
                                            );
                                        },
                                    );
                                });
                        },
                    );
                });

                ui.horizontal(|ui| {
                    if let Some(task) = self.current_task() {
                        ui.add(egui::Spinner::new());
                        ui.label(format!("Running: {task}"));
                    } else if self.html_path.is_none() {
                        ui.label(
                            "No HTML selected - browse or drag&drop a file to enable commands",
                        );
                    } else {
                        ui.label("Idle");
                    }
                });
            });

            let extra_vspace = (ui.spacing().item_spacing.x
                - ui.spacing().item_spacing.y)
                .max(0.0);
            ui.add_space(extra_vspace);

            let enabled = self.html_path.is_some() && !self.is_running();
            let livemerge_running = self.current_task().as_deref() == Some("Livemerge");
            let merge_path_val = self.merge_path.lock().unwrap().clone();
            let patch_path_val = self.patch_path.lock().unwrap().clone();
            let livemerge_path_val = self.livemerge_path.lock().unwrap().clone();

            let inner_cell_padding = 6.0_f32;
            let button_height = ui.spacing().interact_size.y;
            let inner_row_height = button_height + 2.0 * inner_cell_padding;
            let inner_separator = ui.spacing().item_spacing.y.max(2.0);
            let box_content_height = 2.0 * inner_row_height + inner_separator;
            let box_total_height = title_row_height + box_content_height;
            let outer_row_height = box_total_height + 4.0;

            let row_size = egui::vec2(ui.available_width(), outer_row_height);

            let cell_h_padding = 8.0_f32;

            ui.allocate_ui_with_layout(
                row_size,
                egui::Layout::left_to_right(egui::Align::TOP),
                |ui| {
                    let gap = ui.spacing().item_spacing.x;
                    let total_w = ui.available_width() - gap;
                    let left_w = (total_w * 0.30).floor();
                    let right_w = total_w - left_w;

                    ui.allocate_ui_with_layout(
                        egui::vec2(left_w, row_size.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                        egui::Frame::new()
                            .stroke(border)
                            .corner_radius(corner)
                            .inner_margin(egui::Margin::ZERO)
                            .show(ui, |ui| {
                                let full_w = ui.available_width();
                                let (full_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(full_w, box_total_height),
                                    egui::Sense::hover(),
                                );
                                let title_rect = egui::Rect::from_min_size(
                                    full_rect.min,
                                    egui::vec2(full_w, title_row_height),
                                );
                                let rect = egui::Rect::from_min_max(
                                    egui::pos2(
                                        full_rect.min.x,
                                        full_rect.min.y + title_row_height,
                                    ),
                                    full_rect.max,
                                );
                                paint_title(ui, title_rect, TITLE_LEFT, "Install mods");
                                let patch_mid_y = rect.min.y + inner_row_height;
                                let patch_top = egui::Rect::from_min_max(
                                    rect.min,
                                    egui::pos2(rect.max.x, patch_mid_y),
                                );
                                let patch_bottom = egui::Rect::from_min_max(
                                    egui::pos2(rect.min.x, patch_mid_y),
                                    rect.max,
                                );
                                let buttons_padded =
                                    patch_top.shrink2(egui::vec2(cell_h_padding, 0.0));
                                ui.scope_builder(
                                    egui::UiBuilder::new().max_rect(buttons_padded),
                                    |ui| {
                                        ui.with_layout(
                                            egui::Layout::left_to_right(
                                                egui::Align::Center,
                                            ),
                                            |ui| {
                                                let patch_w =
                                                    measure_button_width(ui, "Patch");
                                                let open_w = measure_button_width(
                                                    ui,
                                                    "Open patched",
                                                );
                                                let gap = ui.spacing().item_spacing.x;
                                                let total = patch_w + gap + open_w;
                                                let pad =
                                                    ((ui.available_width() - total) * 0.5)
                                                        .max(0.0);
                                                let btn_h =
                                                    ui.spacing().interact_size.y;
                                                ui.add_space(pad);
                                                if ui
                                                    .add_enabled(
                                                        enabled,
                                                        egui::Button::new("Patch")
                                                            .min_size(egui::vec2(
                                                                patch_w, btn_h,
                                                            )),
                                                    )
                                                    .clicked()
                                                {
                                                    self.trigger_patch();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        patch_path_val.is_some(),
                                                        egui::Button::new("Open patched")
                                                            .min_size(egui::vec2(
                                                                open_w, btn_h,
                                                            )),
                                                    )
                                                    .clicked()
                                                {
                                                    if let Some(path) = &patch_path_val {
                                                        self.open_in_configured_browser(path);
                                                    }
                                                }
                                            },
                                        );
                                    },
                                );
                                let mod_list = self.mod_list.clone();
                                let status_padded = patch_bottom
                                    .shrink2(egui::vec2(cell_h_padding, 0.0));
                                ui.scope_builder(
                                    egui::UiBuilder::new().max_rect(status_padded),
                                    |ui| {
                                        ui.with_layout(
                                            egui::Layout::top_down(egui::Align::Center),
                                            |ui| {
                                                render_mod_list_counts(ui, &mod_list);
                                                render_kitty_status(
                                                    ui,
                                                    mod_list.kitty_status,
                                                );
                                            },
                                        );
                                    },
                                );
                            });
                        },
                    );

                    ui.allocate_ui_with_layout(
                        egui::vec2(right_w, row_size.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                        egui::Frame::new()
                            .stroke(border)
                            .corner_radius(corner)
                            .inner_margin(egui::Margin::ZERO)
                            .show(ui, |ui| {
                                let full_w = ui.available_width();
                                let (full_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(full_w, box_total_height),
                                    egui::Sense::hover(),
                                );
                                let title_rect = egui::Rect::from_min_size(
                                    full_rect.min,
                                    egui::vec2(full_w, title_row_height),
                                );
                                let rect = egui::Rect::from_min_max(
                                    egui::pos2(
                                        full_rect.min.x,
                                        full_rect.min.y + title_row_height,
                                    ),
                                    full_rect.max,
                                );
                                paint_title(ui, title_rect, TITLE_RIGHT, "Developer tools");

                                let mid_x = (rect.min.x + rect.max.x) * 0.5;
                                let mid_y = (rect.min.y + rect.max.y) * 0.5;
                                let tl = egui::Rect::from_min_max(
                                    rect.min,
                                    egui::pos2(mid_x, mid_y),
                                );
                                let tr = egui::Rect::from_min_max(
                                    egui::pos2(mid_x, rect.min.y),
                                    egui::pos2(rect.max.x, mid_y),
                                );
                                let bl = egui::Rect::from_min_max(
                                    egui::pos2(rect.min.x, mid_y),
                                    egui::pos2(mid_x, rect.max.y),
                                );
                                let br = egui::Rect::from_min_max(
                                    egui::pos2(mid_x, mid_y),
                                    rect.max,
                                );

                                let painter = ui.painter().clone();
                                painter.hline(rect.x_range(), mid_y, border);
                                painter.vline(mid_x, rect.y_range(), border);

                                let src_dir_exists = self.src_dir_exists;
                                centred_row(ui, tl, &mut |ui| {
                                    if ui
                                        .add_enabled(enabled, egui::Button::new("Extract"))
                                        .clicked()
                                    {
                                        self.trigger_extract();
                                    }
                                    if ui
                                        .add_enabled(
                                            src_dir_exists,
                                            egui::Button::new("Edit"),
                                        )
                                        .clicked()
                                    {
                                        self.trigger_edit_source();
                                    }
                                });
                                centred_row(ui, tr, &mut |ui| {
                                    if ui
                                        .add_enabled(enabled, egui::Button::new("Merge"))
                                        .clicked()
                                    {
                                        self.trigger_merge();
                                    }
                                    if ui
                                        .add_enabled(
                                            merge_path_val.is_some(),
                                            egui::Button::new("Open merged"),
                                        )
                                        .clicked()
                                    {
                                        if let Some(path) = &merge_path_val {
                                            self.open_in_configured_browser(path);
                                        }
                                    }
                                });
                                centred_row(ui, bl, &mut |ui| {
                                    if ui
                                        .add_enabled(enabled, egui::Button::new("Diff"))
                                        .clicked()
                                    {
                                        self.trigger_diff();
                                    }
                                });
                                centred_row(ui, br, &mut |ui| {
                                    if ui
                                        .add_enabled(enabled, egui::Button::new("Livemerge"))
                                        .clicked()
                                    {
                                        self.spawn_livemerge();
                                    }
                                    if ui
                                        .add_enabled(
                                            livemerge_running,
                                            egui::Button::new("Stop"),
                                        )
                                        .clicked()
                                    {
                                        self.stop_livemerge();
                                    }
                                    if ui
                                        .add_enabled(
                                            livemerge_path_val.is_some(),
                                            egui::Button::new("Open"),
                                        )
                                        .clicked()
                                    {
                                        if let Some(path) = &livemerge_path_val {
                                            self.open_in_configured_browser(path);
                                        }
                                    }
                                });
                            });
                        },
                    );
                },
            );

            ui.add_space(extra_vspace);
    }
}

// ---- Free helpers -------------------------------------------------------

pub(crate) fn detect_starting_html() -> Option<PathBuf> {
    detect_html_in_dir(&std::env::current_dir().ok()?)
}

pub(crate) fn detect_html_in_dir(dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if !entry.file_type().ok()?.is_file() {
            continue;
        }
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str())?;
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".html") || lower.ends_with(".htm")) {
            continue;
        }
        if is_derived_output(&lower) {
            continue;
        }
        candidates.push(path);
        if candidates.len() > 1 {
            return None;
        }
    }
    candidates.into_iter().next()
}

fn is_unmatched_mod_bundle<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> bool {
    let mut diff_stem: Option<String> = None;
    let mut json_stem: Option<String> = None;
    let mut file_count = 0usize;

    for i in 0..archive.len() {
        let entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => return false,
        };
        if entry.is_dir() {
            continue;
        }
        file_count += 1;
        if file_count > 2 {
            return false;
        }
        let raw_name = entry.name();
        let file_name = Path::new(raw_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(raw_name);
        if let Some(stem) = file_name.strip_suffix(".diff") {
            if diff_stem.is_some() {
                return false;
            }
            diff_stem = Some(stem.to_string());
        } else if let Some(stem) = file_name.strip_suffix(".json") {
            if json_stem.is_some() {
                return false;
            }
            json_stem = Some(stem.to_string());
        } else {
            return false;
        }
    }

    match (diff_stem, json_stem) {
        (Some(d), Some(j)) => file_count == 2 && d != j,
        _ => false,
    }
}

fn copy_zip_entry<R: std::io::Read>(entry: &mut R, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(entry, &mut out)?;
    Ok(())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

fn measure_button_width(ui: &egui::Ui, text: &str) -> f32 {
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    galley.size().x + ui.spacing().button_padding.x * 2.0
}

fn render_mod_list_counts(ui: &mut egui::Ui, state: &ModListState) {
    if state.mod_count == 0 && state.mod_kitty_count == 0 {
        ui.label("No mods found");
    } else {
        ui.label(format!(
            "{} .diff mods, {} .mod files, {} errors",
            state.mod_count, state.mod_kitty_count, state.mod_error_count
        ));
    }
}

fn render_kitty_status(ui: &mut egui::Ui, status: KittyStatus) {
    let bad = egui::Color32::from_rgb(180, 90, 90);
    let good = egui::Color32::from_rgb(110, 170, 110);
    match status {
        KittyStatus::Unused => {}
        KittyStatus::MissingPatcherPath => {
            ui.label(egui::RichText::new("KittyPatcher NOT FOUND (set it on the Config tab)").color(bad));
        }
        KittyStatus::MissingPythonPath => {
            ui.label(
                egui::RichText::new("Python for KittyPatcher NOT FOUND (set it on the Config tab)").color(bad),
            );
        }
        KittyStatus::Present => {
            ui.label(egui::RichText::new("KittyPatcher present").color(good));
        }
        KittyStatus::Internal => {
            ui.label(egui::RichText::new("Internal patcher chosen").color(good));
        }
        KittyStatus::InternalFallback { patcher_found } => {
            let text = if patcher_found {
                "Internal patcher chosen (KittyPatcher fallback present)"
            } else {
                "Internal patcher chosen (no KittyPatcher fallback set)"
            };
            ui.label(egui::RichText::new(text).color(good));
        }
    }
}

pub(crate) fn detect_existing_outputs(
    html_path: Option<&Path>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let Some(p) = html_path else {
        return (None, None);
    };
    let existing = |label: &str| -> Option<PathBuf> {
        commands::derive_output(p, label)
            .ok()
            .filter(|p| p.is_file())
    };
    (existing("merged"), existing("modded"))
}

fn is_derived_output(lower_name: &str) -> bool {
    const SUFFIXES: [&str; 4] = [
        ".merged.html",
        ".modded.html",
        ".livemerged.html",
        ".livepatched.html",
    ];
    SUFFIXES.iter().any(|s| lower_name.ends_with(s))
}

