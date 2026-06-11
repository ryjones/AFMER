//! Loads every FFL (Federal Firearms License) snapshot found in the FFLS/ data
//! set and folds them into a temporal dimension: for each license (keyed by its
//! AFMER RDS key) we keep the *distinct* versions of its metadata over time, each
//! tagged with the snapshot range over which that version was observed. AFMER
//! production reports are then enriched against the version effective for the
//! report's year so historic rows line up with how the license actually looked
//! at the time.

use anyhow::{anyhow, Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The mutable metadata fields tracked for each license, mirroring the columns
/// shared by the `*-ffl-list` and `rds*` source files.
#[derive(Clone, Default)]
pub struct FflRecord {
    pub lic_regn: String,
    pub lic_dist: String,
    pub lic_cnty: String,
    pub lic_type: String,
    pub lic_xprdte: String,
    pub lic_seqn: String,
    pub license_name: String,
    pub business_name: String,
    pub premise_street: String,
    pub premise_city: String,
    pub premise_state: String,
    pub premise_zip: String,
    pub mail_street: String,
    pub mail_city: String,
    pub mail_state: String,
    pub mail_zip: String,
    pub voice_phone: String,
}

impl FflRecord {
    /// AFMER RDS key: region digit + 2-digit district + 5-digit sequence.
    /// e.g. region 1, district 66, seqn 332 -> "16600332".
    fn rds_key(&self) -> Option<String> {
        let regn: u32 = self.lic_regn.trim().parse().ok()?;
        let dist: u32 = self.lic_dist.trim().parse().ok()?;
        let seqn: u32 = self.lic_seqn.trim().parse().ok()?;
        Some(format!("{regn}{dist:02}{seqn:05}"))
    }

    /// Hash of the metadata fields, used to collapse consecutive identical
    /// snapshots into a single version.
    fn content_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        for f in [
            &self.lic_cnty,
            &self.lic_type,
            &self.lic_xprdte,
            &self.license_name,
            &self.business_name,
            &self.premise_street,
            &self.premise_city,
            &self.premise_state,
            &self.premise_zip,
            &self.mail_street,
            &self.mail_city,
            &self.mail_state,
            &self.mail_zip,
            &self.voice_phone,
        ] {
            f.hash(&mut h);
        }
        h.finish()
    }
}

/// One distinct version of a license's metadata and the snapshot window over
/// which it was observed (`YYYY-MM` strings, inclusive).
pub struct FflVersion {
    pub record: FflRecord,
    pub first_seen: String,
    pub last_seen: String,
    pub first_year: i32,
    pub last_year: i32,
    pub seq: u32,
    hash: u64,
}

/// A single source file plus the snapshot date parsed from its name.
struct Snapshot {
    file: PathBuf,
    label: String, // YYYY-MM
    year: i32,
    month: u32,
}

pub struct SnapshotMeta {
    pub label: String,
    pub source_file: String,
    pub row_count: usize,
}

/// The assembled temporal FFL dimension.
pub struct FflStore {
    /// rds_key -> chronological list of distinct versions
    pub versions: HashMap<String, Vec<FflVersion>>,
    pub snapshots: Vec<SnapshotMeta>,
}

impl FflStore {
    /// Synthesize an RDS key for a record that lacks one, by matching its
    /// license name + premise city + state against the FFL dimension. Returns a
    /// key only when the match is unambiguous (exactly one license matches).
    pub fn synthesize_key(&self, name: &str, city: &str, state: &str) -> Option<String> {
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase();
        let (n, c, s) = (norm(name), norm(city), norm(state));
        if n.is_empty() {
            return None;
        }
        let mut found: Option<String> = None;
        for (key, chain) in &self.versions {
            let hit = chain.iter().any(|v| {
                norm(&v.record.license_name) == n
                    && norm(&v.record.premise_city) == c
                    && norm(&v.record.premise_state) == s
            });
            if hit {
                match &found {
                    Some(k) if k != key => return None, // ambiguous
                    _ => found = Some(key.clone()),
                }
            }
        }
        found
    }

