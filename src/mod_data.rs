use std::collections::BTreeSet;

#[derive(Debug)]
pub struct ModData {
    pub mod_name: String,
    pub diff_file_name: String,
    pub dependencies: Vec<String>,
    pub diff_text: String,
    pub modified_files: BTreeSet<String>,
    /// Per-mod override for the fuzzy-apply tolerance (0..=2). None
    /// means "use the default" — the direct-apply path runs strict,
    /// so context drift falls through to 3-way merge.
    pub max_fuzz: Option<u32>,
}
