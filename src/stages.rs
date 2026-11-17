//! Stage builder: an ordered list of Input HTML, Mods and Output HTML items that is run as a chain.
//!
//! ```text
//! Input HTML      CourseOfTemptation.html
//! Mods            Bits&Fun V_1.4.2.3_v3.mod
//! Output HTML     stage1.html            <- written to disk, and the input of what follows
//! Mods            stage 2\mods (folder)
//! Output HTML     CourseOfTemptation.modded.html
//! ```
//!
//! The Mods items between two Outputs form **one** patch run (like a stage folder): the mods load in list order
//! (a folder inside itself by `# Priority:` and name) and can see each other's changes. An Output closes the run,
//! is written to disk, and its result is patched further in memory by the items below it.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::Reporter;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    /// the game HTML to start from (must be the first item)
    Input(PathBuf),
    /// a mod file, or a folder of mod files
    Mods(PathBuf),
    /// where to write the result of the mods above it
    Output(PathBuf),
}

impl Item {
    pub fn path(&self) -> &Path {
        match self {
            Item::Input(p) | Item::Mods(p) | Item::Output(p) => p,
        }
    }
}

/// One patch run: the Mods items (in list order) and the file its result is written to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub mods: Vec<PathBuf>,
    pub output: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub input: PathBuf,
    pub stages: Vec<Stage>,
}

/// Check the list and turn it into stages. The error text is shown to the user as it is.
pub fn plan(items: &[Item]) -> Result<Plan, String> {
    let Some(first) = items.first() else {
        return Err("Add the input HTML first (Add input HTML, or drop the game .html here).".to_string());
    };
    let Item::Input(input) = first else {
        return Err("The first item must be the Input HTML: move it to the top.".to_string());
    };
    let mut stages: Vec<Stage> = Vec::new();
    let mut pending: Vec<PathBuf> = Vec::new();
    for (i, item) in items.iter().enumerate().skip(1) {
        match item {
            Item::Input(_) => return Err(format!("Item {}: there can be only one Input HTML, at the top.", i + 1)),
            Item::Mods(p) => pending.push(p.clone()),
            Item::Output(out) => {
                if pending.is_empty() {
                    return Err(format!("Item {}: this Output has no Mods above it (since the previous Output).", i + 1));
                }
                if out == input || stages.iter().any(|s| &s.output == out) {
                    return Err(format!("Item {}: {} is already used as the input or another output.", i + 1, out.display()));
                }
                stages.push(Stage { mods: std::mem::take(&mut pending), output: out.clone() });
            }
        }
    }
    if !pending.is_empty() {
        return Err("The Mods at the bottom have no Output below them: add an Output HTML.".to_string());
    }
    if stages.is_empty() {
        return Err("Add some Mods and an Output HTML below the input.".to_string());
    }
    Ok(Plan { input: input.clone(), stages })
}

/// Folder of the per-stage logs: `<output folder>\logs\<output name>`.
fn stage_log_dir(output: &Path) -> PathBuf {
    let stem = output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "stage".to_string());
    output.parent().map(|p| p.join("logs").join(&stem)).unwrap_or_else(|| PathBuf::from("logs").join(&stem))
}

/// Read an HTML file the way KittyPatcher does (UTF-8, universal newlines).
fn read_html(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| anyhow!("{} is not valid UTF-8", path.display()))?;
    Ok(kitty_engine::pytext::universal_newlines(&text))
}

