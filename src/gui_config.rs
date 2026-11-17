//! Config-tab specific GUI code: render function for the Config
//! panel, drag&drop handler that picks up KittyPatcher binaries, and
//! the App methods that drive them. Lives alongside `gui.rs`, which
//! keeps the shared App state, common widget helpers, and the
//! Output panel.

use std::path::Path;

use eframe::egui;

use crate::config;
use crate::gui::{App, TITLE_GREY, titled_box};
use crate::tool_paths::{self, LocationSource};

/// State backing the "Edit command" modal. Carries the in-progress
/// edit list, a tag pointing at which App field to commit to on OK,
/// and the title shown in the window's header. The list is mutated
/// in place across frames; OK trims trailing empty entries before
/// writing back, Cancel discards.
pub(crate) struct CommandEditDialog {
    pub(crate) target: CommandEditTarget,
    pub(crate) args: Vec<String>,
    pub(crate) title: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CommandEditTarget {
    /// Writes back into `Config::directory_edit_command` and
    /// persists the config + refreshes derived tool paths.
    DirectoryEdit,
    /// Writes back into `Config::text_edit_command`, same
    /// persistence semantics as DirectoryEdit.
    TextEdit,
}

impl App {
    /// Drag-and-drop handler used while the Config tab is active.
    /// Only drops we can map onto a config field are honoured: a
    /// KittyPatcher `.py` script or `.exe` sets the corresponding
    /// path. Anything else is rejected with a warning.
    pub(crate) fn handle_config_drop(&mut self, path: &Path) {
        if path.is_dir() {
            log::warn!(
                "Drag&drop on Config tab doesn't accept directories: {}",
                path.display()
            );
            return;
        }
        let lower_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if (lower_name.ends_with(".py") || lower_name.ends_with(".exe"))
            && lower_name.contains("kittypatcher")
        {
            self.set_kittypatcher_path(path);
            return;
        }
        log::warn!(
            "Only a KittyPatcher .py or .exe file can be dropped here: {}",
            path.display()
        );
    }

    /// Persist a config entry pointing at the given KittyPatcher
    /// binary, and refresh the cached tool paths so the Config panel
    /// and the Patch button's status label both reflect the change
    /// on the next frame.
    pub(crate) fn set_kittypatcher_path(&mut self, path: &Path) {
        self.config.kittypatcher_path = path.to_string_lossy().into_owned();
        log::info!("KittyPatcher path set to {} via drag&drop", path.display());
        if let Err(err) = config::save(&self.config) {
            log::error!("Failed to save config: {err:#}");
        }
        self.refresh_tool_paths();
    }

