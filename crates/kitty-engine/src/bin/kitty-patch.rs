//! `kitty-patch <input.html> <mods_dir> <out.html> [--crlf] [--legacy-all]`: parse the mods, apply them
//! to the HTML, write the result. `--crlf` writes Windows line endings, like the Python patcher does on
//! Windows (used to compare with its raw output). `--legacy-all` turns on the legacy phase: the old
//! `~` / `~~` blocks run first and replace every match.
//!
//! Environment: `KITTY_PROGRESS=1` prints the engine's stage messages with timestamps to stderr;
//! `KITTY_LOG=<file>` writes the full patch log and the failure list to that file.
//! Exit code: 0 = all blocks applied, 1 = a block failed or an error (the message is printed), 2 = wrong usage.

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let crlf = args.iter().any(|a| a == "--crlf");
    args.retain(|a| a != "--crlf");
    let legacy_all = args.iter().any(|a| a == "--legacy-all");
    args.retain(|a| a != "--legacy-all");
    if args.len() != 3 {
        eprintln!("usage: kitty-patch <input.html> <mods_dir> <out.html> [--crlf] [--legacy-all]");
        std::process::exit(2);
    }
    let t0 = std::time::Instant::now();
    let opts = kitty_engine::load::LoadOptions { legacy_replace_all: legacy_all };
    let res = kitty_engine::load::load_mods_with(Path::new(&args[1]), &opts)?;
    for w in &res.warnings {
        eprintln!("warning: {w}");
    }
    let input = std::fs::read_to_string(&args[0])?;
    let t_parse = t0.elapsed().as_secs_f64();
    let verbose = std::env::var("KITTY_PROGRESS").is_ok();
    let out = kitty_engine::apply::patch_with_progress(&input, &res.state, &res.loaded_mods, &mut |m| {
        if verbose {
            eprintln!("[{:6.2}s] {m}", t0.elapsed().as_secs_f64());
        }
    })?;
    let t_all = t0.elapsed().as_secs_f64();
    let text = if crlf { out.html.replace('\n', "\r\n") } else { out.html.clone() };
    std::fs::write(&args[2], text)?;
    if let Ok(path) = std::env::var("KITTY_LOG") {
        // full patch log and failure log, for debugging
        let mut s = out.log.join("\n");
        s.push_str("\n=== FAILURES ===\n");
        s.push_str(&out.fail_log.join("\n=====\n"));
        std::fs::write(path, s)?;
    }
    println!(
        "replacements made {} | failed {} | parse {:.2}s | total {:.2}s",
        out.replacements_made, out.replacements_failed, t_parse, t_all
    );
    for l in &out.fail_log {
        println!("FAIL: {}", l.chars().take(200).collect::<String>());
    }
    if out.replacements_failed > 0 || !out.fail_log.is_empty() {
        // the output file is still written (handy for comparing runs), but scripts can tell something failed
        std::process::exit(1);
    }
    Ok(())
}