    /// Pick the version effective for `year`: the version whose observed window
    /// contains the year, else the one whose window is nearest to it. Ties break
    /// toward the most recent version.
    pub fn version_for_year(&self, key: &str, year: i32) -> Option<&FflVersion> {
        let vs = self.versions.get(key)?;
        vs.iter().min_by_key(|v| {
            let dist = if year < v.first_year {
                v.first_year - year
            } else if year > v.last_year {
                year - v.last_year
            } else {
                0
            };
            // (distance, then prefer later version on ties via negative seq)
            (dist, -(v.seq as i64))
        })
    }
}

/// Expand a list of paths (files or directories) into FFL snapshot files,
/// ordered chronologically. Unrecognized filenames are skipped with a warning.
fn collect_snapshots(inputs: &[PathBuf]) -> Result<Vec<Snapshot>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in inputs {
        if p.is_dir() {
            for entry in std::fs::read_dir(p)
                .with_context(|| format!("reading FFL directory {}", p.display()))?
            {
                let path = entry?.path();
                if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("csv") | Some("xlsx")
                ) {
                    files.push(path);
                }
            }
        } else {
            files.push(p.clone());
        }
    }

    let mut snaps: Vec<Snapshot> = Vec::new();
    for file in files {
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match parse_snapshot_date(&stem) {
            Some((year, month)) => snaps.push(Snapshot {
                file,
                label: format!("{year:04}-{month:02}"),
                year,
                month,
            }),
            None => eprintln!(
                "  skipping {} (cannot parse snapshot date from name)",
                file.display()
            ),
        }
    }
    snaps.sort_by_key(|s| (s.year, s.month, s.file.clone()));
    Ok(snaps)
}

/// Recognize snapshot dates from the filename stem:
///   `2024-02-ffl-list` -> 2024-02
///   `0126-ffl-list`     -> 2026-01 (MMYY)
///   `rds-2023`          -> 2023-01
fn parse_snapshot_date(stem: &str) -> Option<(i32, u32)> {
    if let Some(rest) = stem.strip_prefix("rds-") {
        let year: i32 = rest.get(0..4)?.parse().ok()?;
        return Some((year, 1));
    }
    let prefix = stem.strip_suffix("-ffl-list")?;
    if let Some((y, m)) = prefix.split_once('-') {
        // YYYY-MM
        if y.len() == 4 && m.len() == 2 {
            return Some((y.parse().ok()?, m.parse().ok()?));
        }
    } else if prefix.len() == 4 && prefix.chars().all(|c| c.is_ascii_digit()) {
        // MMYY
        let month: u32 = prefix[0..2].parse().ok()?;
        let yy: i32 = prefix[2..4].parse().ok()?;
        return Some((2000 + yy, month));
    }
    None
}

/// Load every snapshot and fold them into the temporal store.
pub fn load(inputs: &[PathBuf]) -> Result<FflStore> {
    let snaps = collect_snapshots(inputs)?;
    if snaps.is_empty() {
        return Err(anyhow!("no usable FFL snapshot files found"));
    }

    let mut versions: HashMap<String, Vec<FflVersion>> = HashMap::new();
    let mut meta: Vec<SnapshotMeta> = Vec::new();

    for snap in &snaps {
        let records = read_records(&snap.file)
            .with_context(|| format!("reading FFL file {}", snap.file.display()))?;
        let mut count = 0usize;
        for rec in records {
            let Some(key) = rec.rds_key() else { continue };
            count += 1;
            let hash = rec.content_hash();
            let chain = versions.entry(key).or_default();
            match chain.last_mut() {
                // Same content as the most recent version -> extend its window.
                Some(prev) if prev.hash == hash => {
                    prev.last_seen = snap.label.clone();
                    prev.last_year = snap.year;
                }
                // New or changed -> append a new version.
                _ => {
                    let seq = chain.len() as u32;
                    chain.push(FflVersion {
                        record: rec,
                        first_seen: snap.label.clone(),
                        last_seen: snap.label.clone(),
                        first_year: snap.year,
                        last_year: snap.year,
                        seq,
                        hash,
                    });
                }
            }
        }
        eprintln!(
            "  {} -> {} ({} licenses)",
            snap.file.display(),
            snap.label,
            count
        );
        meta.push(SnapshotMeta {
            label: snap.label.clone(),
            source_file: snap.file.display().to_string(),
            row_count: count,
        });
    }

    Ok(FflStore {
        versions,
        snapshots: meta,
    })
}

