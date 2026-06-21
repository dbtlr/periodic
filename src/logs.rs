//! Per-day JSONL run output: one file per calendar day under the logs dir, with
//! job/run identity carried in line fields. Retention is file deletion.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One captured output line. `ts` is RFC3339; `stream` is `"stdout"`/`"stderr"`.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct LogRecord {
    pub(crate) ts: String,
    pub(crate) job_id: String,
    pub(crate) run_id: String,
    pub(crate) attempt: u32,
    pub(crate) stream: String,
    pub(crate) text: String,
}

/// Appends `LogRecord`s as JSONL to a per-day file (`YYYY-MM-DD.jsonl`), keyed by
/// the record's own timestamp date. Caches the open file and reopens on date change.
pub(crate) struct DailyLogWriter {
    dir: PathBuf,
    current: Option<(String, File)>, // (date, open file)
}

impl DailyLogWriter {
    pub(crate) fn new(dir: PathBuf) -> Self {
        DailyLogWriter { dir, current: None }
    }

    /// The `YYYY-MM-DD` date prefix of an RFC3339 timestamp (first 10 chars).
    fn date_of(ts: &str) -> &str {
        ts.get(0..10).unwrap_or(ts)
    }

    pub(crate) fn append(&mut self, rec: &LogRecord) -> std::io::Result<()> {
        let date = Self::date_of(&rec.ts).to_owned();
        let reopen = !matches!(&self.current, Some((d, _)) if *d == date);
        if reopen {
            fs::create_dir_all(&self.dir)?;
            let file = OpenOptions::new()
                .create(true).append(true)
                .open(self.dir.join(format!("{date}.jsonl")))?;
            self.current = Some((date, file));
        }
        let (_, file) = self.current.as_mut().expect("file set above");
        let line = serde_json::to_string(rec).expect("LogRecord serializes");
        writeln!(file, "{line}")
    }
}

/// Read all `LogRecord`s under `dir`, filtered by `job_id` and optionally `run_id`,
/// in file (chronological day) then line order. Ignores non-`.jsonl` entries and
/// unparseable lines. A missing dir reads as empty.
#[allow(dead_code)]
pub(crate) fn read_logs(dir: &Path, job_id: &str, run_id: Option<&str>) -> Result<Vec<LogRecord>> {
    let mut files: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };
    files.sort(); // date-named → chronological
    let mut out = Vec::new();
    for path in files {
        let file = File::open(&path).map_err(Error::Io)?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(Error::Io)?;
            if line.trim().is_empty() { continue; }
            let Ok(rec) = serde_json::from_str::<LogRecord>(&line) else { continue; };
            if rec.job_id != job_id { continue; }
            if let Some(want) = run_id && rec.run_id != want { continue; }
            out.push(rec);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn rec(ts_secs: i64, job: &str, run: &str, stream: &str, text: &str) -> LogRecord {
        let ts = Utc.timestamp_opt(ts_secs, 0).unwrap().to_rfc3339();
        LogRecord { ts, job_id: job.into(), run_id: run.into(), attempt: 1,
            stream: stream.into(), text: text.into() }
    }

    #[test]
    fn append_then_read_filters_by_job_and_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = DailyLogWriter::new(dir.path().to_path_buf());
        let day = Utc.with_ymd_and_hms(2026, 6, 20, 12, 0, 0).unwrap().timestamp();
        w.append(&rec(day, "cleanup", "r1", "stdout", "hello")).unwrap();
        w.append(&rec(day, "cleanup", "r2", "stderr", "other")).unwrap();
        w.append(&rec(day, "backup", "r9", "stdout", "nope")).unwrap();

        let all = read_logs(dir.path(), "cleanup", None).unwrap();
        assert_eq!(all.len(), 2);
        let r1 = read_logs(dir.path(), "cleanup", Some("r1")).unwrap();
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].text, "hello");
    }

    #[test]
    fn lines_land_in_file_for_their_own_day() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = DailyLogWriter::new(dir.path().to_path_buf());
        let d20 = Utc.with_ymd_and_hms(2026, 6, 20, 23, 59, 0).unwrap().timestamp();
        let d21 = Utc.with_ymd_and_hms(2026, 6, 21, 0, 1, 0).unwrap().timestamp();
        w.append(&rec(d20, "j", "r", "stdout", "a")).unwrap();
        w.append(&rec(d21, "j", "r", "stdout", "b")).unwrap();
        assert!(dir.path().join("2026-06-20.jsonl").exists());
        assert!(dir.path().join("2026-06-21.jsonl").exists());
        assert_eq!(read_logs(dir.path(), "j", Some("r")).unwrap().len(), 2);
    }

    #[test]
    fn read_logs_on_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_logs(&dir.path().join("nope"), "x", None).unwrap().is_empty());
    }
}
