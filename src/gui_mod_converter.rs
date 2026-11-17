//! Mod-converter tab specific GUI code: render functions for the
//! converter input panel, the import-version / import-mods modal,
//! the list views, and the App methods that drive them. Lives
//! alongside `gui.rs`, which keeps the shared App state, common
//! widget helpers, and the Output panel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use eframe::egui;

use crate::gui::{App, TITLE_GREY, titled_box};
use crate::mod_converter::{
    self, ConvertInputs, ConverterModIndex, ConverterModInfo, ConverterModStatus,
    ConverterVersionIndex,
};

/// State backing the Mod-converter import dialog. Two flavours exist:
/// a single-HTML import (game-version registration) and a batch
/// .mod/.diff import (mod conversion source). Bytes are slurped at
/// drop time so a confirm click only writes — the zip can be moved or
/// deleted afterwards without affecting the import.
pub(crate) struct ConverterImportDialog {
    pub(crate) zip_path: PathBuf,
    pub(crate) version_input: String,
    pub(crate) kind: ConverterImportKind,
    /// Flips to `true` after the first render flushes a focus
    /// request to the version text edit. Without this guard we'd
    /// re-request focus every frame, which turns Enter handling
    /// into a no-op (the focus restore racing the press cancels
    /// `Response::lost_focus`).
    pub(crate) auto_focused: bool,
}

/// State backing the "Change game version" modal opened from the
/// pencil icon next to a mod's Game version cell. `file_name` is the
/// HashMap key in `converter_mod_index.mods` (full filename incl.
/// extension); `previous_version` is the version the file currently
/// lives under on disk. `version_input` starts pre-filled with that
/// previous version so just hitting Enter is a no-op.
pub(crate) struct ChangeVersionDialog {
    pub(crate) file_name: String,
    pub(crate) previous_version: String,
    pub(crate) version_input: String,
    pub(crate) auto_focused: bool,
}

pub(crate) enum ConverterImportKind {
    GameVersion {
        html_entry_name: String,
        html_bytes: Vec<u8>,
    },
    Mods {
        base_directory: PathBuf,
        files: Vec<(String, Vec<u8>)>,
    },
}

