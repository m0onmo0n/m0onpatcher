//! About tab: what this program is, who owns what, and the people it builds on.

use eframe::egui;

use crate::gui::{app_version, titled_box, App, TITLE_GREY};

impl App {
    pub(crate) fn render_about(&mut self, ui: &mut egui::Ui) {
        titled_box(ui, "About", TITLE_GREY, &mut |ui| {
            ui.heading(format!("M0on's packer {}", app_version()));
            ui.add_space(4.0);
            ui.label(
                "A fusion of the m0on-kittypatcher v0.7.6 fork and ScRepacker v0.1.5, for modding Course of Temptation (SugarCube / Twine):",
            );
            ui.label("  \u{2022} ScRepacker v0.1.5, which extracts the game HTML into editable sources, applies .diff mods and repacks it, and");
            ui.label("  \u{2022} the m0on-kittypatcher v0.7.6 fork, which applies .mod files, rewritten in Rust as the built-in \"kitty-engine\".");
            ui.add_space(4.0);
            ui.label(
                "One program now handles both mod formats on its own. An external KittyPatcher is \
                 optional (see \"Patch engine\" on the Config tab).",
            );
            ui.horizontal_wrapped(|ui| {
                ui.label("Source (branches packer and patcher; patcher is the standalone patcher without the packer):");
                ui.hyperlink_to("github.com/m0onmo0n/m0onpatcher", "https://github.com/m0onmo0n/m0onpatcher");
            });

            ui.add_space(10.0);
            ui.strong("Copyright");
            ui.label("Original parts of this fork: Copyright (c) 2026 m0on. All rights reserved.");
            ui.label(
                "ScRepacker and KittyPatcher are published without a licence and their authors keep all \
                 rights in their work. They have given m0on their permission to build on it and to \
                 distribute this program; that permission covers this program only.",
            );

            ui.add_space(10.0);
            ui.strong("Credits");
            ui.horizontal_wrapped(|ui| {
                ui.label("\u{2022} nuqqur: ScRepacker, the extractor, packer and interface this program is built on");
                ui.hyperlink_to("(screpacker-rs)", "https://gitgud.io/nuqqur/screpacker-rs/");
            });
            ui.label("\u{2022} LK / kitty: original creator of KittyPatcher (0.1.5c and 0.2.0a)");
            ui.label("\u{2022} CoderMaverick: author of KittyPatcher v0.4.0, v0.6.0 and the v0.7.x versions, and of the KittyPatcher ecosystem");
            ui.label("\u{2022} The Course of Temptation modding community");
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Built with Rust, egui/eframe, regex and other open-source crates.",
                )
                .small(),
            );
        });
    }
}