    pub(crate) fn render_config(&mut self, ui: &mut egui::Ui) {
        let border = ui.visuals().widgets.noninteractive.bg_stroke;
        // Width reserved for the label column. Picked to fit the
        // longest label ("KittyPatcher:") plus a small cushion;
        // sharing it across rows aligns all path frames at the same
        // X regardless of label text width.
        let config_label_width: f32 = 110.0;

        let browse_path_row = |ui: &mut egui::Ui,
                               label: &str,
                               value: &mut String,
                               display_override: Option<String>,
                               tooltip: Option<&str>|
         -> bool {
            let mut changed = false;
            ui.horizontal(|ui| {
                let row_height = ui.spacing().interact_size.y;
                let label_widget = egui::Label::new(label).sense(egui::Sense::hover());
                let label_resp = ui.add_sized(
                    egui::vec2(config_label_width, row_height),
                    label_widget,
                );
                if let Some(text) = tooltip {
                    label_resp
                        .on_hover_cursor(egui::CursorIcon::Help)
                        .on_hover_text(text);
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .add_enabled(
                                !value.is_empty(),
                                egui::Button::new("Reset"),
                            )
                            .clicked()
                        {
                            value.clear();
                            changed = true;
                        }
                        if ui.button("Browse…").clicked() {
                            if let Some(picked) = rfd::FileDialog::new().pick_file() {
                                *value = picked.to_string_lossy().into_owned();
                                changed = true;
                            }
                        }
                        let display = if let Some(text) = display_override {
                            egui::RichText::new(text)
                        } else if value.is_empty() {
                            egui::RichText::new("(not set)")
                        } else {
                            egui::RichText::new(value.as_str())
                        };
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
                                            egui::Label::new(display).truncate(),
                                        );
                                    },
                                );
                            });
                    },
                );
            });
            changed
        };

        titled_box(ui, "Config", TITLE_GREY, &mut |ui| {
            let mut changed = false;

            // ---- patch engine selector ----
            let mut mode = self.config.engine_mode();
            ui.horizontal(|ui| {
                let row_height = ui.spacing().interact_size.y;
                let label = egui::Label::new("Patch engine:").sense(egui::Sense::hover());
                ui.add_sized(egui::vec2(config_label_width, row_height), label)
                    .on_hover_cursor(egui::CursorIcon::Help)
                    .on_hover_text(concat!(
                        "Which engine applies .mod files. The internal engine is built in ",
                        "and needs no KittyPatcher. \"KittyPatcher as fallback\" runs ",
                        "KittyPatcher only for mod sets the internal engine cannot handle yet."
                    ));
                egui::ComboBox::from_id_salt("patch_engine")
                    .selected_text(mode.label())
                    .width(300.0)
                    .show_ui(ui, |ui| {
                        for m in config::EngineMode::ALL {
                            ui.selectable_value(&mut mode, m, m.label());
                        }
                    });
            });
            if mode.config_value() != self.config.engine {
                self.config.engine = mode.config_value().to_string();
                changed = true;
            }
            match mode {
                config::EngineMode::Internal => {
                    ui.horizontal(|ui| {
                        ui.add_space(config_label_width + ui.spacing().item_spacing.x);
                        ui.label("Internal engine is selected: KittyPatcher is not used.");
                    });
                }
                config::EngineMode::InternalFallback => {
                    ui.horizontal(|ui| {
                        ui.add_space(config_label_width + ui.spacing().item_spacing.x);
                        ui.label("KittyPatcher is only called if the internal engine cannot handle a mod set.");
                    });
                }
                config::EngineMode::External => {}
            }

            let show_kp = mode != config::EngineMode::Internal;
            let kp_display = Some(self.kittypatcher_tool.display_label());
            let kp_tooltip = concat!(
                "Path to the KittyPatcher executable or Python script. Only needed when the ",
                "Patch engine above is set to use KittyPatcher (as the fallback or as the only engine). ",
                "If you pick a Python script (.py), a Python interpreter must also be found or set. ",
                "Autodetection takes the first file with \"kittypatcher\" in its name that sits next to ",
                "the HTML file or next to this program; if it picks the wrong one, set the path yourself ",
                "or remove the extra files.",
            );
            if show_kp {
                changed |= browse_path_row(
                    ui,
                    "KittyPatcher:",
                    &mut self.config.kittypatcher_path,
                    kp_display,
                    Some(kp_tooltip),
                );

                let py_display = Some(self.python_tool.display_label());
                let py_tooltip = concat!(
                    "Path to the Python interpreter to use if using a .py ",
                    "version of KittyPatcher. If KittyPatcher is used as .exe, ",
                    "then this does not need to be set.",
                );
                changed |= browse_path_row(
                    ui,
                    "Python:",
                    &mut self.config.python_path,
                    py_display,
                    Some(py_tooltip),
                );
            }

            let browser_display = if self.config.browser_path.is_empty() {
                Some("(system default)".to_string())
            } else {
                None
            };
            let browser_tooltip = concat!(
                "Path to the web browser executable to use for \"Open\" ",
                "actions. Uses system default for .html files if not set.",
            );
            changed |= browse_path_row(
                ui,
                "Browser:",
                &mut self.config.browser_path,
                browser_display,
                Some(browser_tooltip),
            );
            if changed {
                if let Err(err) = config::save(&self.config) {
                    log::error!("Failed to save config: {err:#}");
                }
                self.refresh_tool_paths();
            }

            // Directory editor: configurable argv list (e.g.
            // ["code", "-n"]) used to open project directories in
            // an external editor. Read-only display reflects the
            // resolved command (config wins, otherwise autodetect)
            // so the user sees what would actually be launched.
            let editor_cmd = tool_paths::tool_command_directory_edit(&self.config);
            let mut open_edit = false;
            ui.horizontal(|ui| {
                let row_height = ui.spacing().interact_size.y;
                let label_widget =
                    egui::Label::new("Directory editor:").sense(egui::Sense::hover());
                ui.add_sized(
                    egui::vec2(config_label_width, row_height),
                    label_widget,
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .add_enabled(
                                self.command_edit_dialog.is_none(),
                                egui::Button::new("Edit"),
                            )
                            .clicked()
                        {
                            open_edit = true;
                        }
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
                                        render_resolved_command_inline(
                                            ui,
                                            editor_cmd.as_ref(),
                                        );
                                    },
                                );
                            });
                    },
                );
            });
            if open_edit {
                self.open_command_edit(
                    CommandEditTarget::DirectoryEdit,
                    self.config.directory_edit_command.clone(),
                    "Edit directory editor command".to_string(),
                );
            }

            // Text editor: same shape as the directory-editor row
            // above, just bound to `text_edit_command` and resolved
            // via `tool_command_text_edit`.
            let text_cmd = tool_paths::tool_command_text_edit(&self.config);
            let mut open_text_edit = false;
            ui.horizontal(|ui| {
                let row_height = ui.spacing().interact_size.y;
                let label_widget =
                    egui::Label::new("Text editor:").sense(egui::Sense::hover());
                ui.add_sized(
                    egui::vec2(config_label_width, row_height),
                    label_widget,
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .add_enabled(
                                self.command_edit_dialog.is_none(),
                                egui::Button::new("Edit"),
                            )
                            .clicked()
                        {
                            open_text_edit = true;
                        }
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
                                        render_resolved_command_inline(
                                            ui,
                                            text_cmd.as_ref(),
                                        );
                                    },
                                );
                            });
                    },
                );
            });
            if open_text_edit {
                self.open_command_edit(
                    CommandEditTarget::TextEdit,
                    self.config.text_edit_command.clone(),
                    "Edit text editor command".to_string(),
                );
            }
        });

        let extra_vspace =
            (ui.spacing().item_spacing.x - ui.spacing().item_spacing.y).max(0.0);
        ui.add_space(extra_vspace);
    }

    fn open_command_edit(
        &mut self,
        target: CommandEditTarget,
        args: Vec<String>,
        title: String,
    ) {
        self.command_edit_dialog = Some(CommandEditDialog {
            target,
            args,
            title,
        });
    }

    /// Modal "Edit command" dialog. Each argv entry gets its own row
    /// with a text editor + Browse… file picker (since arguments are
    /// often paths too). The trailing row is always blank — typing
    /// in it spawns a new blank row below on the next frame, so
    /// adding entries needs no explicit "+" button. Per-row × removes
    /// the entry; trailing blanks are trimmed on OK.
    pub(crate) fn render_command_edit_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.command_edit_dialog.take() else {
            return;
        };
        let mut commit = false;
        let mut close = false;

        // Make sure there's always a trailing empty row so the user
        // can keep typing without hitting a "+ Add" button.
        if dialog.args.last().map_or(true, |s| !s.is_empty()) {
            dialog.args.push(String::new());
        }

        egui::Window::new(&dialog.title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .min_width(520.0)
            .show(ctx, |ui| {
                let mut to_remove: Option<usize> = None;
                let last_idx = dialog.args.len().saturating_sub(1);
                // Fixed label-cell width so "binary:" and "arg:"
                // don't shift the text editors / Browse buttons
                // between rows. ~52px fits "binary:" comfortably at
                // egui's default proportional font.
                let label_col_width = 52.0_f32;
                let row_height = ui.spacing().interact_size.y;
                for (i, arg) in dialog.args.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let label = if i == 0 { "binary:" } else { "arg:" };
                        ui.add_sized(
                            egui::vec2(label_col_width, row_height),
                            egui::Label::new(label),
                        );
                        ui.add(
                            egui::TextEdit::singleline(arg).desired_width(360.0),
                        );
                        if ui.button("Browse…").clicked() {
                            if let Some(picked) = rfd::FileDialog::new().pick_file() {
                                *arg = picked.to_string_lossy().into_owned();
                            }
                        }
                        // Skip the × on the auto-spawned trailing
                        // empty row — clicking it would just kill the
                        // row that's about to come back next frame.
                        let is_trailing_blank = i == last_idx && arg.is_empty();
                        if ui
                            .add_enabled(!is_trailing_blank, egui::Button::new("×"))
                            .clicked()
                        {
                            to_remove = Some(i);
                        }
                    });
                }
                if let Some(i) = to_remove {
                    dialog.args.remove(i);
                }

                ui.add_space(4.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });

        if commit {
            // Strip every empty entry (not just trailing) — the
            // auto-spawned blank row at the end always shows up
            // here, and any blank gaps inside the list are user
            // typos / a delete-then-undo path that would round-trip
            // to argv as bogus empty arguments.
            dialog.args.retain(|s| !s.is_empty());
            match dialog.target {
                CommandEditTarget::DirectoryEdit => {
                    self.config.directory_edit_command = dialog.args.clone();
                    if let Err(err) = config::save(&self.config) {
                        log::error!("Failed to save config: {err:#}");
                    }
                    self.refresh_tool_paths();
                    log::info!(
                        "Directory editor command set to: {}",
                        self.config.directory_edit_command.join(" ▸ ")
                    );
                }
                CommandEditTarget::TextEdit => {
                    self.config.text_edit_command = dialog.args.clone();
                    if let Err(err) = config::save(&self.config) {
                        log::error!("Failed to save config: {err:#}");
                    }
                    self.refresh_tool_paths();
                    log::info!(
                        "Text editor command set to: {}",
                        self.config.text_edit_command.join(" ▸ ")
                    );
                }
            }
            close = true;
        }

        if !close {
            self.command_edit_dialog = Some(dialog);
        }
    }
}