impl App {
    /// Drag-and-drop handler used while the Mod-converter tab is
    /// active. Directory drops set the converter base directory; loose
    /// `.diff`/`.mod` drops trigger the "Import mods" dialog with that
    /// single file as the payload (same code path as if it had been
    /// dropped inside a zip); loose `.html`/`.htm` drops and zip drops
    /// with exactly one `.html` entry both trigger the "Import game
    /// version" dialog. Anything else is ignored with a warning.
    pub(crate) fn handle_converter_drop(&mut self, path: &Path) {
        if path.is_dir() {
            self.set_converter_directory(Some(path.to_path_buf()));
            return;
        }
        let lower_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if lower_name.ends_with(".diff") || lower_name.ends_with(".mod") {
            // Same downstream flow as a zipped mod file — read the
            // bytes once at drop time and queue an Import-mods dialog
            // with this single (filename, bytes) entry.
            let Some(base_dir) = self.converter_directory.clone() else {
                log::warn!(
                    "Mod file {} dropped but no base directory is set — set one before importing mods",
                    path.display()
                );
                return;
            };
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(err) => {
                    log::warn!("Failed to read {}: {err}", path.display());
                    return;
                }
            };
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            self.converter_import_dialogs
                .push_back(ConverterImportDialog {
                    zip_path: path.to_path_buf(),
                    version_input: String::new(),
                    kind: ConverterImportKind::Mods {
                        base_directory: base_dir,
                        files: vec![(file_name, bytes)],
                    },
                    auto_focused: false,
                });
            return;
        }
        if lower_name.ends_with(".html") || lower_name.ends_with(".htm") {
            // Same downstream flow as a zip with a single .html entry —
            // queue an Import-game-version dialog with the file's bytes
            // as the payload.
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(err) => {
                    log::warn!("Failed to read {}: {err}", path.display());
                    return;
                }
            };
            let entry_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            self.converter_import_dialogs
                .push_back(ConverterImportDialog {
                    zip_path: path.to_path_buf(),
                    version_input: String::new(),
                    kind: ConverterImportKind::GameVersion {
                        html_entry_name: entry_name,
                        html_bytes: bytes,
                    },
                    auto_focused: false,
                });
            return;
        }
        if lower_name.ends_with(".zip") {
            // Mod-bundle takes precedence: any .mod/.diff entries
            // means this zip is being imported as a mods-from-version
            // payload, regardless of any HTML it might also contain.
            let mod_files = mod_converter::read_mod_files_from_zip(path);
            if !mod_files.is_empty() {
                let Some(base_dir) = self.converter_directory.clone() else {
                    log::warn!(
                        "Zip {} contains mod files but no base directory is set — set one before importing mods",
                        path.display()
                    );
                    return;
                };
                self.converter_import_dialogs
                    .push_back(ConverterImportDialog {
                        zip_path: path.to_path_buf(),
                        version_input: String::new(),
                        kind: ConverterImportKind::Mods {
                            base_directory: base_dir,
                            files: mod_files,
                        },
                        auto_focused: false,
                    });
                return;
            }
            // Fall back to single-HTML game-version import.
            match mod_converter::read_single_html_from_zip(path) {
                Some((html_entry, html_bytes)) => {
                    self.converter_import_dialogs
                        .push_back(ConverterImportDialog {
                            zip_path: path.to_path_buf(),
                            version_input: String::new(),
                            kind: ConverterImportKind::GameVersion {
                                html_entry_name: html_entry,
                                html_bytes,
                            },
                            auto_focused: false,
                        });
                }
                None => {
                    log::warn!(
                        "Zip {} contains no recognised mod files and not exactly one .html — nothing to import",
                        path.display()
                    );
                }
            }
            return;
        }
        log::warn!(
            "Drag&drop on Mod converter tab only accepts directories, .diff/.mod files, .html files, or version zip files: {}",
            path.display()
        );
    }

    /// Modal import dialog for the Mod converter tab. Two flavours,
    /// dispatched on `dialog.kind`: GameVersion writes the slurped
    /// HTML to `<exe>/versions/v<version>.html` (and refreshes the
    /// version index); Mods writes every slurped .mod/.diff into
    /// `<base>/from-v<version>/`. Both reject Add unless the version
    /// string starts with a digit.
    pub(crate) fn render_converter_import_dialog(&mut self, ctx: &egui::Context) {
        // Once the queue runs dry, drop any in-effect "Add all"
        // version. A fresh batch always starts in manual mode.
        if self.converter_import_dialogs.is_empty() {
            self.converter_import_auto_version = None;
            return;
        }
        // Pop the head of the queue, work with it; on close (Cancel,
        // or Add-success) we drop it so the next queued dialog comes
        // forward next frame. While still pending we push it back to
        // the front so it stays the active modal across frames.
        let mut dialog = self
            .converter_import_dialogs
            .pop_front()
            .expect("non-empty checked above");
        let is_mods = matches!(dialog.kind, ConverterImportKind::Mods { .. });
        let mut commit = false;
        let mut commit_all = false;
        let mut close = false;

        // If "Add all" is in effect and this is a Mods dialog, skip
        // rendering and commit immediately with the remembered
        // version — same write path as a normal manual commit, just
        // pre-filled. GameVersion (HTML) imports always render even
        // when auto_version is set; the user explicitly opted out of
        // auto-completing those.
        if is_mods {
            if let Some(v) = self.converter_import_auto_version.clone() {
                dialog.version_input = v;
                commit = true;
            }
        }

        let (title, info_line) = match &dialog.kind {
            ConverterImportKind::GameVersion {
                html_entry_name, ..
            } => (
                "Import game version",
                format!("HTML in archive: {html_entry_name}"),
            ),
            ConverterImportKind::Mods { files, .. } => (
                "Import mods",
                format!("Mod files in archive: {}", files.len()),
            ),
        };

        if !commit {
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("Source: {}", dialog.zip_path.display()));
                ui.label(info_line);
                ui.add_space(4.0);
                let mut version_response = None;
                ui.horizontal(|ui| {
                    ui.label("Game version:");
                    let resp = ui.text_edit_singleline(&mut dialog.version_input);
                    // One-shot auto-focus on the first render so the
                    // user can type immediately. After that we never
                    // re-request — doing so on every frame would race
                    // the Enter keypress and silently re-grab focus
                    // before `resp.lost_focus()` could see it.
                    if !dialog.auto_focused {
                        resp.request_focus();
                        dialog.auto_focused = true;
                    }
                    version_response = Some(resp);
                });
                let starts_with_digit = dialog
                    .version_input
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit());
                if !starts_with_digit && !dialog.version_input.is_empty() {
                    ui.label(
                        egui::RichText::new("Version must start with a digit")
                            .color(egui::Color32::from_rgb(180, 90, 90)),
                    );
                }
                // Enter on the focused text edit submits when the
                // version is valid; Escape always cancels regardless of
                // focus, matching standard modal-dialog behaviour.
                if let Some(resp) = version_response.as_ref() {
                    if starts_with_digit
                        && resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        commit = true;
                    }
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(starts_with_digit, egui::Button::new("Add"))
                        .clicked()
                    {
                        commit = true;
                    }
                    // "Add all" only makes sense for mod batches —
                    // GameVersion (HTML) imports always require an
                    // explicit version per file.
                    if is_mods
                        && ui
                            .add_enabled(
                                starts_with_digit,
                                egui::Button::new("Add all"),
                            )
                            .clicked()
                    {
                        commit = true;
                        commit_all = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        }

        if commit {
            match &dialog.kind {
                ConverterImportKind::GameVersion {
                    html_entry_name,
                    html_bytes,
                } => match mod_converter::write_html_to_versions(
                    html_bytes,
                    &dialog.version_input,
                ) {
                    Ok(out_path) => {
                        log::info!(
                            "Imported {} as game version v{} -> {}",
                            html_entry_name,
                            dialog.version_input,
                            out_path.display()
                        );
                        self.scan_versions();
                        close = true;
                    }
                    Err(err) => {
                        log::error!("Failed to import game version: {err:#}");
                    }
                },
                ConverterImportKind::Mods {
                    base_directory,
                    files,
                } => match mod_converter::write_mods_to_from_version_dir(
                    base_directory,
                    &dialog.version_input,
                    files,
                ) {
                    Ok(out_dir) => {
                        log::info!(
                            "Imported {} mod files for v{} -> {}",
                            files.len(),
                            dialog.version_input,
                            out_dir.display()
                        );
                        // The freshly-written from-v<version>/ may
                        // have added or replaced files this index
                        // tracks, so re-scan against the same base
                        // directory to surface them in the list view.
                        self.scan_mods();
                        close = true;
                        if commit_all {
                            self.converter_import_auto_version =
                                Some(dialog.version_input.clone());
                        }
                    }
                    Err(err) => {
                        log::error!("Failed to import mods: {err:#}");
                        // If this commit was driven by an in-effect
                        // auto-version (no Window rendered this
                        // frame), clear the flag and re-queue so the
                        // next frame surfaces the dialog manually
                        // for the user to fix.
                        if self.converter_import_auto_version.is_some() {
                            self.converter_import_auto_version = None;
                        }
                    }
                },
            }
        }

        if !close {
            self.converter_import_dialogs.push_front(dialog);
        }
    }

    /// Modal "Change game version" dialog for a single mod file.
    /// Three exits:
    /// - Change / Enter: if the version differs from the previous
    ///   one, move `from-v<prev>/<file>` to `from-v<new>/<file>`.
    /// - Cancel / Esc: discard, no filesystem changes.
    /// - Delete: move `from-v<prev>/<file>` to
    ///   `tmp/removed-from-v<prev>/<file>`.
    /// Any non-cancel exit triggers a `scan_mods()` so the list
    /// reflects the new on-disk state.
    pub(crate) fn render_change_version_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.change_version_dialog.take() else {
            return;
        };
        let Some(base) = self.converter_directory.clone() else {
            // Base directory was cleared while the dialog was open —
            // nothing meaningful to do; drop the dialog.
            log::warn!("Change game version dialog closed: no base directory");
            return;
        };

        let mut do_change = false;
        let mut do_delete = false;
        let mut close = false;

        egui::Window::new("Change game version")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(format!("Mod file: {}", dialog.file_name));
                ui.label(format!("Current version: v{}", dialog.previous_version));
                ui.add_space(4.0);
                let mut version_response = None;
                ui.horizontal(|ui| {
                    ui.label("New version:");
                    let resp = ui.text_edit_singleline(&mut dialog.version_input);
                    if !dialog.auto_focused {
                        resp.request_focus();
                        dialog.auto_focused = true;
                    }
                    version_response = Some(resp);
                });
                let starts_with_digit = dialog
                    .version_input
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit());
                if !starts_with_digit && !dialog.version_input.is_empty() {
                    ui.label(
                        egui::RichText::new("Version must start with a digit")
                            .color(egui::Color32::from_rgb(180, 90, 90)),
                    );
                }
                // Enter on the focused text edit submits when the
                // version is valid; Escape cancels regardless of
                // focus, matching the import dialog's behaviour.
                if let Some(resp) = version_response.as_ref() {
                    if starts_with_digit
                        && resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        do_change = true;
                    }
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(starts_with_digit, egui::Button::new("Change"))
                        .clicked()
                    {
                        do_change = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.button("Delete").clicked() {
                        do_delete = true;
                    }
                });
            });

        if do_delete {
            match mod_converter::delete_mod_file(
                &base,
                &dialog.previous_version,
                &dialog.file_name,
            ) {
                Ok(dest) => {
                    log::info!(
                        "Deleted mod {} (moved to {})",
                        dialog.file_name,
                        dest.display()
                    );
                    self.scan_mods();
                    close = true;
                }
                Err(err) => {
                    log::error!("Failed to delete mod {}: {err:#}", dialog.file_name);
                }
            }
        } else if do_change {
            // No-op when the version is unchanged — just close.
            if dialog.version_input == dialog.previous_version {
                close = true;
            } else {
                match mod_converter::move_mod_to_version(
                    &base,
                    &dialog.previous_version,
                    &dialog.version_input,
                    &dialog.file_name,
                ) {
                    Ok(dest) => {
                        log::info!(
                            "Moved mod {} to v{} ({})",
                            dialog.file_name,
                            dialog.version_input,
                            dest.display()
                        );
                        self.scan_mods();
                        close = true;
                    }
                    Err(err) => {
                        log::error!(
                            "Failed to move mod {} to v{}: {err:#}",
                            dialog.file_name,
                            dialog.version_input
                        );
                    }
                }
            }
        }

        if !close {
            self.change_version_dialog = Some(dialog);
        }
    }

    /// Native folder picker for the Mod-converter tab.
    pub(crate) fn browse_converter_directory(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.set_converter_directory(Some(path));
        }
    }

    pub(crate) fn set_converter_directory(&mut self, path: Option<PathBuf>) {
        if let Some(p) = path.as_deref() {
            let as_string = p.display().to_string();
            if self.config.last_converter_directory != as_string {
                self.config.last_converter_directory = as_string;
                if let Err(err) = crate::config::save(&self.config) {
                    log::error!("Failed to persist last converter directory: {err:#}");
                }
            }
        }
        self.converter_directory = path;
        self.scan_versions();
        self.scan_mods();
        // The first scan has now happened; subsequent tab opens won't
        // double-scan, but later directory changes still refresh.
        self.converter_index_initialized = true;
    }

    /// Restore the last-used base directory from config. Used by the
    /// "Use last" button — only meaningful when nothing is currently
    /// selected (the button is greyed out otherwise).
    pub(crate) fn use_last_converter_directory(&mut self) {
        let last = self.config.last_converter_directory.clone();
        if last.is_empty() {
            return;
        }
        let path = PathBuf::from(&last);
        if !path.is_dir() {
            log::warn!("Last converter directory no longer exists: {last}");
            return;
        }
        self.set_converter_directory(Some(path));
    }

    /// Refresh the version index from disk, then run the
    /// post-scan reconcile step so the version list and mod-referenced
    /// versions stay in sync.
    pub(crate) fn scan_versions(&mut self) {
        self.converter_version_index
            .scan_versions(self.converter_directory.as_deref());
        self.finalize_converter_indexes();
    }

    /// Refresh the mod index from disk, then run the post-scan
    /// reconcile step. When no base directory is set the index is
    /// reset wholesale (scan_mods needs a directory to operate on).
    pub(crate) fn scan_mods(&mut self) {
        if let Some(dir) = self.converter_directory.as_deref() {
            self.converter_mod_index.scan_mods(dir);
        } else {
            self.converter_mod_index = ConverterModIndex::new();
        }
        self.finalize_converter_indexes();
    }

    /// Switch the converter target version. No-op if the requested
    /// version already matches. Otherwise archives every existing
    /// `to-v*` directory into `inactive-to-v*`, creates a fresh
    /// `to-v<version>`, then re-scans so the index reflects the new
    /// target.
    pub(crate) fn set_target_version(&mut self, version: &str) {
        if self.converter_mod_index.target_version.as_deref() == Some(version) {
            return;
        }
        let Some(base) = self.converter_directory.clone() else {
            log::warn!("Cannot switch target version: no base directory selected");
            return;
        };
        if let Err(err) = mod_converter::switch_target_version(&base, version) {
            log::error!("Failed to switch target version to v{version}: {err:#}");
            return;
        }
        log::info!("Target version switched to v{version}");
        self.scan_mods();
    }

    /// Already-running guard used by the Start button. Reuses the
    /// existing global `running` slot so converter work blocks the
    /// modding tab's commands too — only one task at a time.
    pub(crate) fn is_converting(&self) -> bool {
        self.current_task().as_deref() == Some("Convert")
    }

    /// Spawn the background conversion worker over the supplied mod
    /// subset. Snapshots everything the worker needs (cloned
    /// ConvertInputs, fresh cancel flag) so any subsequent edits to
    /// the GUI's live indexes don't leak into the running conversion.
    /// Refuses to spawn unless the target version's GameData is
    /// actually loaded — mods that reference a missing source version
    /// are still passed through and fail with a per-mod error during
    /// conversion.
    pub(crate) fn start_conversion(&mut self, mods: Vec<ConverterModInfo>) {
        if self.is_running() {
            return;
        }
        if mods.is_empty() {
            log::warn!("No mods selected for conversion");
            return;
        }
        let Some(base) = self.converter_directory.clone() else {
            log::warn!("Cannot start conversion: no base directory selected");
            return;
        };
        let Some(target) = self.converter_mod_index.target_version.clone() else {
            log::warn!("Cannot start conversion: no target version selected");
            return;
        };
        let target_data = match self
            .converter_version_index
            .versions
            .get(&target)
            .and_then(|v| v.game_data.as_ref())
        {
            Some(Ok(arc)) => arc.clone(),
            Some(Err(err)) => {
                log::warn!(
                    "Cannot start conversion: target version v{target} HTML failed to parse: {err:#}"
                );
                return;
            }
            None => {
                log::warn!(
                    "Cannot start conversion: target version v{target} HTML not loaded"
                );
                return;
            }
        };

        // Collect Arc<GameData> for every version any mod (or the
        // target itself) points to. Versions whose load failed or
        // never happened are silently omitted; the per-mod conversion
        // catches the absence and surfaces a clean error.
        let mut source_data: HashMap<String, Arc<_>> = HashMap::new();
        source_data.insert(target.clone(), target_data.clone());
        for m in self.converter_mod_index.mods.values() {
            if source_data.contains_key(&m.game_version) {
                continue;
            }
            if let Some(Ok(arc)) = self
                .converter_version_index
                .versions
                .get(&m.game_version)
                .and_then(|v| v.game_data.as_ref())
            {
                source_data.insert(m.game_version.clone(), arc.clone());
            }
        }

        let inputs = ConvertInputs {
            base_directory: base,
            target_version: target,
            target_data,
            source_data,
            mods,
            config: self.config.clone(),
        };

        // Fresh cancel flag for this run so a Stop click on a
        // previous run doesn't immediately cancel this one.
        self.convert_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.convert_cancel.clone();
        let finished = self.convert_finished.clone();
        let outcome_slot = self.convert_outcome.clone();
        let running = self.running.clone();
        *running.lock().unwrap() = Some("Convert".to_string());
        finished.store(false, Ordering::Relaxed);
        // Wipe any previous outcome so the post-finish poll below
        // doesn't operate on a stale result if it fires first.
        *outcome_slot.lock().unwrap() = None;

        thread::spawn(move || {
            log::info!("--- Starting Convert ---");
            let outcome = mod_converter::convert(inputs, cancel);
            *outcome_slot.lock().unwrap() = Some(outcome);
            log::info!("--- Convert finished ---");
            finished.store(true, Ordering::Relaxed);
            *running.lock().unwrap() = None;
        });
    }

    /// Trip the cooperative cancel flag — the worker reads it between
    /// mods, so the current per-mod step finishes before exit.
    pub(crate) fn stop_conversion(&self) {
        self.convert_cancel.store(true, Ordering::Relaxed);
    }

    /// Open the "Change game version" modal for the given mod file.
    /// `file_name` is the HashMap key in `converter_mod_index.mods`.
    /// Looks up the current version off the index entry and stages
    /// it as the dialog's pre-filled value.
    pub(crate) fn open_change_version_dialog(&mut self, file_name: &str) {
        let Some(info) = self.converter_mod_index.mods.get(file_name) else {
            log::warn!(
                "Cannot change game version: {file_name} not in mod index"
            );
            return;
        };
        self.change_version_dialog = Some(ChangeVersionDialog {
            file_name: file_name.to_string(),
            previous_version: info.game_version.clone(),
            version_input: info.game_version.clone(),
            auto_focused: false,
        });
    }

    /// Open `<base>/from-v<version>/<file_name>` in the configured
    /// text editor. `file_name` is the HashMap key from
    /// `converter_mod_index.mods` (always the full name including
    /// extension), so the path is fully reconstructable from the
    /// index entry alone.
    pub(crate) fn open_mod_file_in_editor(&self, file_name: &str) {
        let Some(base) = self.converter_directory.as_deref() else {
            log::warn!("Cannot open mod file: no base directory selected");
            return;
        };
        let Some(info) = self.converter_mod_index.mods.get(file_name) else {
            log::warn!("Cannot open mod file: {file_name} not in index");
            return;
        };
        let path = base
            .join(format!("from-v{}", info.game_version))
            .join(file_name);
        crate::tool_execute::open_text_editor(&self.config, &path);
    }

    /// Open `<base>/tmp/src-v<version>` in the configured editor,
    /// running `game_data_extractor::extract_from_data` first if the
    /// directory doesn't already exist. The extraction step is offloaded
    /// to a worker thread; the editor launch happens from that thread
    /// once extraction succeeds. When the version's GameData is already
    /// cached on the version index we hand the worker an Arc clone
    /// instead of re-reading the HTML.
    pub(crate) fn open_or_extract_version_source(&self, version: &str) {
        // Fall back to the executable directory when the user hasn't
        // picked a base directory yet — opening a bundled version's
        // source shouldn't depend on having a converter project set up.
        let base = match self.converter_directory.as_deref() {
            Some(p) => p.to_path_buf(),
            None => {
                let Some(exe_dir) = crate::config::executable_dir() else {
                    log::warn!(
                        "Cannot open version source: no base directory selected and executable directory unavailable"
                    );
                    return;
                };
                exe_dir
            }
        };
        let target_dir = base.join("tmp").join(format!("src-v{version}"));
        if target_dir.is_dir() {
            crate::tool_execute::open_directory_editor(&self.config, &target_dir);
            return;
        }

        let Some(info) = self.converter_version_index.versions.get(version) else {
            log::warn!("Cannot open version source: v{version} not in index");
            return;
        };
        // Try cached GameData first; fall back to the HTML path so the
        // worker can read it itself. Either way the version's HTML must
        // be present on disk to be openable.
        let cached = match info.game_data.as_ref() {
            Some(Ok(arc)) => Some(arc.clone()),
            _ => None,
        };
        let html_path = info.html_file_info.as_ref().map(|h| h.path.clone());
        if cached.is_none() && html_path.is_none() {
            log::warn!("Cannot open version source: v{version} has no HTML file on disk");
            return;
        }

        let config = self.config.clone();
        let version_owned = version.to_string();
        thread::spawn(move || {
            let data = match cached {
                Some(arc) => arc,
                None => {
                    let path = html_path.expect("checked above");
                    match crate::game_data_reader::read_from_file(&path) {
                        Ok(d) => Arc::new(d),
                        Err(err) => {
                            log::error!(
                                "Failed to parse v{version_owned} HTML for source extraction: {err:#}"
                            );
                            return;
                        }
                    }
                }
            };
            log::info!(
                "Extracting v{version_owned} source to {}",
                target_dir.display()
            );
            if let Err(err) =
                crate::game_data_extractor::extract_from_data(&data, &target_dir)
            {
                log::error!("Source extraction for v{version_owned} failed: {err:#}");
                return;
            }
            crate::tool_execute::open_directory_editor(&config, &target_dir);
        });
    }

    /// Move every `.resolve` template that no longer contains the
    /// "REPLACE THIS WITH RESOLUTION" placeholder out of
    /// `tmp/resolve-templates/` and into `conflicts/` so subsequent
    /// conversions can pick them up via the resolution index.
    pub(crate) fn move_resolved_conflicts(&self) {
        let Some(base) = self.converter_directory.as_deref() else {
            log::warn!("Cannot move resolved conflicts: no base directory selected");
            return;
        };
        match mod_converter::move_resolved_conflicts(base) {
            Ok(moved) if moved.is_empty() => {
                log::info!("No resolved conflict files to move");
            }
            Ok(moved) => {
                log::info!(
                    "Moved {} resolved conflict file(s) into conflicts/:",
                    moved.len()
                );
                for name in &moved {
                    log::info!("  {name}");
                }
            }
            Err(err) => {
                log::error!("Failed to move resolved conflicts: {err:#}");
            }
        }
    }

    /// Called once per frame from `render_mod_converter`: when the
    /// worker has signalled completion since the last poll, take the
    /// outcome, mark every failed mod's status as Failed in the live
    /// index, then re-scan. The Failed marks survive the rebuild via
    /// `scan_mods`'s sticky-Failed-when-bytes-unchanged rule, while
    /// successful mods get flipped to Converted naturally because
    /// their output now lives in `to-v*/`.
    fn poll_convert_finished(&mut self) {
        if !self.convert_finished.swap(false, Ordering::Relaxed) {
            return;
        }
        let outcome = self.convert_outcome.lock().unwrap().take();
        if let Some(outcome) = outcome {
            if !outcome.failed.is_empty() {
                let failed: std::collections::HashSet<&str> =
                    outcome.failed.iter().map(String::as_str).collect();
                for info in self.converter_mod_index.mods.values_mut() {
                    if failed.contains(info.name.as_str()) {
                        info.status = ConverterModStatus::Failed;
                    }
                }
            }
        }
        self.scan_mods();
    }

    /// Shared cleanup after either scan_versions or scan_mods:
    ///
    /// 1. Drop version entries with no `html_file_info` — we treat
    ///    those as stale placeholders left behind by previous mod
    ///    references.
    /// 2. For every mod, ask the version index to prepare a slot for
    ///    its `game_version`. That re-inserts placeholders for any
    ///    version that's referenced by a mod but missing from disk
    ///    (so the user sees them as "Missing" in the version list)
    ///    and lazily loads the GameData when the HTML is present.
    fn finalize_converter_indexes(&mut self) {
        self.converter_version_index
            .versions
            .retain(|_, v| v.html_file_info.is_some());

        // Collect first to avoid holding an immutable borrow on
        // `converter_mod_index` while mutating `converter_version_index`.
        let game_versions: Vec<String> = self
            .converter_mod_index
            .mods
            .values()
            .map(|m| m.game_version.clone())
            .collect();
        for version in game_versions {
            self.converter_version_index.prepare_version(&version);
        }
        // Same treatment for the converter target — its version needs
        // a slot in the index even if no mod currently references it,
        // so the version list shows it (as Missing if its HTML isn't
        // on disk yet).
        if let Some(target) = self.converter_mod_index.target_version.clone() {
            self.converter_version_index.prepare_version(&target);
        }

        self.update_converter_status();
    }

    /// Rebuild `converter_status` — a one-liner that summarises
    /// whatever's currently blocking the converter from doing useful
    /// work. Empty when nothing's wrong. Order of checks roughly
    /// follows "what does the user need to fix first".
    fn update_converter_status(&mut self) {
        use mod_converter::ConverterVersionInfo;

        let mut status = String::new();
        if self.converter_directory.is_none() {
            status.push_str("No base directory.");
            self.converter_status = status;
            return;
        }

        let version_state = |info: Option<&ConverterVersionInfo>| {
            match info {
                None => Some("missing"),
                Some(v) if v.html_file_info.is_none() => Some("missing"),
                Some(v) if matches!(v.game_data, Some(Err(_))) => Some("invalid"),
                Some(_) => None,
            }
        };

        match self.converter_mod_index.target_version.as_deref() {
            None => status.push_str("Target version not selected. "),
            Some(target) => {
                match version_state(self.converter_version_index.versions.get(target)) {
                    Some("missing") => status.push_str("Target version HTML missing. "),
                    Some("invalid") => status.push_str("Target version HTML invalid. "),
                    _ => {}
                }
            }
        }

        if self.converter_mod_index.mods.is_empty() {
            status.push_str("No mods to convert. ");
        } else {
            let mut missing = 0usize;
            let mut invalid = 0usize;
            for m in self.converter_mod_index.mods.values() {
                match version_state(
                    self.converter_version_index.versions.get(&m.game_version),
                ) {
                    Some("missing") => missing += 1,
                    Some("invalid") => invalid += 1,
                    _ => {}
                }
            }
            if missing > 0 {
                status.push_str(&format!("{missing} mods with version HTML missing. "));
            }
            if invalid > 0 {
                status.push_str(&format!("{invalid} mods with version HTML invalid. "));
            }
        }

        self.converter_status = status;
    }

    pub(crate) fn render_mod_converter(&mut self, ui: &mut egui::Ui) {
        // First-time tab-open scan so startup doesn't pay for a
        // directory walk that the user may never need.
        if !self.converter_index_initialized {
            self.scan_versions();
            self.converter_index_initialized = true;
        }

        let ctx = ui.ctx().clone();
        let border = ui.visuals().widgets.noninteractive.bg_stroke;
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        titled_box(ui, "Input", TITLE_GREY, &mut |ui| {
            ui.horizontal(|ui| {
                ui.label("Base directory:");
                let path_text = if hovering {
                    egui::RichText::new("Drop the directory here…")
                        .color(egui::Color32::LIGHT_GREEN)
                } else if let Some(p) = &self.converter_directory {
                    egui::RichText::new(p.display().to_string())
                } else {
                    egui::RichText::new("(drop a directory or click Browse…)")
                };

                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.button("Browse…").clicked() {
                            self.browse_converter_directory();
                        }
                        let use_last_enabled = self.converter_directory.is_none()
                            && !self.config.last_converter_directory.is_empty();
                        if ui
                            .add_enabled(use_last_enabled, egui::Button::new("Use last"))
                            .on_hover_text(self.config.last_converter_directory.as_str())
                            .clicked()
                        {
                            self.use_last_converter_directory();
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
                                    egui::Layout::left_to_right(egui::Align::Center),
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
                if self.converter_directory.is_none() {
                    ui.label(
                        "No base directory selected - browse or drag&drop the base directory for the version-specific subdirectories",
                    );
                } else {
                    let target = self
                        .converter_mod_index
                        .target_version
                        .as_deref()
                        .unwrap_or("unset (click on the circle in a version row)");
                    ui.label(format!("Target version: {target}"));
                }
            });

            ui.add_space(ui.spacing().item_spacing.y);

            // Two side-by-side list views beneath the path row. Each
            // list scrolls independently when its rows overflow the
            // 150 px height. `horizontal_top` (cross-align Min) keeps
            // the two list boxes anchored to the row's top edge; the
            // default `horizontal` centre-aligns them and produces a
            // visible Y-offset between boxes whose internal content
            // settles to slightly different sizes.
            let list_height = 150.0;
            let gap = ui.spacing().item_spacing.x;
            let total_w = ui.available_width();
            let left_w = ((total_w - gap) * 0.25).floor();
            let right_w = total_w - gap - left_w;

            let version_index = &self.converter_version_index;
            let mod_index = &self.converter_mod_index;
            let current_target = mod_index.target_version.as_deref();
            let mut clicked_target: Option<String> = None;
            let mut clicked_open: Option<String> = None;
            let mut clicked_mod_open: Option<String> = None;
            let mut clicked_mod_edit: Option<String> = None;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(left_w, list_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let (target_clicked, open_clicked) = render_versions_list(
                            ui,
                            list_height,
                            version_index,
                            current_target,
                        );
                        clicked_target = target_clicked;
                        clicked_open = open_clicked;
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(right_w, list_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        let (mod_open, mod_edit) =
                            render_mods_list(ui, list_height, mod_index);
                        clicked_mod_open = mod_open;
                        clicked_mod_edit = mod_edit;
                    },
                );
            });
            // Both index borrows end at the close of `horizontal_top`,
            // so it's safe to mutate self via set_target_version here.
            if let Some(version) = clicked_target {
                self.set_target_version(&version);
            }
            if let Some(version) = clicked_open {
                self.open_or_extract_version_source(&version);
            }
            if let Some(file_name) = clicked_mod_open {
                self.open_mod_file_in_editor(&file_name);
            }
            if let Some(file_name) = clicked_mod_edit {
                self.open_change_version_dialog(&file_name);
            }
        });

        // Match the modding tab's between-boxes spacing so Input and
        // Actions don't visually fuse.
        let extra_vspace = (ui.spacing().item_spacing.x
            - ui.spacing().item_spacing.y)
            .max(0.0);
        ui.add_space(extra_vspace);

        let mut do_rescan = false;
        let mut do_stop = false;
        let mut do_move_resolved = false;
        let mut do_edit_conflicts = false;
        let mut start_filter: Option<Option<ConverterModStatus>> = None;
        let converting = self.is_converting();
        let any_running = self.is_running();
        let total_count = self.converter_mod_index.mods.len();
        let unprocessed_count = self
            .converter_mod_index
            .mods
            .values()
            .filter(|m| m.status == ConverterModStatus::Unprocessed)
            .count();
        let failed_count = self
            .converter_mod_index
            .mods
            .values()
            .filter(|m| m.status == ConverterModStatus::Failed)
            .count();
        titled_box(ui, "Actions", TITLE_GREY, &mut |ui| {
            ui.horizontal(|ui| {
                ui.label("Status: ");
                let bad = egui::Color32::from_rgb(180, 90, 90);
                let good = egui::Color32::from_rgb(110, 170, 110);
                if self.converter_status.is_empty() {
                    ui.label(
                        egui::RichText::new("Ready to convert.").color(good),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(self.converter_status.as_str())
                            .color(bad),
                    );
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !any_running && total_count > 0,
                        egui::Button::new("Convert (all)"),
                    )
                    .clicked()
                {
                    // Outer Some = "do start", inner None = no
                    // status filter (pass every mod).
                    start_filter = Some(None);
                }
                if ui
                    .add_enabled(
                        !any_running && unprocessed_count > 0,
                        egui::Button::new("Convert (remaining)"),
                    )
                    .clicked()
                {
                    start_filter = Some(Some(ConverterModStatus::Unprocessed));
                }
                if ui
                    .add_enabled(
                        !any_running && failed_count > 0,
                        egui::Button::new("Convert (retry failed)"),
                    )
                    .clicked()
                {
                    start_filter = Some(Some(ConverterModStatus::Failed));
                }
                if ui
                    .add_enabled(converting, egui::Button::new("Stop"))
                    .clicked()
                {
                    do_stop = true;
                }
                if ui
                    .add_enabled(!any_running, egui::Button::new("Rescan"))
                    .clicked()
                {
                    do_rescan = true;
                }
                if ui
                    .add_enabled(
                        !any_running && self.converter_directory.is_some(),
                        egui::Button::new("Edit conflicts"),
                    )
                    .clicked()
                {
                    do_edit_conflicts = true;
                }
                if ui
                    .add_enabled(
                        !any_running && self.converter_directory.is_some(),
                        egui::Button::new("Move resolved conflicts"),
                    )
                    .clicked()
                {
                    do_move_resolved = true;
                }
            });
        });
        if let Some(filter) = start_filter {
            let mods: Vec<ConverterModInfo> = self
                .converter_mod_index
                .mods
                .values()
                .filter(|m| filter.map_or(true, |status| m.status == status))
                .cloned()
                .collect();
            self.start_conversion(mods);
        }
        if do_stop {
            self.stop_conversion();
        }
        if do_rescan {
            self.scan_versions();
            self.scan_mods();
        }
        if do_move_resolved {
            self.move_resolved_conflicts();
        }
        if do_edit_conflicts {
            if let Some(base) = self.converter_directory.clone() {
                let target = base.join("tmp").join("resolve-templates");
                crate::tool_execute::open_directory_editor(&self.config, &target);
            }
        }
        // Re-scan once the worker thread has signalled completion so
        // the freshly-written to-v<version>/ files flip the relevant
        // mods to Converted in the list view.
        self.poll_convert_finished();
        // While a conversion is running, force periodic repaints so
        // the log keeps updating even without user input.
        if converting {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Match the modding tab's trailing gap so the spacing before
        // the Output box equals the spacing between the boxes above.
        ui.add_space(extra_vspace);
    }
}

