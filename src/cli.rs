//! The command line: `m0onpatcher-cli <command> [options]`. Run it with `help` for the commands.
//!
//! Exit codes: 0 = done, 1 = failed (a block did not apply, a file is missing, ...), 2 = wrong usage.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::stages::{self, Item};
use crate::{Echo, PatchJob, Reporter, Shared, SharedRef, Status};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
enum CliError {
    /// wrong arguments: exit code 2
    Usage(String),
    /// the work failed: exit code 1
    Failed(String),
}

fn usage<T>(msg: impl Into<String>) -> Result<T, CliError> {
    Err(CliError::Usage(msg.into()))
}

fn failed<T>(msg: impl Into<String>) -> Result<T, CliError> {
    Err(CliError::Failed(msg.into()))
}

/// Ok carries the text printed at the end (the result line or a help text).
type CliResult = Result<String, CliError>;

// ---------------------------------------------------------------- help

fn general_help() -> String {
    format!(
        "m0onpatcher-cli {VERSION}: the m0onpatcher engine from the terminal

USAGE
    m0onpatcher-cli <command> [options]

COMMANDS
    patch      Apply a folder of mods to a game HTML (like the Patcher tab)
    stages     Run a chain of patch stages in one go (like the Stage builder)
    extract    Extract a game HTML into editable sources
    compile    Compile the sources back into an HTML
    help       Show the help of a command, for example: m0onpatcher-cli help patch

OPTIONS (for every command)
    -h, --help       Show help
    -q, --quiet      Print only warnings, errors and the final result
    -v, --verbose    Also print the per-block patch log
    -V, --version    Show the version (instead of a command)

EXIT CODE
    0 done, 1 failed (a block did not apply, a file is missing, ...), 2 wrong usage"
    )
}

fn command_help(cmd: &str) -> Option<String> {
    let text = match cmd {
        "patch" => {
            "m0onpatcher-cli patch: apply a folder of mods to a game HTML

USAGE
    m0onpatcher-cli patch <game.html> <mods-folder> [options]
    m0onpatcher-cli patch --html <game.html> --mods <mods-folder> [options]

OPTIONS
    --html <file>    The game HTML (only read)
    --mods <folder>  The folder with the mod files (sub-folders are searched)
    -o, --out <file> Where to write the result (default: <game name>.modded.html next to the game)
    --legacy-all     Legacy option: the old ~ / ~~ blocks of mods without a '# Legacy:' header
                     are applied first and replace every match
    --raw            Write the raw patched HTML, as the KittyPatcher script alone writes it, instead
                     of the cleaned form
    -q, --quiet / -v, --verbose

Writes the patched HTML in the cleaned form ScRepacker writes (like the Patcher tab; --raw switches
that off) and the three logs (FailsPatchLog.txt,
MainPatchLog.txt, ModPatchLog.txt) to <game folder>/logs. If a block fails, no output is written."
        }
        "stages" => {
            "m0onpatcher-cli stages: run a chain of patch stages (the Stage builder, from the terminal)

USAGE
    m0onpatcher-cli stages --input <game.html> --mods <path> [--mods <path>...] --output <file> [...]
    m0onpatcher-cli stages --file <list.json>

The --input, --mods and --output options are the items of the Stage builder list and run in the order you
write them. Mods between two outputs are one patch run, in that order (a folder inside itself by
'# Priority:' and file name). An output is written to disk and is the input of what follows.

OPTIONS
    --input <file>    The game HTML (must come first)
    --mods <path>     A mod file or a folder of mod files (repeatable)
    --output <file>   Ends a stage: where to write its result (repeatable)
    --file <list.json>  Read the list from a file instead (relative paths start in that file's folder): [{\"Input\":\"game.html\"},{\"Mods\":\"a.mod\"},
                        {\"Output\":\"stage1.html\"},{\"Mods\":\"stage2/mods\"},{\"Output\":\"final.html\"}]
    --legacy-all      Legacy option for every stage (see 'help patch')
    -q, --quiet / -v, --verbose

Each output is written the way ScRepacker writes it. The first stage that fails stops the chain; the outputs
written before it stay. Every stage writes its three logs to <output folder>/logs/<output name>/.

EXAMPLE
    m0onpatcher-cli stages --input game.html --mods \"Bits&Fun.mod\" --output stage1.html --mods stage2/mods --output final.html"
        }
        "extract" => {
            "m0onpatcher-cli extract: extract a game HTML into editable sources

USAGE
    m0onpatcher-cli extract <game.html> [options]

OPTIONS
    --html <file>     The game HTML (or just give it as the first argument)
    --src <folder>    Where to write the sources (default: the 'src' folder next to the game HTML;
                      a folder given as an argument is taken as this)
    -f, --force       Replace an existing source folder (without it the command refuses, because
                      extracting again replaces the files and changes made there are lost)
    -q, --quiet / -v, --verbose"
        }
        "compile" => {
            "m0onpatcher-cli compile: compile the sources back into an HTML

USAGE
    m0onpatcher-cli compile <game.html> [options]
    m0onpatcher-cli compile <sources-folder> --html <game.html> [options]

OPTIONS
    --html <file>     The original game HTML: the sources are merged into it as the reference
    --src <folder>    The sources (default: the 'src' folder next to the game HTML; a folder given as an
                      argument is taken as this)
    -o, --out <file>  Where to write the result (default: <game name>.merged.html next to the game)
    -q, --quiet / -v, --verbose"
        }
        _ => return None,
    };
    Some(text.to_string())
}

// ---------------------------------------------------------------- argument parsing

struct Spec {
    /// options that take a value (`--out file` or `--out=file`)
    values: &'static [&'static str],
    /// options without a value
    switches: &'static [&'static str],
}