/// Inline read-only renderer for a resolved `ToolCommand`. `None`
/// shows as `(not set)`. When the command came from auto-detection
/// (no config entry), an `(autodetected)` prefix in the muted-grey
/// tone the existing config rows use makes the origin obvious.
/// Each argv entry that follows alternates between two text colours
/// so the boundary between them is visually clear without relying
/// on a separator glyph (some fonts don't render `▸` cleanly).
fn render_resolved_command_inline(
    ui: &mut egui::Ui,
    cmd: Option<&crate::tool_paths::ToolCommand>,
) {
    let Some(cmd) = cmd else {
        ui.label(egui::RichText::new("(not set)"));
        return;
    };
    if cmd.source == LocationSource::Autodetected {
        ui.label(
            egui::RichText::new("(autodetected)")
                .color(egui::Color32::from_gray(160)),
        );
    }
    let color_a = egui::Color32::from_rgb(220, 220, 220);
    let color_b = egui::Color32::from_rgb(150, 200, 230);
    let parts = std::iter::once(cmd.binary_path.as_str())
        .chain(cmd.arguments.iter().map(String::as_str));
    for (i, arg) in parts.enumerate() {
        let color = if i % 2 == 0 { color_a } else { color_b };
        ui.label(egui::RichText::new(arg).color(color));
    }
}