/// Mod-converter "Game version" list — a fixed-height bordered Frame
/// with a leading 15px clickable target-marker column, then sticky
/// "Game version" / "Status" columns. Reads directly from
/// `ConverterVersionIndex`. Returns the clicked version string when
/// the user picks a new target, so the caller can route it to
/// `App::set_target_version` (we can't call self-mutating methods
/// from inside a borrow of `&self.converter_*`).
/// Returns `(target_clicked, open_clicked)` — first slot is the
/// version whose target circle was clicked, second is the version
/// whose tiny folder-open icon was clicked. Both are independent so
/// callers can handle them in either order.
fn render_versions_list(
    ui: &mut egui::Ui,
    height: f32,
    index: &ConverterVersionIndex,
    target: Option<&str>,
) -> (Option<String>, Option<String>) {
    const CIRCLE_COL_W: f32 = 15.0;
    const OPEN_ICON_W: f32 = 14.0;

    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    let mut clicked: Option<String> = None;
    let mut open_clicked: Option<String> = None;

    let mut versions: Vec<&mod_converter::ConverterVersionInfo> =
        index.versions.values().collect();
    versions.sort_by(|a, b| a.version.cmp(&b.version));

    egui::Frame::new()
        .stroke(border)
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.set_min_height(height);
            ui.set_max_height(height);
            ui.set_min_width(ui.available_width());

            let inner_w = ui.available_width();
            let cell_h = ui.spacing().interact_size.y;
            let scroll = &ui.spacing().scroll;
            let scrollbar_w =
                scroll.bar_width + scroll.bar_inner_margin + scroll.bar_outer_margin;
            let content_w = (inner_w - scrollbar_w).max(0.0);
            let remaining_w = (content_w - CIRCLE_COL_W).max(0.0);
            let version_col_w = (remaining_w * 0.5).floor();
            let status_col_w = remaining_w - version_col_w;

            // Sticky header — same shape as render_list_view's, plus
            // a blank slot above the circle column.
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.horizontal(|ui| {
                    ui.add_sized([CIRCLE_COL_W, cell_h], egui::Label::new(""));
                    ui.add_sized(
                        [version_col_w, cell_h],
                        egui::Label::new(egui::RichText::new("Game version").strong()),
                    );
                    ui.add_sized(
                        [status_col_w, cell_h],
                        egui::Label::new(egui::RichText::new("Status").strong()),
                    );
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .id_salt("converter_versions_scroll")
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    egui::Grid::new("converter_versions_grid")
                        .num_columns(3)
                        .striped(true)
                        .min_col_width(0.0)
                        .spacing([0.0, ui.spacing().item_spacing.y])
                        .show(ui, |ui| {
                            for v in &versions {
                                let is_target = target == Some(v.version.as_str());
                                if circle_button(
                                    ui,
                                    [CIRCLE_COL_W, cell_h],
                                    is_target,
                                )
                                .clicked()
                                {
                                    clicked = Some(v.version.clone());
                                }
                                let status = match (
                                    v.html_file_info.as_ref(),
                                    v.game_data.as_ref(),
                                ) {
                                    (None, _) => "Missing",
                                    (Some(_), None) => "Present",
                                    (Some(_), Some(Ok(_))) => "Loaded",
                                    (Some(_), Some(Err(_))) => "Failed",
                                };
                                let openable = matches!(status, "Present" | "Loaded");
                                ui.add_sized(
                                    [version_col_w, cell_h],
                                    egui::Label::new(v.version.as_str()),
                                );
                                // Status cell: tiny folder-open icon
                                // (or blank gutter so the labels stay
                                // aligned across rows) followed by the
                                // status text. Both children carry
                                // their exact widths so the cell adds
                                // up to status_col_w and the row
                                // matches the header.
                                ui.scope(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    ui.horizontal(|ui| {
                                        if openable {
                                            if folder_open_button(
                                                ui,
                                                [OPEN_ICON_W, cell_h],
                                            )
                                            .on_hover_text(
                                                "Extract & open this version's source",
                                            )
                                            .clicked()
                                            {
                                                open_clicked = Some(v.version.clone());
                                            }
                                        } else {
                                            ui.add_space(OPEN_ICON_W);
                                        }
                                        let remaining =
                                            (status_col_w - OPEN_ICON_W).max(0.0);
                                        ui.add_sized(
                                            [remaining, cell_h],
                                            egui::Label::new(status),
                                        );
                                    });
                                });
                                ui.end_row();
                            }
                        });
                });
        });

    (clicked, open_clicked)
}

