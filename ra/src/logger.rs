use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::rc::Rc;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Clone)]
pub(crate) struct Logger {
    inner: Rc<RefCell<LoggerInner>>,
}

struct LoggerInner {
    file_writer: Option<BufWriter<File>>,
    stdout_writer: Option<BufWriter<std::io::Stdout>>,
    buffer: Option<Vec<Value>>,
}

impl Logger {
    pub(crate) fn new(
        log_path: Option<PathBuf>,
        stream_to_stdout: bool,
        buffer_for_stdout: bool,
    ) -> Result<Self> {
        let file_writer = if let Some(log_path) = log_path {
            if let Some(parent) = log_path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create log directory {}", parent.display())
                })?;
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&log_path)
                .with_context(|| format!("failed to create log file {}", log_path.display()))?;
            Some(BufWriter::new(file))
        } else {
            None
        };

        let stdout_writer = if stream_to_stdout {
            Some(BufWriter::new(std::io::stdout()))
        } else {
            None
        };

        let buffer = if buffer_for_stdout {
            Some(Vec::new())
        } else {
            None
        };

        Ok(Self {
            inner: Rc::new(RefCell::new(LoggerInner {
                file_writer,
                stdout_writer,
                buffer,
            })),
        })
    }

    pub(crate) fn log_event(&mut self, event: &Value) -> Result<()> {
        let mut enriched = event.clone();
        if let Value::Object(obj) = &mut enriched {
            // Codex-style logs include timestamps; we emit both a human-readable UTC timestamp
            // and a stable numeric timestamp for sorting.
            let now = OffsetDateTime::now_utc();
            let ts_ms = now.unix_timestamp_nanos() / 1_000_000;
            obj.entry("timestamp_ms").or_insert_with(|| json!(ts_ms));
            obj.entry("timestamp").or_insert_with(|| {
                json!(now.format(&Rfc3339).unwrap_or_else(|_| ts_ms.to_string()))
            });
        }
        let mut inner = self.inner.borrow_mut();

        if let Some(buf) = inner.buffer.as_mut() {
            buf.push(enriched.clone());
        }

        if let Some(w) = inner.file_writer.as_mut() {
            serde_json::to_writer(&mut *w, &enriched)?;
            w.write_all(b"\n")?;
            w.flush()?;
        }

        if let Some(w) = inner.stdout_writer.as_mut() {
            serde_json::to_writer(&mut *w, &enriched)?;
            w.write_all(b"\n")?;
            w.flush()?;
        }
        Ok(())
    }

    pub(crate) fn emit_buffer_to_stdout(&self) -> Result<()> {
        let inner = self.inner.borrow();
        let Some(buf) = inner.buffer.as_ref() else {
            return Ok(());
        };
        let mut out = BufWriter::new(std::io::stdout());
        for event in buf {
            serde_json::to_writer(&mut out, event)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        Ok(())
    }
}
