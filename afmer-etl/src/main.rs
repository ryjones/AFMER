//! AFMER ETL — convert ATF AFMER production-report PDFs into a SQLite database
//! whose layout mirrors the reference AFMER-*.xlsx workbooks, enriched with a
//! temporal FFL license dimension built from every FFL snapshot supplied.
//!
//! Only the current AFMER table format (27 columns, as in 2022/2023) is
//! ingested. The walk-back mode (`--pdf-dir` + optional `--from`) ingests reports
//! newest-first and stops at the first year whose layout differs, reporting that
//! those historic reports need a separate ingest method.

mod db;
mod ffl;
mod histpdf;
mod pdf;
mod xlsx;

use anyhow::{Context, Result};
use clap::Parser;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "afmer-etl",
    about = "Convert AFMER report PDFs into a SQLite database enriched with FFL metadata."
)]
struct Cli {
    /// Explicit AFMER report PDF(s) to ingest (e.g. SOURCE/NU/AFMER-2023.pdf).
    #[arg(long = "pdf", num_args = 1..)]
    pdfs: Vec<PathBuf>,

    /// Directory of AFMER-YYYY.pdf reports. Ingests newest-first, walking back
    /// one year at a time and stopping at the first format change.
    #[arg(long)]
    pdf_dir: Option<PathBuf>,

    /// Year to start the walk-back from (default: newest AFMER-YYYY.pdf found).
    #[arg(long)]
    from: Option<i32>,

    /// Structured AFMER workbook(s) to ingest (e.g. AFMER-2000.xlsx). Used for
    /// historic years; the 10-sheet and flat layouts are auto-detected. A year
    /// already loaded (e.g. from a PDF) is left untouched.
    #[arg(long = "xlsx", num_args = 1..)]
    xlsx: Vec<PathBuf>,

    /// Historic AFMER report PDF(s) with the paginated per-manufacturer "RDS Key"
    /// tables (2013–2019, 2024). A year already loaded is left untouched.
    #[arg(long = "hist-pdf", num_args = 1..)]
    hist_pdf: Vec<PathBuf>,

    /// FFL snapshot files or directories (CSV/XLSX). Directories are scanned for
    /// all *-ffl-list and rds* files. Pass the FFLS/ directory to load everything.
    /// Required when ingesting; not needed for `--export-json` alone.
    #[arg(long = "ffl", num_args = 1..)]
    ffls: Vec<PathBuf>,

    /// Output SQLite database path.
    #[arg(long, default_value = "afmer.db")]
    db: String,

    /// Also export the afmer table to this directory as per-year gzipped JSON
    /// (afmer-YYYY.json.gz) plus meta.json, for the static-site SPA. May be used
    /// alone to export from an existing database.
    #[arg(long)]
    export_json: Option<PathBuf>,

    /// Friendly field-label mapping to load into the DB and the export's
    /// meta.json (default: ./activity-labels.json if present).
    #[arg(long)]
    labels: Option<PathBuf>,
}

/// Pull a 4-digit year out of a filename like `AFMER-2023.pdf`.
fn year_from_path(path: &Path) -> Option<i32> {
    let stem = path.file_stem()?.to_str()?;
    for w in stem.as_bytes().windows(4) {
        if w.iter().all(|b| b.is_ascii_digit()) {
            let y: i32 = std::str::from_utf8(w).ok()?.parse().ok()?;
            if (1990..=2100).contains(&y) {
                return Some(y);
            }
        }
    }
    None
}

/// Discover `AFMER-YYYY.pdf` files in a directory, returning (year, path) pairs.
fn discover_reports(dir: &Path) -> Result<Vec<(i32, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("pdf") {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.to_ascii_uppercase().starts_with("AFMER-") {
            continue;
        }
        if let Some(y) = year_from_path(&path) {
            out.push((y, path));
        }
    }
    Ok(out)
}