/// Tiny click-able 10×10 folder-open icon used in the version list's
/// Status cell. Painted manually (rather than relying on a font glyph)
/// so it stays crisp at this size: a small folder body with a tab
/// notch on the top-left edge.
fn folder_open_button(ui: &mut egui::Ui, size: [f32; 2]) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::from(size), egui::Sense::click());
    let visuals = ui.visuals();
    let base = visuals.widgets.inactive.fg_stroke.color;
    let color = if response.hovered() {
        visuals.widgets.hovered.fg_stroke.color
    } else {
        base
    };
    let painter = ui.painter();
    let centre = rect.center();
    // 10x10 icon area centred in the allocated rect.
    let half = 5.0;
    let body = egui::Rect::from_min_max(
        egui::pos2(centre.x - half, centre.y - half + 2.0),
        egui::pos2(centre.x + half, centre.y + half - 1.0),
    );
    let tab = egui::Rect::from_min_max(
        egui::pos2(centre.x - half, centre.y - half),
        egui::pos2(centre.x - half + 4.0, centre.y - half + 2.0),
    );
    let stroke = egui::Stroke::new(1.0, color);
    painter.rect_stroke(tab, 0.0, stroke, egui::StrokeKind::Inside);
    painter.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
    response
}

/// Tiny click-able circle widget used as the target-version selector.
/// Filled when `selected`, hollow otherwise. Brighter on hover so
/// it's discoverable as interactive even though it has no label.
fn circle_button(ui: &mut egui::Ui, size: [f32; 2], selected: bool) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::from(size), egui::Sense::click());
    let visuals = ui.visuals();
    let base = visuals.widgets.inactive.fg_stroke;
    let color = if response.hovered() {
        visuals.widgets.hovered.fg_stroke.color
    } else {
        base.color
    };
    let center = rect.center();
    let radius = (size[0].min(size[1]) * 0.32).floor();
    let painter = ui.painter();
    if selected {
        painter.circle_filled(center, radius, color);
    } else {
        painter.circle_stroke(center, radius, egui::Stroke::new(base.width, color));
    }
    response
}

