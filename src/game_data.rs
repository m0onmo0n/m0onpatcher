// ============================================================
// Structure definitions (from GameDataStructure.kt)
// ============================================================

pub struct UserDataType {
    pub log_name: &'static str,
    pub directory: &'static str,
    pub html_id: &'static str,
    pub default_file_name: &'static str,
    pub naming_matcher: fn(&str) -> bool,
}

#[derive(Debug)]
pub struct ModuleType {
    pub directory_name: &'static str,
    pub file_extension: &'static str,
    pub start_tag_prefix: &'static str,
    pub start_tag_suffix: &'static str,
    pub end_tag: &'static str,
    pub naming_matcher: fn(&str) -> bool,
}

pub const CSS_TYPE: UserDataType = UserDataType {
    log_name: "Stylesheet",
    directory: "css",
    html_id: "twine-user-stylesheet",
    default_file_name: "style.css",
    naming_matcher: |n| n.ends_with(".css") || n.ends_with(".ttf"),
};

pub const JS_TYPE: UserDataType = UserDataType {
    log_name: "Script",
    directory: "js",
    html_id: "twine-user-script",
    default_file_name: "script.js",
    naming_matcher: |n| n.ends_with(".js"),
};

pub const CSS_MODULE_TYPE: ModuleType = ModuleType {
    directory_name: "style-module",
    file_extension: "css",
    start_tag_prefix: "<style id=\"style-module-",
    start_tag_suffix: "\" type=\"text/css\">",
    end_tag: "</style>",
    naming_matcher: |n| n.ends_with(".css"),
};

pub const JS_MODULE_TYPE: ModuleType = ModuleType {
    directory_name: "script-module",
    file_extension: "js",
    start_tag_prefix: "<script id=\"script-module-",
    start_tag_suffix: "\" type=\"text/javascript\">",
    end_tag: "</script>",
    naming_matcher: |n| n.ends_with(".js"),
};

pub const PASSAGE_DIRECTORY: &str = "passage";
pub const PASSAGE_HTML_TAG: &str = "tw-passagedata";
pub const PASSAGE_EXTENSION: &str = "twee";

pub const SYSTEM_SCRIPTS_DIRECTORY: &str = "system-scripts";
pub const SYSTEM_LIBRARIES_FILE_NAME: &str = "libraries.js";
pub const SYSTEM_SCRIPT_FILE_NAME: &str = "sugarcube.js";

// ============================================================
// Data types (from GameData.kt)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HtmlTagRanges {
    pub inner: TextRange,
    pub outer: TextRange,
}

#[derive(Debug)]
pub struct GameData {
    pub document_raw_text: String,
    pub styles: UserData,
    pub scripts: UserData,
    pub modules: Option<Modules>,
    pub passages: Vec<Passage>,
    pub csp_range: Option<TextRange>,
    pub body_start: usize,
    pub system_components: SystemModules,
}

/// SugarCube engine pieces lifted out of the HTML when present in the
/// expected positions: `script_libraries` lives in `<head>` ahead of any
/// CSP / module block, and `sugarcube-script` lives at end-of-document
/// past the last passage. Anything outside those slots is left in place
/// (and not surfaced here) so the merger doesn't try to round-trip a
/// custom mid-document `<script>` we wouldn't know how to splice back.
#[derive(Debug, Default)]
pub struct SystemModules {
    pub sugarcube_libraries: Option<HtmlTagRanges>,
    pub sugarcube_script: Option<HtmlTagRanges>,
}

#[derive(Debug)]
pub struct UserData {
    pub user_data_raw_text: String,
    pub entries: Vec<UserDataEntry>,
    pub is_split: bool,
    pub ranges: HtmlTagRanges,
}

#[derive(Debug)]
pub struct UserDataEntry {
    pub disk_file_name: String,
    pub file_name: String,
    pub index: usize,
    pub raw_text_range: TextRange,
}

#[derive(Debug)]
pub struct Modules {
    pub html_range: TextRange,
    pub script_modules: ModulesOfType,
    pub style_modules: ModulesOfType,
}

#[derive(Debug)]
pub struct ModulesOfType {
    pub module_type: &'static ModuleType,
    pub entries: Vec<ModuleEntry>,
}

#[derive(Debug)]
pub struct ModuleEntry {
    pub original_module_name: String,
    pub module_name: String,
    pub ranges: HtmlTagRanges,
}

#[derive(Debug)]
pub struct Passage {
    pub name: String,
    pub attributes: PassageAttributes,
    pub tags: Vec<String>,
    pub is_widget: bool,
    pub ranges: HtmlTagRanges,
    pub decoded_text: String,
}

#[derive(Debug)]
pub struct PassageAttributes {
    pub tags: Option<String>,
    pub position: Option<String>,
    pub pid: Option<String>,
}
