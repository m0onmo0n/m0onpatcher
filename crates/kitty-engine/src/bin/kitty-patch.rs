//! `kitty-patch <input.html> <mods_dir> <out.html> [--crlf]`: parse the mods, apply them to the
//! HTML, write the result. `--crlf` writes Windows line endings, like the Python patcher does on
//! Windows (used to compare with its raw output).

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let crlf = args.iter().any(|a| a == "--crlf");
    args.retain(|a| a != "--crlf");
    if args.len() != 3 {
        eprintln!("usage: kitty-patch <input.html> <mods_dir> <out.html> [--crlf]");
        std::process::exit(2);
    }
    let t0 = std::time::Instant::now();
    let res = kitty_engine::load::load_mods(Path::new(&args[1]))?;
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
    println!(
        "replacements made {} | failed {} | parse {:.2}s | total {:.2}s",
        out.replacements_made, out.replacements_failed, t_parse, t_all
    );
    for l in &out.fail_log {
        println!("FAIL: {}", l.chars().take(200).collect::<String>());
    }
    Ok(())
}
