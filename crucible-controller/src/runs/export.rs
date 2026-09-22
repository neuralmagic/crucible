//! On-demand parquet analytics sidecars, served at `GET /api/export/runs.parquet` and
//! `/api/export/iterations.parquet`. These replaced the flat parquet files the retired
//! `crucible report` SSG wrote next to its static site, so DuckDB keeps reading the same
//! analytics off the controller API. This module is now the schema's single owner.
//!
//! Columns cover everything the controller's ledger can source. The old SSG's artifact-only
//! columns — `uuid`, `goal`, `gate`, `model`, `namespace`, `baseline`, `improvement_pct`,
//! `elapsed_secs`, `segments`, `base_sha`, `kept_commits`, `pr_urls`, `escalation` (and
//! per-iteration `note`, `diffstat`, `tool_calls`, `transcript_chars`) — live in
//! `session.jsonl`/`summary.json`, not the `runs`/`candidates` tables, so they are
//! intentionally OMITTED here rather than faked as NULLs. The controller adds `issue_key`,
//! the provenance key the published-run file layout doesn't carry.

use anyhow::{Context, Result};
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::record::RecordWriter;
use parquet_derive::ParquetRecordWriter;

/// One `runs.parquet` row. `improved` folds the run's keep decisions (any kept candidate beat the
/// running best, so `kept > 0` means the run improved — a direction-free fallback, since no summary
/// verdict is available from the DB).
#[derive(Debug, Clone, PartialEq, ParquetRecordWriter)]
pub struct RunParquetRow {
    pub(crate) run_id: String,
    pub(crate) issue_key: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) status: String,
    pub(crate) improved: bool,
    pub(crate) best: Option<f64>,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) iterations: i64,
    pub(crate) kept: i64,
}

/// One `iterations.parquet` row, including the controller's `kind`/`lane` (wide-lane vs deep-iter
/// provenance).
#[derive(Debug, Clone, PartialEq, ParquetRecordWriter)]
pub struct IterParquetRow {
    pub(crate) run_id: String,
    pub(crate) kind: Option<String>,
    pub(crate) lane: Option<i64>,
    pub(crate) iter: Option<i64>,
    pub(crate) score: Option<f64>,
    pub(crate) decision: Option<String>,
}

/// Serialize the run rows into an in-memory snappy parquet file (`runs.parquet`).
pub(crate) fn write_run_parquet(rows: &[RunParquetRow]) -> Result<Vec<u8>> {
    write_table(rows)
}

/// Serialize the iteration rows into an in-memory snappy parquet file (`iterations.parquet`).
pub(crate) fn write_iter_parquet(rows: &[IterParquetRow]) -> Result<Vec<u8>> {
    write_table(rows)
}

/// Write one derive-schema'd table as a single-row-group, snappy-compressed parquet file into an
/// in-memory buffer. Bounded by the table size (the full `runs`/`candidates` set), which is fine
/// to buffer; these are analytics dumps, not the streaming artifact path.
fn write_table<T>(rows: &[T]) -> Result<Vec<u8>>
where
    for<'a> &'a [T]: RecordWriter<T>,
{
    let schema = rows.schema().context("deriving parquet schema")?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut buf: Vec<u8> = Vec::new();
    // `&mut Vec<u8>` is the `Write` sink; `close()` consumes the writer, ending the borrow so the
    // buffer is free to return.
    let mut writer = SerializedFileWriter::new(&mut buf, schema, props.into())
        .context("opening parquet writer")?;
    let mut group = writer.next_row_group().context("opening row group")?;
    rows.write_to_row_group(&mut group)
        .context("writing rows to row group")?;
    group.close().context("closing row group")?;
    writer.close().context("closing parquet writer")?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;
    use std::io::Write;

    /// Read a parquet buffer back and return (row_count, rows) via a temp file (`std::fs::File`
    /// implements the parquet `ChunkReader`), the reader half of the round-trip.
    fn read_back(buf: &[u8]) -> (i64, Vec<parquet::record::Row>) {
        let mut tmp = tempfile::NamedTempFile::new().expect("temp parquet");
        tmp.write_all(buf).expect("write parquet");
        tmp.flush().expect("flush");
        let file = tmp.reopen().expect("reopen");
        let reader = SerializedFileReader::new(file).expect("parquet reader");
        let n = reader.metadata().file_metadata().num_rows();
        let rows = reader
            .get_row_iter(None)
            .expect("row iter")
            .map(|r| r.expect("row"))
            .collect();
        (n, rows)
    }

    #[test]
    fn run_parquet_round_trips_rows_and_a_sampled_field() {
        let rows = vec![
            RunParquetRow {
                run_id: "20260704T101112Z-raise-it".into(),
                issue_key: Some("o/r#1".into()),
                repo: Some("o/r".into()),
                status: "finished".into(),
                improved: true,
                best: Some(234.0),
                cost_usd: Some(1.23),
                iterations: 2,
                kept: 1,
            },
            RunParquetRow {
                run_id: "20260704T090000Z-other".into(),
                issue_key: None,
                repo: None,
                status: "incomplete".into(),
                improved: false,
                best: None,
                cost_usd: None,
                iterations: 0,
                kept: 0,
            },
        ];
        let buf = write_run_parquet(&rows).expect("write");
        let (n, read) = read_back(&buf);
        assert_eq!(n, 2, "two run rows");
        assert_eq!(read.len(), 2);
        // Sample the first row's run_id (col 0) and improved (col 4).
        assert_eq!(read[0].get_string(0).expect("run_id"), &rows[0].run_id);
        assert!(read[0].get_bool(4).expect("improved"));
    }

    #[test]
    fn iter_parquet_round_trips_rows_and_a_sampled_field() {
        let rows = vec![
            IterParquetRow {
                run_id: "run-1".into(),
                kind: Some("deep".into()),
                lane: None,
                iter: Some(1),
                score: Some(240.0),
                decision: Some("keep".into()),
            },
            IterParquetRow {
                run_id: "run-1".into(),
                kind: Some("deep".into()),
                lane: None,
                iter: Some(2),
                score: Some(255.0),
                decision: Some("reject".into()),
            },
        ];
        let buf = write_iter_parquet(&rows).expect("write");
        let (n, read) = read_back(&buf);
        assert_eq!(n, 2, "two iteration rows");
        // Sample the second row's iter (col 3) and decision (col 5).
        assert_eq!(read[1].get_long(3).expect("iter"), 2);
        assert_eq!(read[1].get_string(5).expect("decision"), "reject");
    }
}
