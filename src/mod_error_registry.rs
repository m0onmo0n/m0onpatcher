use std::collections::HashSet;

pub struct ModErrorRegistry {
    failed_info: Vec<FailureInfo>,
    failed_names: HashSet<String>,
    warning_files: HashSet<String>,
    success_count: u32,
}

#[derive(Debug, Clone)]
pub struct FailureInfo {
    pub mod_name: String,
    pub mod_diff_name: String,
    pub failure_reason: String,
}

impl ModErrorRegistry {
    pub fn new() -> Self {
        Self {
            failed_info: Vec::new(),
            failed_names: HashSet::new(),
            warning_files: HashSet::new(),
            success_count: 0,
        }
    }

    pub fn record_success(&mut self) {
        self.success_count += 1;
    }

    pub fn record_failure(&mut self, mod_name: &str, diff_file_name: &str, failure_reason: &str) {
        self.failed_info.push(FailureInfo {
            mod_name: mod_name.to_string(),
            mod_diff_name: diff_file_name.to_string(),
            failure_reason: failure_reason.to_string(),
        });
        self.failed_names.insert(mod_name.to_string());
    }

    pub fn failures(&self) -> &[FailureInfo] {
        &self.failed_info
    }

    pub fn success_count(&self) -> u32 {
        self.success_count
    }

    pub fn record_warning_for_file(&mut self, relative_path: &str) {
        self.warning_files.insert(relative_path.to_string());
    }

    pub fn files_with_warnings(&self) -> &HashSet<String> {
        &self.warning_files
    }
}
