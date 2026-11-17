//! `kitty-dump <mods_dir> <out.json>`: parse a mods folder and write the registries as JSON
//! (the Rust side of the M1 comparison with the Python reference dump).

use std::path::Path;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: kitty-dump <mods_dir> <out.json>");
        std::process::exit(2);
    }
    let t0 = std::time::Instant::now();
    let res = kitty_engine::load::load_mods(Path::new(&args[0]))?;
    let secs = t0.elapsed().as_secs_f64();
    for w in &res.warnings {
        eprintln!("warning: {w}");
    }
    for (m, tok) in &res.state.unsupported {
        eprintln!("unsupported directive in {m}: {tok}");
    }
    let json = kitty_engine::dump::to_json(&res.loaded_mods, &res.state);
    std::fs::write(&args[1], serde_json::to_string_pretty(&json)?)?;
    let st = &res.state;
    println!(
        "mods {} | mod_dict {} | mod_list {} | reg {} | func {} | passages {} | js {} | css {} | events {} | {:.2}s",
        res.loaded_mods.len(),
        st.mod_dict.len(),
        st.mod_list.len(),
        st.mod_reg_list.len(),
        st.mod_func_list.len(),
        st.passage_registry.len(),
        st.js_registry.len(),
        st.css_registry.len(),
        st.events_registry.len(),
        secs
    );
    Ok(())
}
