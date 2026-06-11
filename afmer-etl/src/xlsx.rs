//! Ingests structured AFMER workbooks (`AFMER-YYYY.xlsx`) — used to load the
//! historic years whose PDFs are in formats the PDF parser does not handle.
//!
//! Two shapes are supported, both mapped by column *name* (not position):
//!   * **Historic (2000–2012):** ten category sheets (Pistols MFG, Pistols EXP,
//!     …) each carrying RDS KEY + name/street/city/state + a few count columns.
//!     They are joined by RDS key into one unified row per license.
//!   * **Flat (e.g. 2020, 2021):** a single sheet with one row per license.
//!
//! Count headers vary across years (`PSTL_22`, `PISTOL 22`, `PISTOL 22 MFG`, …)
//! and are reconciled via [`crate::pdf::canonical_count`]. Rows lacking a usable
//! RDS key have one synthesized from the FFL dimension by name/city/state match.

use crate::ffl::{cell_to_string, FflStore};
use crate::pdf::{canonical_count, count_index, AfmerRow, NameSource, COUNT_COLS};
use anyhow::{Context, Result};
use calamine::{open_workbook_auto, Reader};
use std::collections::HashMap;
use std::path::Path;

const N: usize = COUNT_COLS.len();

pub struct Stats {
    pub rows: usize,
    pub keyless_skipped: usize,
    pub keys_synthesized: usize,
    pub ffl_matched: usize,
}

pub enum Outcome {
    Parsed(Vec<AfmerRow>, Stats),
    Unusable(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Key,
    LicType,
    Name,
    Street,
    City,
    State,
    Count(usize),
    Ignore,
}

fn classify(header: &str) -> Role {
    let n = header
        .to_ascii_uppercase()
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match n.as_str() {
        "APP RDS KEY" | "RDS KEY" => Role::Key,
        "APP LIC TYPE" | "LIC TYPE" => Role::LicType,
        "APP LICENSE NAME" | "LICENSE NAME" => Role::Name,
        "APP PREMISE STREET" | "STREET" => Role::Street,
        "APP PREMISE CITY" | "CITY" => Role::City,
        "APP PREMISE STATE" | "STATE" | "ST" => Role::State,
        _ => match canonical_count(header).and_then(count_index) {
            Some(i) => Role::Count(i),
            None => Role::Ignore,
        },
    }
}

/// An accumulating record while joining the historic category sheets.
#[derive(Default)]
struct Rec {
    lic_type: String,
    name: String,
    street: String,
    city: String,
    state: String,
    counts: [i64; N],
}

fn is_valid_key(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_digit())
}

fn parse_count(s: &str) -> i64 {
    s.trim().replace(',', "").parse().unwrap_or(0)
}

pub fn ingest(path: &Path, year: i32, ffl: &FflStore) -> Result<Outcome> {
    let mut wb = open_workbook_auto(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let sheet_names = wb.sheet_names().to_vec();

    let mut recs: HashMap<String, Rec> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut flat_rows: Vec<(String, Rec)> = Vec::new();
    let multi_sheet = sheet_names.len() > 1;
    let mut stats = Stats {
        rows: 0,
        keyless_skipped: 0,
        keys_synthesized: 0,
        ffl_matched: 0,
    };
    let mut saw_counts = false;

    for sheet in &sheet_names {
        let range = match wb.worksheet_range(sheet) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut rows = range.rows();
        let Some(header) = rows.next() else { continue };
        let roles: Vec<Role> = header
            .iter()
            .map(|c| classify(&cell_to_string(c)))
            .collect();
        if roles.iter().any(|r| matches!(r, Role::Count(_))) {
            saw_counts = true;
        }
        // A sheet with no key column is not usable.
        if !roles.iter().any(|r| *r == Role::Key) {
            continue;
        }

        for row in rows {
            let mut rec = Rec::default();
            let mut key = String::new();
            for (i, cell) in row.iter().enumerate() {
                let v = cell_to_string(cell);
                match roles.get(i) {
                    Some(Role::Key) => key = v.trim().to_string(),
                    Some(Role::LicType) => rec.lic_type = v.trim().to_string(),
                    Some(Role::Name) => rec.name = v.trim().to_string(),
                    Some(Role::Street) => rec.street = v.trim().to_string(),
                    Some(Role::City) => rec.city = v.trim().to_string(),
                    Some(Role::State) => rec.state = v.trim().to_string(),
                    Some(Role::Count(idx)) => rec.counts[*idx] = parse_count(&v),
                    _ => {}
                }
            }
            // Resolve / synthesize the RDS key.
            if !is_valid_key(&key) {
                if let Some(k) = ffl.synthesize_key(&rec.name, &rec.city, &rec.state) {
                    key = k;
                    stats.keys_synthesized += 1;
                } else {
                    if !rec.name.is_empty() {
                        stats.keyless_skipped += 1;
                    }
                    continue;
                }
            }

            if multi_sheet {
                let entry = recs.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    Rec::default()
                });
                // Fill identity fields once; merge in this sheet's counts.
                if entry.name.is_empty() { entry.name = rec.name; }
                if entry.lic_type.is_empty() { entry.lic_type = rec.lic_type; }
                if entry.street.is_empty() { entry.street = rec.street; }
                if entry.city.is_empty() { entry.city = rec.city; }
                if entry.state.is_empty() { entry.state = rec.state; }
                for j in 0..N {
                    if rec.counts[j] != 0 {
                        entry.counts[j] = rec.counts[j];
                    }
                }
            } else {
                flat_rows.push((key, rec));
            }
        }
    }

    if !saw_counts {
        return Ok(Outcome::Unusable(
            "no recognizable AFMER count columns".into(),
        ));
    }

    let mut out = Vec::new();
    let mut emit = |key: String, rec: Rec, stats: &mut Stats| {
        let ver = ffl.version_for_year(&key, year);
        if ver.is_some() {
            stats.ffl_matched += 1;
        }
        out.push(AfmerRow {
            rds_key: key,
            app_lic_type: rec.lic_type,
            app_license_name: rec.name,
            app_premise_street: rec.street,
            app_premise_city: rec.city,
            app_premise_state: rec.state,
            counts: rec.counts,
            ffl_version_first_seen: ver.map(|v| v.first_seen.clone()),
            name_source: NameSource::Xlsx,
        });
        stats.rows += 1;
    };

    if multi_sheet {
        for key in order {
            if let Some(rec) = recs.remove(&key) {
                emit(key, rec, &mut stats);
            }
        }
    } else {
        for (key, rec) in flat_rows {
            emit(key, rec, &mut stats);
        }
    }

    Ok(Outcome::Parsed(out, stats))
}