#[derive(Debug, Default)]
struct Parsed {
    pos: Vec<String>,
    /// in the order they were written
    opts: Vec<(String, Option<String>)>,
}

impl Parsed {
    fn has(&self, name: &str) -> bool {
        self.opts.iter().any(|(k, _)| k == name)
    }

    /// the last value given for `name`
    fn value(&self, name: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }

    fn echo(&self) -> Result<Echo, CliError> {
        match (self.has("--quiet"), self.has("--verbose")) {
            (true, true) => usage("--quiet and --verbose cannot be used together"),
            (true, false) => Ok(Echo::Quiet),
            (false, true) => Ok(Echo::Verbose),
            (false, false) => Ok(Echo::Normal),
        }
    }
}

fn canonical(arg: &str) -> &str {
    match arg {
        "-h" => "--help",
        "-V" => "--version",
        "-q" => "--quiet",
        "-v" => "--verbose",
        "-o" => "--out",
        "-f" => "--force",
        other => other,
    }
}

fn parse(args: &[String], spec: &Spec) -> Result<Parsed, CliError> {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];
        i += 1;
        if raw == "--" {
            p.pos.extend(args[i..].iter().cloned());
            break;
        }
        if raw.starts_with('-') && raw.len() > 1 {
            let (name_raw, inline) = match raw.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
                _ => (raw.as_str(), None),
            };
            let name = canonical(name_raw);
            if spec.values.contains(&name) {
                let value = match inline {
                    Some(v) => v,
                    None if i < args.len() => {
                        i += 1;
                        args[i - 1].clone()
                    }
                    None => return usage(format!("{name} needs a value")),
                };
                p.opts.push((name.to_string(), Some(value)));
            } else if spec.switches.contains(&name) {
                if inline.is_some() {
                    return usage(format!("{name} does not take a value"));
                }
                p.opts.push((name.to_string(), None));
            } else {
                return usage(format!("unknown option '{raw}'"));
            }
        } else {
            p.pos.push(raw.clone());
        }
    }
    Ok(p)
}

/// The positional arguments split into things that are existing folders and everything else.
fn split_positionals(p: &Parsed) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for a in &p.pos {
        let path = PathBuf::from(a);
        if path.is_dir() {
            dirs.push(path);
        } else {
            files.push(path);
        }
    }
    (files, dirs)
}

fn html_arg(p: &Parsed) -> Result<PathBuf, CliError> {
    if let Some(v) = p.value("--html") {
        return Ok(PathBuf::from(v));
    }
    match split_positionals(p).0.into_iter().next() {
        Some(f) => Ok(f),
        None => usage("give the game HTML (a file), for example: m0onpatcher-cli <command> game.html"),
    }
}