/// Run the whole chain. Stops at the first stage that fails; the outputs written before it stay on disk.
///
/// Every stage reads the HTML it starts from (the input file, then the output file of the stage before), patches it,
/// and writes its output in the normalized form ScRepacker writes (see `normalize`).
pub fn run(plan: &Plan, legacy_replace_all: bool, r: &Reporter) -> anyhow::Result<String> {
    let started = Instant::now();
    let n = plan.stages.len();
    let opts = kitty_engine::load::LoadOptions { legacy_replace_all };
    let mut total_made = 0usize;
    // the file this stage starts from; it is also the reference document the output is merged into
    let mut reference = plan.input.clone();

    for (i, stage) in plan.stages.iter().enumerate() {
        let base = i as f32 / n as f32;
        let span = 1.0 / n as f32;
        r.line("INFO", format!("Reading {}", reference.display()));
        let html = read_html(&reference)?;

        let mut groups: Vec<Vec<PathBuf>> = Vec::new();
        for item in &stage.mods {
            if item.is_dir() {
                let files = kitty_engine::load::mod_files_in(item).with_context(|| format!("read {}", item.display()))?;
                groups.push(files);
            } else if item.is_file() {
                groups.push(vec![item.clone()]);
            } else {
                bail!("stage {}: {} does not exist", i + 1, item.display());
            }
        }
        let loaded = kitty_engine::load::load_mod_groups(&groups, &opts).with_context(|| format!("stage {}: load mods", i + 1))?;
        for w in &loaded.warnings {
            r.line("WARN", w);
        }
        if loaded.loaded_mods.is_empty() {
            bail!("stage {}: no .mod files found in its Mods items", i + 1);
        }
        r.line(
            "INFO",
            format!(
                "=== Stage {}/{n}: {} mod file(s) from {} item(s) -> {}",
                i + 1,
                loaded.loaded_mods.len(),
                stage.mods.len(),
                stage.output.display()
            ),
        );
        crate::log_legacy_summary(&loaded, r);
        r.progress(base + span * 0.03);

        let outcome = crate::apply_loaded(&html, &loaded, &stage_log_dir(&stage.output), r, base + span * 0.03, span * 0.94)
            .map_err(|e| anyhow!("stage {}/{n}: {e:#}", i + 1))?;
        total_made += outcome.replacements_made;
        r.line(
            "INFO",
            format!("Stage {}/{n} done: {} replacements made. Writing {}", i + 1, outcome.replacements_made, stage.output.display()),
        );
        crate::normalize::write_normalized(&reference, &outcome.html, &stage.output)
            .with_context(|| format!("stage {}/{n}: write {}", i + 1, stage.output.display()))?;
        reference = stage.output.clone();
        r.progress(base + span);
    }

    Ok(format!(
        "Done: {n} stage(s), {total_made} replacements made in {:.1} s. Final output: {}",
        started.elapsed().as_secs_f64(),
        plan.stages.last().map(|s| s.output.display().to_string()).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_valid_list_becomes_stages() {
        let items = vec![
            Item::Input(p("in.html")),
            Item::Mods(p("a.mod")),
            Item::Mods(p("mods2")),
            Item::Output(p("s1.html")),
            Item::Mods(p("b.mod")),
            Item::Output(p("s2.html")),
        ];
        let plan = plan(&items).unwrap();
        assert_eq!(plan.input, p("in.html"));
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.stages[0].mods, vec![p("a.mod"), p("mods2")]);
        assert_eq!(plan.stages[0].output, p("s1.html"));
        assert_eq!(plan.stages[1].mods, vec![p("b.mod")]);
    }

    #[test]
    fn bad_lists_are_explained() {
        assert!(plan(&[]).unwrap_err().contains("input HTML first"));
        assert!(plan(&[Item::Mods(p("a.mod"))]).unwrap_err().contains("first item must be the Input"));
        assert!(plan(&[Item::Input(p("in.html"))]).unwrap_err().contains("Add some Mods"));
        assert!(plan(&[Item::Input(p("in.html")), Item::Output(p("o.html"))]).unwrap_err().contains("no Mods above"));
        assert!(plan(&[Item::Input(p("in.html")), Item::Mods(p("a.mod"))]).unwrap_err().contains("no Output below"));
        assert!(plan(&[Item::Input(p("in.html")), Item::Mods(p("a.mod")), Item::Input(p("x.html"))]).unwrap_err().contains("only one Input"));
        assert!(plan(&[Item::Input(p("in.html")), Item::Mods(p("a.mod")), Item::Output(p("in.html"))]).unwrap_err().contains("already used"));
        let twice = [Item::Input(p("in.html")), Item::Mods(p("a.mod")), Item::Output(p("o.html")), Item::Mods(p("b.mod")), Item::Output(p("o.html"))];
        assert!(plan(&twice).unwrap_err().contains("already used"));
    }

    const HTML: &str = concat!(
        "<html><body><tw-storydata name=\"T\" startnode=\"1\" creator=\"Twine\" creator-version=\"2.6.2\" ifid=\"X\" zoom=\"1\" ",
        "format=\"SugarCube\" format-version=\"2.36.1\" options=\"\" hidden>",
        "<style role=\"stylesheet\" id=\"twine-user-stylesheet\" type=\"text/twine-css\"></style>",
        "<script role=\"script\" id=\"twine-user-script\" type=\"text/twine-javascript\">var a = 1;</script>",
        "<tw-passagedata pid=\"1\" name=\"Start\" tags=\"\" position=\"0,0\" size=\"100,100\">cat sat</tw-passagedata>",
        "</tw-storydata></body></html>"
    );

    fn reporter() -> (Reporter, Arc<Mutex<crate::Shared>>) {
        let shared = Arc::new(Mutex::new(crate::Shared {
            log: Vec::new(),
            progress: 0.0,
            indeterminate: false,
            task: String::new(),
            status: crate::Status::Idle,
        }));
        (Reporter { shared: shared.clone(), ctx: eframe::egui::Context::default(), echo: crate::Echo::Off }, shared)
    }

    #[test]
    fn the_second_stage_works_on_the_result_of_the_first() {
        let dir = std::env::temp_dir().join(format!("m0on-stages-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mods2")).unwrap();
        std::fs::write(dir.join("in.html"), HTML).unwrap();
        // stage 1: one picked file; stage 2: a folder whose mod only matches text that stage 1 created
        std::fs::write(dir.join("a.mod"), "cat sat\n~\nkitten purred\n~~\n").unwrap();
        std::fs::write(dir.join("mods2").join("b.mod"), "Replace:\nkitten purred\nWith:\nkitten purred loudly\n").unwrap();
        let items = vec![
            Item::Input(dir.join("in.html")),
            Item::Mods(dir.join("a.mod")),
            Item::Output(dir.join("stage1.html")),
            Item::Mods(dir.join("mods2")),
            Item::Output(dir.join("stage2.html")),
        ];
        let plan = plan(&items).unwrap();
        let (r, shared) = reporter();
        let msg = run(&plan, false, &r).unwrap_or_else(|e| panic!("{e:#}\n{:?}", shared.lock().unwrap().log));
        assert!(msg.contains("2 stage(s)"), "{msg}");
        let s1 = std::fs::read_to_string(dir.join("stage1.html")).unwrap();
        let s2 = std::fs::read_to_string(dir.join("stage2.html")).unwrap();
        assert!(s1.contains(">kitten purred</tw-passagedata>"), "stage 1 output");
        assert!(s2.contains(">kitten purred loudly</tw-passagedata>"), "stage 2 output");
        // one log folder per stage
        assert!(dir.join("logs").join("stage1").join("ModPatchLog.txt").is_file());
        assert!(dir.join("logs").join("stage2").join("FailsPatchLog.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_stage_stops_the_chain_and_keeps_earlier_outputs() {
        let dir = std::env::temp_dir().join(format!("m0on-stages-test-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in.html"), HTML).unwrap();
        std::fs::write(dir.join("a.mod"), "Replace:\ncat sat\nWith:\ndog sat\n").unwrap();
        std::fs::write(dir.join("b.mod"), "Replace:\nthis text does not exist\nWith:\nx\n").unwrap();
        let items = vec![
            Item::Input(dir.join("in.html")),
            Item::Mods(dir.join("a.mod")),
            Item::Output(dir.join("stage1.html")),
            Item::Mods(dir.join("b.mod")),
            Item::Output(dir.join("stage2.html")),
        ];
        let (r, _shared) = reporter();
        let err = run(&plan(&items).unwrap(), false, &r).unwrap_err();
        assert!(format!("{err:#}").contains("stage 2/2"), "{err:#}");
        assert!(dir.join("stage1.html").is_file());
        assert!(!dir.join("stage2.html").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real data: `M0ON_REAL_HTML`, `M0ON_REAL_MODS1`, `M0ON_REAL_MODS2` (folders) and `M0ON_REAL_OUT` (folder).
    /// Run with `cargo test -p m0onpatcher real_two_stage -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_two_stage_chain() {
        let get = |k: &str| PathBuf::from(std::env::var(k).unwrap_or_else(|_| panic!("set {k}")));
        let out = get("M0ON_REAL_OUT");
        std::fs::create_dir_all(&out).unwrap();
        let items = vec![
            Item::Input(get("M0ON_REAL_HTML")),
            Item::Mods(get("M0ON_REAL_MODS1")),
            Item::Output(out.join("stage1.html")),
            Item::Mods(get("M0ON_REAL_MODS2")),
            Item::Output(out.join("stage2.html")),
        ];
        let (r, shared) = reporter();
        let res = run(&plan(&items).unwrap(), false, &r);
        let log = shared.lock().unwrap().log.clone();
        for l in log.iter().filter(|l| l.contains("===") || l.contains("Stage ") || l.contains("ERROR")).take(40) {
            println!("{l}");
        }
        println!("{res:?}");
        res.unwrap();
    }
}