/// Mod-converter "Mods" list — same shape as `render_versions_list`
/// but with four columns. Reads directly from `ConverterModIndex`:
/// rows sort by game version, then by mod name within each version.
/// Returns `(open_clicked, edit_clicked)` — both are HashMap keys
/// (file name including extension). The Type cell carries a tiny
/// folder-open icon followed by the diff/mod label; the Game version
/// cell carries a tiny pencil icon followed by the version string.
fn render_mods_list(
    ui: &mut egui::Ui,
    height: f32,
    index: &ConverterModIndex,
) -> (Option<String>, Option<String>) {
    use mod_converter::{ConverterModInfo, ConverterModStatus, ConverterModType};

    const OPEN_ICON_W: f32 = 14.0;
    const EDIT_ICON_W: f32 = 14.0;
    const COLUMN_RATIOS: [f32; 4] = [0.5, 0.1, 0.2, 0.2];
    const HEADERS: [&str; 4] = ["Mod name", "Type", "Game version", "Status"];

    let mut entries: Vec<(&String, &ConverterModInfo)> = index.mods.iter().collect();
    entries.sort_by(|a, b| {
        a.1.game_version
            .cmp(&b.1.game_version)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });

    let border = ui.visuals().widgets.noninteractive.bg_stroke;
    let mut open_clicked: Option<String> = None;
    let mut edit_clicked: Option<String> = None;

    egui::Frame::new()
        .stroke(border)
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.set_min_height(height);
            ui.set_max_height(height);
            ui.set_min_width(ui.available_width());

            let inner_w = ui.available_width();
            let cell_h = ui.spacing().interact_size.y;
            let scroll = &ui.spacing().scroll;
            let scrollbar_w =
                scroll.bar_width + scroll.bar_inner_margin + scroll.bar_outer_margin;
            let content_w = (inner_w - scrollbar_w).max(0.0);

            let mut col_widths = [0.0_f32; 4];
            let mut consumed = 0.0_f32;
            for i in 0..3 {
                let w = (content_w * COLUMN_RATIOS[i]).floor();
                col_widths[i] = w;
                consumed += w;
            }
            col_widths[3] = content_w - consumed;

            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.horizontal(|ui| {
                    for (h, &w) in HEADERS.iter().zip(col_widths.iter()) {
                        ui.add_sized(
                            [w, cell_h],
                            egui::Label::new(egui::RichText::new(*h).strong()),
                        );
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical()
                .id_salt("converter_mods_scroll")
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    egui::Grid::new("converter_mods_grid")
                        .num_columns(4)
                        .striped(true)
                        .min_col_width(0.0)
                        .spacing([0.0, ui.spacing().item_spacing.y])
                        .show(ui, |ui| {
                            for (file_name, m) in &entries {
                                let kind = match m.mod_type {
                                    ConverterModType::Diff => "diff",
                                    ConverterModType::Mod => "mod",
                                };
                                let status = match m.status {
                                    ConverterModStatus::Unprocessed => "Unprocessed",
                                    ConverterModStatus::Converted => "Converted",
                                    ConverterModStatus::Failed => "Failed",
                                };
                                ui.add_sized(
                                    [col_widths[0], cell_h],
                                    egui::Label::new(m.name.as_str()),
                                );
                                // Type cell: tiny folder-open icon
                                // followed by the diff/mod text.
                                // Same exact-widths trick as the
                                // versions-list Status column so the
                                // row total matches the header.
                                ui.scope(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    ui.horizontal(|ui| {
                                        if folder_open_button(
                                            ui,
                                            [OPEN_ICON_W, cell_h],
                                        )
                                        .on_hover_text(
                                            "Open this mod file in the text editor",
                                        )
                                        .clicked()
                                        {
                                            open_clicked = Some((*file_name).clone());
                                        }
                                        let remaining =
                                            (col_widths[1] - OPEN_ICON_W).max(0.0);
                                        ui.add_sized(
                                            [remaining, cell_h],
                                            egui::Label::new(kind),
                                        );
                                    });
                                });
                                // Game version cell: pencil edit
                                // icon + version text. Same exact-
                                // widths trick so the column total
                                // stays aligned with the header.
                                ui.scope(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    ui.horizontal(|ui| {
                                        if pencil_button(
                                            ui,
                                            [EDIT_ICON_W, cell_h],
                                        )
                                        .on_hover_text(
                                            "Change this mod's game version",
                                        )
                                        .clicked()
                                        {
                                            edit_clicked = Some((*file_name).clone());
                                        }
                                        let remaining =
                                            (col_widths[2] - EDIT_ICON_W).max(0.0);
                                        ui.add_sized(
                                            [remaining, cell_h],
                                            egui::Label::new(m.game_version.as_str()),
                                        );
                                    });
                                });
                                ui.add_sized(
                                    [col_widths[3], cell_h],
                                    egui::Label::new(status),
                                );
                                ui.end_row();
                            }
                        });
                });
        });

    (open_clicked, edit_clicked)
}

