//! The legacy phase (`LoadOptions::legacy_replace_all`): `~` / `~~` blocks run first and replace every match,
//! while the default keeps the KittyPatcher 0.7.6 v2 behaviour (first match only, in the normal sequence).

use kitty_engine::apply::patch;
use kitty_engine::load::{load_mods_with, LoadOptions};
use std::path::PathBuf;

const HTML: &str = concat!(
    "<html><body><tw-storydata name=\"T\" startnode=\"1\" creator=\"Twine\" creator-version=\"2.6.2\" ifid=\"X\" zoom=\"1\" ",
    "format=\"SugarCube\" format-version=\"2.36.1\" options=\"\" hidden>",
    "<style role=\"stylesheet\" id=\"twine-user-stylesheet\" type=\"text/twine-css\"></style>",
    "<script role=\"script\" id=\"twine-user-script\" type=\"text/twine-javascript\">var a = 1; var b = 1;</script>",
    "<tw-passagedata pid=\"1\" name=\"Start\" tags=\"\" position=\"0,0\" size=\"100,100\">cat cat cat</tw-passagedata>",
    "<tw-passagedata pid=\"2\" name=\"Other\" tags=\"\" position=\"0,0\" size=\"100,100\">cat dog cat</tw-passagedata>",
    "</tw-storydata></body></html>"
);

fn mods_dir(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kitty-legacy-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    dir
}

fn run(tag: &str, files: &[(&str, &str)], legacy_replace_all: bool) -> kitty_engine::apply::Outcome {
    let dir = mods_dir(tag, files);
    let loaded = load_mods_with(&dir, &LoadOptions { legacy_replace_all }).unwrap();
    let out = patch(HTML, &loaded.state, &loaded.loaded_mods).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn passage<'a>(html: &'a str, name: &str) -> &'a str {
    let start = html.find(&format!("name=\"{name}\"")).unwrap();
    let body_start = html[start..].find('>').unwrap() + start + 1;
    let end = html[body_start..].find("</tw-passagedata>").unwrap() + body_start;
    &html[body_start..end]
}

#[test]
fn default_replaces_only_the_first_match_in_passage_text() {
    let out = run("default", &[("a.mod", "cat\n~\nkitten\n~~\n")], false);
    assert_eq!(passage(&out.html, "Start"), "kitten cat cat");
    assert_eq!(passage(&out.html, "Other"), "cat dog cat");
    assert_eq!(out.replacements_failed, 0);
}

#[test]
fn legacy_phase_replaces_every_match() {
    let out = run("all", &[("a.mod", "cat\n~\nkitten\n~~\n")], true);
    assert_eq!(passage(&out.html, "Start"), "kitten kitten kitten");
    assert_eq!(passage(&out.html, "Other"), "kitten dog kitten");
    assert_eq!(out.replacements_failed, 0);
    assert!(out.log.iter().any(|l| l.starts_with("Legacy: replaced 5 occurrence(s)")), "{:?}", out.log);
}

#[test]
fn legacy_phase_replaces_every_match_in_the_script_block_too() {
    // the script block is replaced everywhere in both modes
    for legacy in [false, true] {
        let out = run("script", &[("a.mod", "= 1;\n~\n= 2;\n~~\n")], legacy);
        assert!(out.html.contains("var a = 2; var b = 2;"), "legacy={legacy}");
    }
}

#[test]
fn replace_blocks_still_replace_only_the_first_match() {
    let out = run("classic", &[("a.mod", "Replace:\ncat\nWith:\nkitten\n")], true);
    assert_eq!(passage(&out.html, "Start"), "kitten cat cat");
}