/// Ingest one PDF. Returns `true` if it matched the current format and was
/// written, `false` if its layout differs (and was therefore skipped).
fn ingest_one(conn: &mut Connection, store: &ffl::FflStore, path: &Path, year: i32) -> Result<bool> {
    eprintln!("\nParsing {} (report year {})…", path.display(), year);
    match pdf::parse(path, year, store)? {
        pdf::Outcome::Parsed(rows, stats) => {
            let source = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            db::write_afmer(conn, year, &source, &rows)?;
            let pct = if stats.parsed > 0 {
                100.0 * stats.ffl_matched as f64 / stats.parsed as f64
            } else {
                0.0
            };
            eprintln!(
                "  {} rows parsed, {} skipped, {} enriched from FFL ({:.1}%).",
                stats.parsed, stats.skipped, stats.ffl_matched, pct
            );
            Ok(true)
        }
        pdf::Outcome::FormatMismatch { found } => {
            match found {
                Some(h) => eprintln!("  format differs — header is not the current 27-column layout:\n    {h}"),
                None => eprintln!("  format differs — no AFMER table header found (prose-style report)."),
            }
            Ok(false)
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let has_ingest = !cli.pdfs.is_empty()
        || cli.pdf_dir.is_some()
        || !cli.xlsx.is_empty()
        || !cli.hist_pdf.is_empty();
    if !has_ingest && cli.export_json.is_none() {
        anyhow::bail!("provide an ingest source (--pdf/--pdf-dir/--xlsx/--hist-pdf) and/or --export-json");
    }

    let mut conn = db::open(&cli.db)?;

    // Field labels: explicit --labels, else the default file if it exists. Stored
    // in the DB for continuity and surfaced in the export's meta.json.
    let labels_path = cli.labels.clone().or_else(|| {
        let p = PathBuf::from("activity-labels.json");
        p.exists().then_some(p)
    });
    if let Some(p) = &labels_path {
        match db::write_labels(&conn, p) {
            Ok(n) => eprintln!("Loaded {n} field labels from {}.", p.display()),
            Err(e) => eprintln!("Warning: could not load labels from {}: {e}", p.display()),
        }
    }

    // FFL is only needed for ingestion (it drives enrichment and name splitting).
    let store = if has_ingest {
        if cli.ffls.is_empty() {
            anyhow::bail!("ingesting requires --ffl <files/dirs>");
        }
        eprintln!("Loading FFL snapshots…");
        let store = ffl::load(&cli.ffls)?;
        let n_versions: usize = store.versions.values().map(|v| v.len()).sum();
        let n_changed = store.versions.values().filter(|v| v.len() > 1).count();
        eprintln!(
            "FFL: {} snapshots, {} licenses, {} distinct versions ({} licenses changed over time).",
            store.snapshots.len(),
            store.versions.len(),
            n_versions,
            n_changed
        );
        db::write_ffl(&mut conn, &store)?;
        store
    } else {
        ffl::FflStore {
            versions: Default::default(),
            snapshots: Vec::new(),
        }
    };

    // Explicit files: ingest each, header-validated; a mismatch is reported but
    // does not stop the others.
    for pdf_path in &cli.pdfs {
        let year = year_from_path(pdf_path).with_context(|| {
            format!("could not determine report year for {}", pdf_path.display())
        })?;
        ingest_one(&mut conn, &store, pdf_path, year)?;
    }

    // Walk-back: newest-first, stop at the first format change.
    if let Some(dir) = &cli.pdf_dir {
        let mut reports = discover_reports(dir)?;
        reports.sort_by_key(|(y, _)| std::cmp::Reverse(*y));
        let start = cli.from.unwrap_or_else(|| reports.first().map(|(y, _)| *y).unwrap_or(0));
        eprintln!("\nWalking back from {start} in {}…", dir.display());
        let mut ingested = Vec::new();
        for (year, path) in reports.into_iter().filter(|(y, _)| *y <= start) {
            if ingest_one(&mut conn, &store, &path, year)? {
                ingested.push(year);
            } else if ingested.is_empty() {
                // Newer reports may already use a different format; skip past
                // them until the current-format run begins.
                eprintln!("  (skipping {year}; not the current format)");
            } else {
                eprintln!(
                    "\nFormat changed at {year}: this and earlier reports need a separate \
                     ingest method. Stopping walk-back."
                );
                break;
            }
        }
        if !ingested.is_empty() {
            ingested.sort();
            eprintln!(
                "Walk-back ingested years: {}",
                ingested
                    .iter()
                    .map(|y| y.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // Structured workbooks (historic years). Skip any year already ingested so
    // a validated PDF year is not clobbered by a thinner workbook.
    for path in &cli.xlsx {
        let Some(year) = year_from_path(path) else {
            eprintln!("\n{}: cannot determine year; skipping", path.display());
            continue;
        };
        eprintln!("\nIngesting workbook {} (year {year})…", path.display());
        if db::year_present(&conn, year)? {
            eprintln!("  year {year} already loaded; skipping workbook.");
            continue;
        }
        match xlsx::ingest(path, year, &store)? {
            xlsx::Outcome::Parsed(rows, st) => {
                let source = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
                db::write_afmer(&mut conn, year, &source, &rows)?;
                eprintln!(
                    "  {} rows ({} keys synthesized, {} keyless skipped, {} FFL-enriched).",
                    st.rows, st.keys_synthesized, st.keyless_skipped, st.ffl_matched
                );
            }
            xlsx::Outcome::Unusable(why) => {
                eprintln!("  not usable for history: {why}");
            }
        }
    }

    // Historic per-manufacturer PDFs (2013–2019, 2024).
    for path in &cli.hist_pdf {
        let Some(year) = year_from_path(path) else {
            eprintln!("\n{}: cannot determine year; skipping", path.display());
            continue;
        };
        eprintln!("\nParsing historic PDF {} (year {year})…", path.display());
        if db::year_present(&conn, year)? {
            eprintln!("  year {year} already loaded; skipping.");
            continue;
        }
        match histpdf::parse(path, year, &store)? {
            histpdf::Outcome::Parsed(rows, st) => {
                let source = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
                db::write_afmer(&mut conn, year, &source, &rows)?;
                eprintln!(
                    "  {} licenses from {} sections; {} FFL-enriched, {} name/city unresolved.",
                    st.rows, st.sections, st.ffl_matched, st.unsplit
                );
            }
            histpdf::Outcome::Unusable(why) => eprintln!("  not usable: {why}"),
        }
    }

    if let Some(dir) = &cli.export_json {
        eprintln!("\nExporting JSON to {}…", dir.display());
        let (n_years, n_rows) = db::export_json(&conn, dir)?;
        eprintln!("  wrote {n_rows} rows across {n_years} per-year files + meta.json.");
    }

    eprintln!("\nDone -> {}", cli.db);
    Ok(())
}