/// Tiny click-able 10×10 pencil icon used as the "edit" affordance
/// in the mods list's Game version cell. Painted as a thick diagonal
/// "shaft" with a small filled triangle at the top-right tip; the
/// tip + body together read as "pen/pencil" at this size, distinct
/// from the folder-open icon used in the same list.
fn pencil_button(ui: &mut egui::Ui, size: [f32; 2]) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::Vec2::from(size), egui::Sense::click());
    let visuals = ui.visuals();
    let base = visuals.widgets.inactive.fg_stroke.color;
    let color = if response.hovered() {
        visuals.widgets.hovered.fg_stroke.color
    } else {
        base
    };
    let painter = ui.painter();
    let centre = rect.center();
    // Diagonal pencil body (bottom-left to top-right).
    let body_start = egui::pos2(centre.x - 4.0, centre.y + 4.0);
    let body_end = egui::pos2(centre.x + 2.0, centre.y - 2.0);
    painter.line_segment([body_start, body_end], egui::Stroke::new(2.0, color));
    // Pencil tip — small filled triangle past the body's top-right
    // end pointing further up-right.
    let tip = vec![
        egui::pos2(centre.x + 1.5, centre.y - 2.5),
        egui::pos2(centre.x + 4.5, centre.y - 0.5),
        egui::pos2(centre.x + 0.5, centre.y - 4.5),
    ];
    painter.add(egui::Shape::convex_polygon(tip, color, egui::Stroke::NONE));
    // Eraser-end nub at the bottom-left so the body reads as a
    // discrete pencil rather than a plain diagonal slash.
    painter.line_segment(
        [
            egui::pos2(centre.x - 4.5, centre.y + 3.5),
            egui::pos2(centre.x - 2.5, centre.y + 4.5),
        ],
        egui::Stroke::new(1.5, color),
    );
    response
}