/// Read one FFL file (CSV or XLSX) into records, mapping columns by header name.
fn read_records(path: &Path) -> Result<Vec<FflRecord>> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("csv") => read_csv(path),
        Some("xlsx") => read_xlsx(path),
        other => Err(anyhow!("unsupported FFL file type: {:?}", other)),
    }
}

/// Map a normalized (UPPERCASE) header name to the matching field setter.
fn set_field(rec: &mut FflRecord, header: &str, value: &str) {
    let v = value.trim().to_string();
    match header {
        "LIC_REGN" => rec.lic_regn = v,
        "LIC_DIST" => rec.lic_dist = v,
        "LIC_CNTY" => rec.lic_cnty = v,
        "LIC_TYPE" => rec.lic_type = v,
        "LIC_XPRDTE" => rec.lic_xprdte = v,
        "LIC_SEQN" => rec.lic_seqn = v,
        "LICENSE_NAME" | "APP_LICENSE_NAME" => rec.license_name = v,
        "BUSINESS_NAME" => rec.business_name = v,
        "PREMISE_STREET" => rec.premise_street = v,
        "PREMISE_CITY" => rec.premise_city = v,
        "PREMISE_STATE" => rec.premise_state = v,
        "PREMISE_ZIP_CODE" | "PREMISE_ZIP" => rec.premise_zip = v,
        "MAIL_STREET" => rec.mail_street = v,
        "MAIL_CITY" => rec.mail_city = v,
        "MAIL_STATE" => rec.mail_state = v,
        "MAIL_ZIP_CODE" | "MAIL_ZIP" => rec.mail_zip = v,
        "VOICE_PHONE" => rec.voice_phone = v,
        _ => {} // ignore extra columns such as the leading RDS key
    }
}

/// Normalize a column header to a canonical key: strip BOM, uppercase, and
/// collapse internal whitespace to single underscores so that both `LIC_REGN`
/// and `Lic Regn` map to the same name.
fn norm_header(h: &str) -> String {
    h.trim_start_matches('\u{feff}')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .to_ascii_uppercase()
}

fn read_csv(path: &Path) -> Result<Vec<FflRecord>> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)?;
    let headers: Vec<String> = rdr.headers()?.iter().map(norm_header).collect();
    let mut out = Vec::new();
    for result in rdr.records() {
        let row = result?;
        let mut rec = FflRecord::default();
        for (i, field) in row.iter().enumerate() {
            if let Some(h) = headers.get(i) {
                set_field(&mut rec, h, field);
            }
        }
        out.push(rec);
    }
    Ok(out)
}

fn read_xlsx(path: &Path) -> Result<Vec<FflRecord>> {
    use calamine::{open_workbook_auto, Reader};
    let mut wb = open_workbook_auto(path)?;
    let range = wb
        .worksheet_range_at(0)
        .ok_or_else(|| anyhow!("xlsx has no worksheets"))??;
    let mut rows = range.rows();
    let Some(header_row) = rows.next() else {
        return Ok(Vec::new());
    };
    let headers: Vec<String> = header_row
        .iter()
        .map(|c| norm_header(&cell_to_string(c)))
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let mut rec = FflRecord::default();
        for (i, cell) in row.iter().enumerate() {
            if let Some(h) = headers.get(i) {
                set_field(&mut rec, h, &cell_to_string(cell));
            }
        }
        out.push(rec);
    }
    Ok(out)
}

pub(crate) fn cell_to_string(cell: &calamine::Data) -> String {
    use calamine::Data;
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        // Zip codes / phone numbers may arrive as floats; render without ".0".
        Data::Float(f) => {
            if f.fract() == 0.0 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => b.to_string(),
        Data::DateTime(d) => d.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
    }
}