fn require_file(path: &std::path::Path, what: &str) -> Result<(), CliError> {
    if path.is_file() {
        Ok(())
    } else {
        failed(format!("{what} {} does not exist", path.display()))
    }
}

// ---------------------------------------------------------------- running a job with terminal output

struct StdLogger {
    echo: Echo,
}

impl Log for StdLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info && metadata.target().starts_with("m0onpatcher")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if record.level() <= Level::Warn {
            eprintln!("[{}] {}", record.level(), record.args());
        } else if matches!(self.echo, Echo::Normal | Echo::Verbose) {
            println!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Run `work` on a thread with a big stack, printing its log lines as they happen. The `log` records of the
/// extractor and merger are printed too.
fn run_reporting<F>(echo: Echo, work: F) -> CliResult
where
    F: FnOnce(&Reporter) -> anyhow::Result<String> + Send + 'static,
{
    // the first call installs the logger; later calls in the same process (tests) keep it
    if log::set_boxed_logger(Box::new(StdLogger { echo })).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    let shared: SharedRef = Arc::new(Mutex::new(Shared {
        log: Vec::new(),
        progress: 0.0,
        indeterminate: false,
        task: String::new(),
        status: Status::Idle,
    }));
    let handle = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let reporter = Reporter { shared, ctx: eframe::egui::Context::default(), echo };
            work(&reporter)
        })
        .map_err(|e| CliError::Failed(format!("cannot start the worker thread: {e}")))?;
    match handle.join() {
        Ok(Ok(msg)) => Ok(msg),
        Ok(Err(err)) => failed(format!("{err:#}")),
        Err(_) => failed("internal error (the worker thread panicked)"),
    }
}

// ---------------------------------------------------------------- commands

fn cmd_patch(args: &[String]) -> CliResult {
    let spec = Spec {
        values: &["--html", "--mods", "--out"],
        switches: &["--legacy-all", "--raw", "--help", "--quiet", "--verbose"],
    };
    let p = parse(args, &spec)?;
    if p.has("--help") {
        return Ok(command_help("patch").unwrap_or_default());
    }
    let (files, dirs) = split_positionals(&p);
    let html = match p.value("--html") {
        Some(v) => PathBuf::from(v),
        None => match files.first() {
            Some(f) => f.clone(),
            None => return usage("give the game HTML and the mods folder: m0onpatcher-cli patch <game.html> <mods-folder>"),
        },
    };
    let mods = match p.value("--mods") {
        Some(v) => PathBuf::from(v),
        None => match (dirs.first(), files.get(1)) {
            (Some(d), _) => d.clone(),
            (None, Some(missing)) => return failed(format!("the mods folder {} does not exist", missing.display())),
            (None, None) => return usage("give the mods folder: m0onpatcher-cli patch <game.html> <mods-folder>"),
        },
    };
    require_file(&html, "the game HTML")?;
    if !mods.is_dir() {
        return failed(format!("the mods folder {} does not exist", mods.display()));
    }
    let output = match p.value("--out") {
        Some(o) => PathBuf::from(o),
        None => {
            let stem = html.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "game".to_string());
            html.with_file_name(format!("{stem}.modded.html"))
        }
    };
    let job = PatchJob { html, mods, output, legacy_replace_all: p.has("--legacy-all"), clean_output: !p.has("--raw") };
    run_reporting(p.echo()?, move |r| crate::run_patch(&job, r))
}

