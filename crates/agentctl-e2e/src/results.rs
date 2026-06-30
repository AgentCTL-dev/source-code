// SPDX-License-Identifier: BUSL-1.1
//! Benchmark results sink: a per-run `e2e/results/<ts>/` directory with CSV tables
//! and a `summary.json` (both git-ignored; the rendered `docs/benchmarks.md` is the
//! committed artifact).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;

/// A timestamped results directory under a base (e.g. `e2e/results/`).
#[derive(Debug, Clone)]
pub struct ResultsDir {
    /// The concrete `<base>/<ts>` directory.
    pub dir: PathBuf,
    /// The unix-second stamp that named the directory (also the report's run id).
    pub stamp: u64,
}

impl ResultsDir {
    /// Create `<base>/<unix_seconds>/`.
    pub fn create(base: &Path) -> Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = base.join(stamp.to_string());
        fs::create_dir_all(&dir).with_context(|| format!("create results dir {dir:?}"))?;
        Ok(ResultsDir { dir, stamp })
    }

    /// Open an existing results directory (for `--report`), reusing its basename as
    /// the stamp when it parses as a unix second.
    pub fn open(dir: &Path) -> Result<Self> {
        let stamp = dir
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(ResultsDir {
            dir: dir.to_path_buf(),
            stamp,
        })
    }

    /// Write a CSV file `<name>.csv` from a header row + data rows.
    pub fn write_csv(&self, name: &str, headers: &[&str], rows: &[Vec<String>]) -> Result<PathBuf> {
        let mut out = String::new();
        out.push_str(&csv_row(
            &headers.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        ));
        out.push('\n');
        for r in rows {
            out.push_str(&csv_row(r));
            out.push('\n');
        }
        let path = self.dir.join(format!("{name}.csv"));
        fs::write(&path, out).with_context(|| format!("write {path:?}"))?;
        Ok(path)
    }

    /// Write a pretty-printed JSON file `<name>.json`.
    pub fn write_json(&self, name: &str, value: &Value) -> Result<PathBuf> {
        let path = self.dir.join(format!("{name}.json"));
        let body = serde_json::to_string_pretty(value).context("serialize results json")?;
        fs::write(&path, body).with_context(|| format!("write {path:?}"))?;
        Ok(path)
    }

    /// Read back `<name>.json` (used by the `--report` renderer).
    pub fn read_json(&self, name: &str) -> Result<Value> {
        let path = self.dir.join(format!("{name}.json"));
        let body = fs::read_to_string(&path).with_context(|| format!("read {path:?}"))?;
        serde_json::from_str(&body).with_context(|| format!("parse {path:?}"))
    }
}

/// Render one CSV record, quoting fields that contain a comma, quote, or newline
/// (RFC 4180: a `"` inside a quoted field is doubled).
fn csv_row(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| {
            if f.contains([',', '"', '\n']) {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_special_fields() {
        let row = csv_row(&["a".into(), "b,c".into(), "d\"e".into()]);
        assert_eq!(row, "a,\"b,c\",\"d\"\"e\"");
    }
}
