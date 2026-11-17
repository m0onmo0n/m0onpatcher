//! Build-helper binary invoked via `cargo dist`. Detects the host OS,
//! runs the right cargo subcommand (`build --release` on Windows /
//! Linux, `bundle --release` on macOS), and zips the resulting artefact
//! into target/dist/m0onpacker-v<version>-<os>.zip.

use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::CompressionMethod;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args
        .next()
        .ok_or_else(|| anyhow!("usage: cargo dist"))?;
    match cmd.as_str() {
        "dist" => run_dist(),
        other => bail!("unknown xtask command: {other}"),
    }
}

fn run_dist() -> Result<()> {
    let workspace = locate_workspace_root()?;
    std::env::set_current_dir(&workspace)
        .with_context(|| format!("chdir to {}", workspace.display()))?;

    let version = read_package_version(&workspace.join("Cargo.toml"))?;
    let dist_dir = workspace.join("target").join("dist");
    fs::create_dir_all(&dist_dir)
        .with_context(|| format!("create {}", dist_dir.display()))?;

    #[cfg(target_os = "windows")]
    let label = "windows";
    #[cfg(target_os = "linux")]
    let label = "linux";
    #[cfg(target_os = "macos")]
    let label = "macos";
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = version;
        bail!("unsupported host OS");
    }

    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        let zip_path = dist_dir.join(format!("m0onpacker-v{version}-{label}.zip"));

        #[cfg(target_os = "windows")]
        {
            run_cargo(&["build", "--release", "--package", "m0onpacker"])?;
            let bin = workspace
                .join("target")
                .join("release")
                .join("m0onpacker.exe");
            zip_files(&release_files(&workspace, bin, "m0onpacker.exe"), &zip_path)?;
        }

        #[cfg(target_os = "linux")]
        {
            run_cargo(&["build", "--release", "--package", "m0onpacker"])?;
            let bin = workspace.join("target").join("release").join("m0onpacker");
            zip_files(&release_files(&workspace, bin, "m0onpacker"), &zip_path)?;
        }

        #[cfg(target_os = "macos")]
        {
            run_cargo(&["bundle", "--release", "--package", "m0onpacker"])?;
            let bundle_dir = workspace
                .join("target")
                .join("release")
                .join("bundle")
                .join("osx");
            let app = find_first_app_bundle(&bundle_dir)?;
            let app_name = app
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("bundle path has no file name"))?
                .to_string();
            zip_directory(&app, &app_name, &release_files(&workspace, PathBuf::new(), ""), &zip_path)?;
        }

        println!("Wrote {}", zip_path.display());
    }

    Ok(())
}

/// Run `cargo <args>` inheriting stdio so build output stays visible.
fn run_cargo(args: &[&str]) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .status()
        .with_context(|| format!("spawn cargo {}", args.join(" ")))?;
    if !status.success() {
        bail!("cargo {} failed with {}", args.join(" "), status);
    }
    Ok(())
}

fn locate_workspace_root() -> Result<PathBuf> {
    let output = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .output()
        .context("cargo locate-project")?;
    if !output.status.success() {
        bail!("cargo locate-project failed");
    }
    let raw = String::from_utf8(output.stdout).context("locate-project output not UTF-8")?;
    let trimmed = raw.trim();
    let cargo_toml = PathBuf::from(trimmed);
    cargo_toml
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("workspace Cargo.toml has no parent: {trimmed}"))
}

/// Minimal TOML scan — grabs the `version = "…"` line from the
/// `[package]` section. Sufficient for our single-package workspace
/// root; avoids pulling in a full TOML parser for the xtask.
fn read_package_version(cargo_toml: &Path) -> Result<String> {
    let text = fs::read_to_string(cargo_toml)
        .with_context(|| format!("read {}", cargo_toml.display()))?;
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(section) = trimmed
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
        {
            in_package = section == "package";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("version") {
            let rest = rest.trim_start_matches([' ', '=']).trim();
            if let Some(inner) = rest.strip_prefix('"') {
                if let Some(end) = inner.find('"') {
                    return Ok(inner[..end].to_string());
                }
            }
        }
    }
    Err(anyhow!(
        "no version = \"…\" under [package] in {}",
        cargo_toml.display()
    ))
}

/// Files that go into every release zip: the program plus the README and the copyright/credits notice.
/// With an empty `program_name` (macOS: the .app bundle is added separately) only the documents are returned.
fn release_files(workspace: &Path, program: PathBuf, program_name: &str) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = Vec::new();
    if !program_name.is_empty() {
        v.push((program, program_name.to_string()));
    }
    for doc in ["README.md", "NOTICE.txt"] {
        let p = workspace.join(doc);
        if p.is_file() {
            v.push((p, doc.to_string()));
        }
    }
    v
}

fn add_file<W: Write + Seek>(zw: &mut ZipWriter<W>, src: &Path, name_in_zip: &str) -> Result<()> {
    let opts = file_options(src)?;
    zw.start_file(name_in_zip, opts)
        .with_context(|| format!("start zip entry {name_in_zip}"))?;
    let mut input = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut buf = Vec::new();
    input.read_to_end(&mut buf)?;
    zw.write_all(&buf)?;
    Ok(())
}

fn zip_files(files: &[(PathBuf, String)], zip_path: &Path) -> Result<()> {
    let file = File::create(zip_path)
        .with_context(|| format!("create {}", zip_path.display()))?;
    let mut zw = ZipWriter::new(file);
    for (src, name) in files {
        add_file(&mut zw, src, name)?;
    }
    zw.finish().context("finalize zip")?;
    Ok(())
}

fn zip_directory(src_dir: &Path, name_in_zip: &str, extras: &[(PathBuf, String)], zip_path: &Path) -> Result<()> {
    let file = File::create(zip_path)
        .with_context(|| format!("create {}", zip_path.display()))?;
    let mut zw = ZipWriter::new(file);
    let base_opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    add_dir_recursive(src_dir, name_in_zip, &mut zw, base_opts)?;
    for (src, name) in extras {
        add_file(&mut zw, src, name)?;
    }
    zw.finish().context("finalize zip")?;
    Ok(())
}

fn add_dir_recursive<W: Write + Seek>(
    dir: &Path,
    zip_prefix: &str,
    zw: &mut ZipWriter<W>,
    base_opts: SimpleFileOptions,
) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_name = format!("{zip_prefix}/{name}");
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let _ = zw.add_directory(&entry_name, base_opts);
            add_dir_recursive(&path, &entry_name, zw, base_opts)?;
        } else if ft.is_file() {
            let opts = file_options(&path)?;
            zw.start_file(&entry_name, opts)
                .with_context(|| format!("start zip entry {entry_name}"))?;
            let mut f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
            io::copy(&mut f, zw)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&path)?;
            let target_str = target.to_string_lossy().into_owned();
            zw.add_symlink::<_, _, ()>(&entry_name, &target_str, base_opts)
                .with_context(|| format!("add symlink {entry_name}"))?;
        }
    }
    Ok(())
}

fn file_options(src: &Path) -> Result<SimpleFileOptions> {
    let mut opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(src)?;
        opts = opts.unix_permissions(meta.permissions().mode());
    }
    #[cfg(not(unix))]
    {
        let _ = src;
    }
    Ok(opts)
}

fn find_first_app_bundle(dir: &Path) -> Result<PathBuf> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("app")
        {
            return Ok(entry.path());
        }
    }
    bail!(".app bundle not found in {}", dir.display())
}