fn cmd_stages(args: &[String]) -> CliResult {
    let spec = Spec {
        values: &["--input", "--mods", "--output", "--file"],
        switches: &["--legacy-all", "--help", "--quiet", "--verbose"],
    };
    let p = parse(args, &spec)?;
    if p.has("--help") {
        return Ok(command_help("stages").unwrap_or_default());
    }
    if !p.pos.is_empty() {
        return usage(format!("unexpected argument '{}': stages takes --input, --mods and --output options", p.pos[0]));
    }
    let items: Vec<Item> = if let Some(file) = p.value("--file") {
        if p.has("--input") || p.has("--mods") || p.has("--output") {
            return usage("use either --file or the --input / --mods / --output options, not both");
        }
        let text = std::fs::read_to_string(file).map_err(|e| CliError::Failed(format!("cannot read {file}: {e}")))?;
        crate::stages::from_json(&text, std::path::Path::new(file).parent().unwrap_or(std::path::Path::new("."))).map_err(|e| CliError::Failed(format!("{file}: {e}")))?
    } else {
        p.opts
            .iter()
            .filter_map(|(k, v)| {
                let path = PathBuf::from(v.as_deref()?);
                match k.as_str() {
                    "--input" => Some(Item::Input(path)),
                    "--mods" => Some(Item::Mods(path)),
                    "--output" => Some(Item::Output(path)),
                    _ => None,
                }
            })
            .collect()
    };
    let plan = match stages::plan(&items) {
        Ok(plan) => plan,
        Err(msg) => return usage(msg),
    };
    require_file(&plan.input, "the input HTML")?;
    let legacy = p.has("--legacy-all");
    run_reporting(p.echo()?, move |r| stages::run(&plan, legacy, r))
}

fn cmd_extract(args: &[String]) -> CliResult {
    let spec = Spec { values: &["--html", "--src"], switches: &["--force", "--help", "--quiet", "--verbose"] };
    let p = parse(args, &spec)?;
    if p.has("--help") {
        return Ok(command_help("extract").unwrap_or_default());
    }
    let html = html_arg(&p)?;
    require_file(&html, "the game HTML")?;
    let src = match p.value("--src") {
        Some(s) => PathBuf::from(s),
        None => match split_positionals(&p).1.into_iter().next() {
            Some(d) => d,
            None => crate::source_dir(&html).unwrap_or_else(|| PathBuf::from("src")),
        },
    };
    if src.exists() && !p.has("--force") {
        return failed(format!(
            "the source folder {} already exists; extracting again replaces its files, so changes made there are lost. Use --force to do it anyway.",
            src.display()
        ));
    }
    run_reporting(p.echo()?, move |_| crate::run_extract(&html, &src))
}

fn cmd_compile(args: &[String]) -> CliResult {
    let spec = Spec { values: &["--html", "--src", "--out"], switches: &["--help", "--quiet", "--verbose"] };
    let p = parse(args, &spec)?;
    if p.has("--help") {
        return Ok(command_help("compile").unwrap_or_default());
    }
    let html = html_arg(&p)?;
    require_file(&html, "the game HTML")?;
    let src = match p.value("--src") {
        Some(s) => PathBuf::from(s),
        None => match split_positionals(&p).1.into_iter().next() {
            Some(d) => d,
            None => crate::source_dir(&html).unwrap_or_else(|| PathBuf::from("src")),
        },
    };
    let out = match p.value("--out") {
        Some(o) => PathBuf::from(o),
        None => crate::compiled_path(&html).unwrap_or_else(|| PathBuf::from("compiled.html")),
    };
    run_reporting(p.echo()?, move |_| crate::run_compile(&html, &src, &out))
}

fn dispatch(args: &[String]) -> CliResult {
    let Some(cmd) = args.first() else {
        return usage("no command given");
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "-h" | "--help" => Ok(general_help()),
        "help" => match rest.first() {
            None => Ok(general_help()),
            Some(c) => match command_help(c) {
                Some(text) => Ok(text),
                None => usage(format!("no help for '{c}': the commands are patch, stages, extract and compile")),
            },
        },
        "-V" | "--version" | "version" => Ok(format!("m0onpatcher-cli {VERSION}")),
        "patch" => cmd_patch(rest),
        "stages" => cmd_stages(rest),
        "extract" => cmd_extract(rest),
        "compile" => cmd_compile(rest),
        other => usage(format!("unknown command '{other}'")),
    }
}

