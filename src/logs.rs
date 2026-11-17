use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use log::{LevelFilter, Log, Metadata, Record};
use simplelog::{Config, SharedLogger};
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

use crate::config;

const LOGS_DIR: &str = "screpacker-logs";

const TIMESTAMP_FMT: &[FormatItem<'static>] = format_description!(
    "[year]-[month]-[day]T[hour]-[minute]-[second].[subsecond digits:6]"
);

/// Per-line timestamp used inside a log file, e.g. `20:03:06.376`.
/// Exposed so the CLI (via simplelog) and the GUI (via its own Log impl)
/// emit the same prefix.
pub const LOG_LINE_TIME_FMT: &[FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:3]");

pub fn format_log_line_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(LOG_LINE_TIME_FMT)
        .unwrap_or_else(|_| String::from("??:??:??.???"))
}

/// How long the writer thread waits between message bursts before
/// flushing. Keeps flush frequency bounded while still letting a burst
/// of log lines share a single flush.
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Producer handle for the background file-writer thread. Cheap to
/// clone; all clones feed the same channel. Dropping every clone
/// signals the worker to flush and exit.
#[derive(Clone)]
pub struct FileSink {
    tx: Sender<String>,
}

impl FileSink {
    /// Spawn a worker thread that owns `file` and drains this sink. The
    /// worker writes each line as it arrives into a BufWriter (so
    /// unrelated writes share a kernel syscall) and flushes only
    /// on idle (no new messages for FLUSH_INTERVAL) or when the channel
    /// closes. Callers never block on IO.
    pub fn start(file: File) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || worker_loop(file, rx));
        Self { tx }
    }

    pub fn write_line(&self, line: String) {
        let _ = self.tx.send(line);
    }
}

fn worker_loop(file: File, rx: mpsc::Receiver<String>) {
    let mut writer = BufWriter::new(file);
    let mut dirty = false;
    loop {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(first) => {
                if writeln!(writer, "{first}").is_err() {
                    return;
                }
                dirty = true;
                // Drain any lines already queued so one flush covers the
                // entire burst. BufWriter still auto-flushes when its
                // 8KB buffer fills, bounding memory use.
                while let Ok(more) = rx.try_recv() {
                    if writeln!(writer, "{more}").is_err() {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if dirty {
                    let _ = writer.flush();
                    dirty = false;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = writer.flush();
                return;
            }
        }
    }
}

/// Log-crate adapter around `FileSink`. Used by the CLI path via
/// simplelog's `CombinedLogger`; the GUI path uses the raw `FileSink`
/// directly from inside its own `GuiLogger`.
pub struct FileLogger {
    level: LevelFilter,
    sink: FileSink,
    config: Config,
}

impl FileLogger {
    pub fn new(level: LevelFilter, sink: FileSink) -> Box<Self> {
        Box::new(Self {
            level,
            sink,
            config: Config::default(),
        })
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} [{}] {}",
            format_log_line_timestamp(),
            record.level(),
            record.args()
        );
        self.sink.write_line(line);
    }

    fn flush(&self) {}
}

impl SharedLogger for FileLogger {
    fn level(&self) -> LevelFilter {
        self.level
    }

    fn config(&self) -> Option<&Config> {
        Some(&self.config)
    }

    fn as_log(self: Box<Self>) -> Box<dyn Log> {
        self
    }
}

/// Create `<executable_dir>/screpacker-logs` if missing and open a new
/// `screpacker-<timestamp>.log` inside it. Returns both the path (for
/// reporting) and an open file ready to be wrapped in a logger.
pub fn open_log_file() -> Result<(PathBuf, File)> {
    let exe_dir = config::executable_dir()
        .ok_or_else(|| anyhow!("cannot determine executable directory"))?;
    let logs_dir = exe_dir.join(LOGS_DIR);
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("create {}", logs_dir.display()))?;
    let path = logs_dir.join(format!("screpacker-{}.log", format_filename_timestamp()));
    let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    Ok((path, file))
}

/// UTC timestamp suitable for use inside a filename — `:` separators in
/// ISO 8601 times would be illegal on Windows, so we swap them for `-`.
/// Example output: `2025-04-30T23-16-55.139216`.
pub fn format_filename_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(TIMESTAMP_FMT)
        .unwrap_or_else(|_| String::from("unknown-time"))
}