#[test]
fn legacy_blocks_run_before_replace_blocks_of_earlier_mods() {
    // a.mod (loaded first) has a classic block on "dog"; b.mod has a legacy block on the whole passage text.
    let files = [("a.mod", "Replace:
dog
With:
wolf
"), ("b.mod", "cat dog cat
~
bird
~~
")];

    // default: load order decides, so the classic block goes first and the legacy block no longer finds its text
    let normal = run("order-off", &files, false);
    assert_eq!(passage(&normal.html, "Other"), "cat wolf cat");
    assert_eq!(normal.replacements_failed, 1);

    // legacy phase: the legacy block goes first, so it is the classic block that finds nothing
    let legacy = run("order-on", &files, true);
    assert_eq!(passage(&legacy.html, "Other"), "bird");
    assert_eq!(legacy.replacements_failed, 1);
}

#[test]
fn a_legacy_block_without_a_match_fails_but_the_others_still_run() {
    let out = run("nomatch", &[("a.mod", "nothing here\n~\nx\n~~\ncat\n~\nkitten\n~~\n")], true);
    assert_eq!(out.replacements_failed, 1);
    assert!(out.fail_log[0].starts_with("No match found for 'nothing here'"), "{:?}", out.fail_log);
    assert_eq!(passage(&out.html, "Start"), "kitten kitten kitten");
}

#[test]
fn a_failure_names_the_earlier_block_that_changed_its_text() {
    // block 1 rewrites "var a = 1;" everywhere, so block 2, which needs the longer script text, no longer matches
    let out = run("culprit", &[("a.mod", "var a = 1;
~
var a = 9;
~~
var a = 1; var b = 1;
~
x
~~
")], true);
    assert_eq!(out.replacements_failed, 1);
    assert!(out.fail_log[0].contains("an earlier legacy block replaced text inside this anchor"), "{:?}", out.fail_log);
}

// ---- per-mod `# Legacy:` header -------------------------------------------------------------------------

#[test]
fn header_all_opts_a_mod_in_even_when_the_default_is_off() {
    let out = run("hdr-all", &[("a.mod", "# Name: a\n# Legacy: all\ncat\n~\nkitten\n~~\n")], false);
    assert_eq!(passage(&out.html, "Start"), "kitten kitten kitten");
    assert_eq!(passage(&out.html, "Other"), "kitten dog kitten");
}

#[test]
fn header_first_keeps_first_match_even_when_the_default_is_on() {
    let out = run("hdr-first", &[("a.mod", "# Legacy: first\ncat\n~\nkitten\n~~\n")], true);
    assert_eq!(passage(&out.html, "Start"), "kitten cat cat");
    assert_eq!(passage(&out.html, "Other"), "cat dog cat");
}

#[test]
fn each_mod_follows_its_own_header_and_the_rest_follow_the_default() {
    // a.mod opts in, b.mod has no header (default: first match only), c.mod opts out
    let files = [
        ("a.mod", "# Legacy: all\ncat\n~\nkitten\n~~\n"),
        ("b.mod", "dog\n~\nwolf\n~~\n"),
        ("c.mod", "# Legacy: first\nkitten kitten kitten\n~\nlitter\n~~\n"),
    ];
    // default off: a replaces every "cat"; c then finds "kitten kitten kitten" and replaces it (first match = only match)
    let off = run("mixed-off", &files, false);
    assert_eq!(passage(&off.html, "Start"), "litter");
    assert_eq!(passage(&off.html, "Other"), "kitten wolf kitten");
    assert_eq!(off.replacements_failed, 0);
    // default on: b.mod joins the legacy phase too
    let on = run("mixed-on", &files, true);
    assert_eq!(passage(&on.html, "Start"), "litter");
    assert_eq!(passage(&on.html, "Other"), "kitten wolf kitten");
    assert_eq!(on.replacements_failed, 0);
}

#[test]
fn an_unknown_header_value_is_ignored_with_a_warning() {
    let dir = mods_dir("hdr-bad", &[("a.mod", "# Legacy: yes\ncat\n~\nkitten\n~~\n")]);
    let loaded = load_mods_with(&dir, &LoadOptions::default()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(loaded.warnings.iter().any(|w| w.contains("unknown value 'yes'")), "{:?}", loaded.warnings);
    assert!(loaded.state.legacy_list.is_empty());
}

// ---- ordered groups (stage builder) ---------------------------------------------------------------------

#[test]
fn groups_load_in_the_given_order_not_alphabetically() {
    use kitty_engine::load::load_mod_groups;
    let dir = mods_dir("groups", &[("a.mod", "cat\n~\nA\n~~\n"), ("b.mod", "dog\n~\nB\n~~\n"), ("c.mod", "# Priority: 1\ncat dog\n~\nC\n~~\n")]);
    let p = |n: &str| dir.join(n);
    // given order: b, a  (alphabetical would be a, b)
    let loaded = load_mod_groups(&[vec![p("b.mod")], vec![p("a.mod")]], &LoadOptions::default()).unwrap();
    assert_eq!(loaded.loaded_mods, vec!["b.mod".to_string(), "a.mod".to_string()]);
    // inside one group the usual order applies: priority 1 (c) before default 1000 (a, b)
    let loaded = load_mod_groups(&[vec![p("b.mod"), p("a.mod"), p("c.mod")]], &LoadOptions::default()).unwrap();
    assert_eq!(loaded.loaded_mods, vec!["c.mod".to_string(), "a.mod".to_string(), "b.mod".to_string()]);
    // the same file in two groups loads once
    let loaded = load_mod_groups(&[vec![p("a.mod")], vec![p("a.mod"), p("b.mod")]], &LoadOptions::default()).unwrap();
    assert_eq!(loaded.loaded_mods, vec!["a.mod".to_string(), "b.mod".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}