/// Run the command line with the arguments after the program name. Returns the exit code.
pub fn run(args: Vec<String>) -> ExitCode {
    match dispatch(&args) {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            ExitCode::SUCCESS
        }
        Err(CliError::Usage(msg)) => {
            eprintln!("error: {msg}");
            eprintln!("Run 'm0onpatcher-cli help' for the commands and options.");
            ExitCode::from(2)
        }
        Err(CliError::Failed(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    const HTML: &str = concat!(
        "<html><body><tw-storydata name=\"T\" startnode=\"1\" creator=\"Twine\" creator-version=\"2.6.2\" ifid=\"X\" zoom=\"1\" ",
        "format=\"SugarCube\" format-version=\"2.36.1\" options=\"\" hidden>",
        "<style role=\"stylesheet\" id=\"twine-user-stylesheet\" type=\"text/twine-css\"></style>",
        "<script role=\"script\" id=\"twine-user-script\" type=\"text/twine-javascript\">var a = 1;</script>",
        "<tw-passagedata pid=\"1\" name=\"Start\" tags=\"\" position=\"0,0\" size=\"100,100\">cat sat</tw-passagedata>",
        "</tw-storydata></body></html>"
    );

    fn workdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("m0on-cli-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("g.html"), HTML).unwrap();
        dir
    }

    fn s(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn options_are_parsed_in_order_with_both_value_forms() {
        let spec = Spec { values: &["--out", "--mods"], switches: &["--force", "--quiet"] };
        let p = parse(&a(&["game.html", "-o", "x.html", "--mods=m1", "--mods", "m2", "-f", "-q"]), &spec).unwrap();
        assert_eq!(p.pos, vec!["game.html"]);
        assert_eq!(p.value("--out"), Some("x.html"));
        assert_eq!(p.value("--mods"), Some("m2"), "the last value wins");
        assert!(p.has("--force") && p.has("--quiet"));
        assert_eq!(p.opts.iter().filter(|(k, _)| k == "--mods").count(), 2);
    }

    #[test]
    fn bad_options_are_usage_errors() {
        let spec = Spec { values: &["--out"], switches: &["--force"] };
        assert!(matches!(parse(&a(&["--nope"]), &spec), Err(CliError::Usage(m)) if m.contains("unknown option '--nope'")));
        assert!(matches!(parse(&a(&["--out"]), &spec), Err(CliError::Usage(m)) if m.contains("needs a value")));
        assert!(matches!(parse(&a(&["--force=1"]), &spec), Err(CliError::Usage(m)) if m.contains("does not take a value")));
    }

    #[test]
    fn help_version_and_unknown_commands() {
        assert!(dispatch(&a(&["--help"])).unwrap().contains("COMMANDS"));
        assert!(dispatch(&a(&["help", "stages"])).unwrap().contains("--output"));
        assert!(dispatch(&a(&["patch", "--help"])).unwrap().contains("--legacy-all"));
        assert!(dispatch(&a(&["--version"])).unwrap().starts_with("m0onpatcher-cli "));
        assert!(matches!(dispatch(&a(&[])), Err(CliError::Usage(_))));
        assert!(matches!(dispatch(&a(&["frobnicate"])), Err(CliError::Usage(m)) if m.contains("unknown command")));
        assert!(matches!(dispatch(&a(&["help", "frobnicate"])), Err(CliError::Usage(_))));
        assert!(matches!(dispatch(&a(&["patch", "-q", "-v", "x", "y"])), Err(CliError::Usage(_)) | Err(CliError::Failed(_))));
    }

    #[test]
    fn patch_writes_the_output_and_fails_without_one_when_a_block_fails() {
        let dir = workdir("patch");
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        std::fs::write(dir.join("mods").join("a.mod"), "Replace:\ncat sat\nWith:\ndog sat\n").unwrap();
        let out = dir.join("o.html");
        let msg = dispatch(&a(&["patch", &s(&dir.join("g.html")), &s(&dir.join("mods")), "--out", &s(&out), "-q"])).unwrap();
        assert!(msg.contains("1 mod file(s)"), "{msg}");
        assert!(std::fs::read_to_string(&out).unwrap().contains(">dog sat</tw-passagedata>"));
        assert!(dir.join("logs").join("FailsPatchLog.txt").is_file());

        // the folder may also come first-or-second, and --html / --mods work too
        let out2 = dir.join("o2.html");
        dispatch(&a(&["patch", &s(&dir.join("mods")), "--html", &s(&dir.join("g.html")), "-o", &s(&out2), "-q"])).unwrap();
        assert!(out2.is_file());

        // a block that does not apply: exit code 1 (Failed) and no output file
        std::fs::write(dir.join("mods").join("b.mod"), "Replace:\nnot there\nWith:\nx\n").unwrap();
        let out3 = dir.join("o3.html");
        let err = dispatch(&a(&["patch", &s(&dir.join("g.html")), &s(&dir.join("mods")), "-o", &s(&out3), "-q"])).unwrap_err();
        assert!(matches!(err, CliError::Failed(_)), "{err:?}");
        assert!(!out3.exists());

        // missing things
        assert!(matches!(dispatch(&a(&["patch", &s(&dir.join("g.html"))])), Err(CliError::Usage(_))));
        assert!(matches!(dispatch(&a(&["patch", &s(&dir.join("g.html")), &s(&dir.join("nope"))])), Err(CliError::Failed(m)) if m.contains("does not exist")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stages_from_options_and_from_a_file() {
        let dir = workdir("stages");
        std::fs::create_dir_all(dir.join("mods2")).unwrap();
        std::fs::write(dir.join("a.mod"), "cat sat\n~\nkitten purred\n~~\n").unwrap();
        std::fs::write(dir.join("mods2").join("b.mod"), "Replace:\nkitten purred\nWith:\nkitten purred loudly\n").unwrap();
        let (g, a_mod, m2) = (s(&dir.join("g.html")), s(&dir.join("a.mod")), s(&dir.join("mods2")));
        let (o1, o2) = (s(&dir.join("s1.html")), s(&dir.join("s2.html")));
        let msg = dispatch(&a(&["stages", "--input", &g, "--mods", &a_mod, "--output", &o1, "--mods", &m2, "--output", &o2, "-q"])).unwrap();
        assert!(msg.contains("2 stage(s)"), "{msg}");
        assert!(std::fs::read_to_string(&o2).unwrap().contains(">kitten purred loudly</tw-passagedata>"));

        // the same chain from a list file
        let list = serde_json::to_string(&vec![Item::Input(g.clone().into()), Item::Mods(a_mod.clone().into()), Item::Output(dir.join("f1.html"))]).unwrap();
        std::fs::write(dir.join("list.json"), list).unwrap();
        dispatch(&a(&["stages", "--file", &s(&dir.join("list.json")), "-q"])).unwrap();
        assert!(dir.join("f1.html").is_file());

        // an invalid list is a usage error; mixing --file with items too
        assert!(matches!(dispatch(&a(&["stages", "--input", &g])), Err(CliError::Usage(_))));
        assert!(matches!(dispatch(&a(&["stages", "--file", "x.json", "--input", &g])), Err(CliError::Usage(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_refuses_to_overwrite_without_force_and_compile_rebuilds_the_html() {
        let dir = workdir("extract");
        let g = s(&dir.join("g.html"));
        dispatch(&a(&["extract", &g, "-q"])).unwrap();
        assert!(dir.join("src").is_dir());
        // a second extract needs --force
        assert!(matches!(dispatch(&a(&["extract", &g, "-q"])), Err(CliError::Failed(m)) if m.contains("--force")));
        dispatch(&a(&["extract", &g, "--force", "-q"])).unwrap();

        // edit a passage in the sources and compile: the change is in the new HTML
        let passage = std::fs::read_dir(dir.join("src").join("passage")).unwrap().next().unwrap().unwrap().path();
        let text = std::fs::read_to_string(&passage).unwrap();
        std::fs::write(&passage, text.replace("cat sat", "cat sat down")).unwrap();
        let out = dir.join("c.html");
        dispatch(&a(&["compile", &g, "-o", &s(&out), "-q"])).unwrap();
        assert!(std::fs::read_to_string(&out).unwrap().contains("cat sat down"));

        // the sources folder may be the argument, with --html for the reference
        let out2 = dir.join("c2.html");
        dispatch(&a(&["compile", &s(&dir.join("src")), "--html", &g, "-o", &s(&out2), "-q"])).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&out2).unwrap());

        // no sources: a failure that says what to do
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        assert!(matches!(dispatch(&a(&["compile", &g, "-q"])), Err(CliError::Failed(m)) if m.contains("extract")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
