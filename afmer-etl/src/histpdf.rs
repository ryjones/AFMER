//! Parses the *historic* AFMER report PDFs (2013–2019, 2024) — the paginated
//! per-manufacturer "RDS Key" tables.
//!
//! These reports are organized into ten category sections (Pistols/Revolvers/
//! Rifles/Shotguns/Misc × Manufactured/Exported). Each section repeats a column
//! header `RDS Key | License Name | Street | City | State | <counts…>` and then
//! lists one row per licensee. The count columns — and crucially their **order**
//! — vary by year (e.g. the revolver caliber order differs between 2018 and
//! 2024), so the count layout is read from each section's header line rather than
//! assumed. Rows are joined by RDS key across sections into one unified record,
//! reusing the same count columns, FFL enrichment, and name/street/city split as
//! the rest of the pipeline.

use crate::ffl::{FflStore, FflVersion};
use crate::pdf::{canonical_count, count_index, AfmerRow, NameSource, COUNT_COLS};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

const N: usize = COUNT_COLS.len();

pub struct Stats {
    pub rows: usize,
    pub sections: usize,
    pub ffl_matched: usize,
    pub unsplit: usize, // rows where name/city could not be resolved from FFL
}

pub enum Outcome {
    Parsed(Vec<AfmerRow>, Stats),
    Unusable(String),
}

#[derive(Default)]
struct Rec {
    name: String,
    street: String,
    city: String,
    state: String,
    counts: [i64; N],
    resolved: bool, // name/city resolved against FFL (vs. heuristic split)
}

/// Parse a section column-header line into the ordered list of count-column
/// indices that follow the `State` column. Returns empty if unrecognized.
fn parse_count_columns(line: &str) -> Vec<usize> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let Some(p) = toks.iter().position(|t| t.eq_ignore_ascii_case("state")) else {
        return Vec::new();
    };
    let rest = &toks[p + 1..];
    if rest.is_empty() {
        return Vec::new();
    }
    let mut cols = Vec::new();
    let first = rest[0].to_ascii_uppercase();
    if first == "PISTOL" || first == "REVOLVER" {
        // Per-caliber columns are two tokens each ("Pistol 22", "Revolver Total").
        let mut i = 0;
        while i + 1 < rest.len() {
            let name = format!("{} {}", rest[i], rest[i + 1]);
            if let Some(c) = canonical_count(&name).and_then(count_index) {
                cols.push(c);
            }
            i += 2;
        }
    } else {
        // Single-count section: the whole remainder names one column.
        if let Some(c) = canonical_count(&rest.join(" ")).and_then(count_index) {
            cols.push(c);
        }
    }
    cols
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase()
}

/// Split the blob (everything between RDS key and state) into name/street/city,
/// using the effective FFL version when available.
fn split_fields(blob: &[&str], ver: Option<&FflVersion>) -> (String, String, String, bool) {
    if let Some(v) = ver {
        let want_name = norm(&v.record.license_name);
        let want_city = norm(&v.record.premise_city);
        let name_k = (1..=blob.len()).find(|&k| norm(&blob[..k].join(" ")) == want_name);
        let city_j = if want_city.is_empty() {
            None
        } else {
            (0..blob.len()).find(|&j| norm(&blob[j..].join(" ")) == want_city)
        };
        match (name_k, city_j) {
            (Some(k), Some(j)) if k <= j => {
                return (
                    blob[..k].join(" "),
                    blob[k..j].join(" "),
                    blob[j..].join(" "),
                    true,
                );
            }
            (Some(k), _) => {
                return (blob[..k].join(" "), blob[k..].join(" "), String::new(), true);
            }
            (None, Some(j)) => {
                let (name, street) = heuristic_name_street(&blob[..j]);
                return (name, street, blob[j..].join(" "), true);
            }
            _ => {}
        }
    }
    let (name, street) = heuristic_name_street(blob);
    (name, street, String::new(), false)
}

/// Fallback: the street begins at the first token starting with a digit.
fn heuristic_name_street(blob: &[&str]) -> (String, String) {
    let split = blob
        .iter()
        .position(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .filter(|&i| i >= 1)
        .unwrap_or(blob.len());
    (blob[..split].join(" "), blob[split..].join(" "))
}

pub fn parse(path: &Path, year: i32, ffl: &FflStore) -> Result<Outcome> {
    let text = pdf_extract::extract_text(path.to_str().context("non-utf8 pdf path")?)
        .with_context(|| format!("extracting text from {}", path.display()))?;

    let mut recs: HashMap<String, Rec> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut cols: Vec<usize> = Vec::new();
    let mut sections = 0usize;
    let mut unsplit = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        // A column-header line begins a section and defines its count columns.
        if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("RDS Key") {
            let new_cols = parse_count_columns(trimmed);
            if !new_cols.is_empty() {
                cols = new_cols;
                sections += 1;
            }
            continue;
        }
        if cols.is_empty() {
            continue;
        }

        let toks: Vec<&str> = trimmed.split_whitespace().collect();
        let n = cols.len();
        // key + (>=1 blob) + state + n counts
        if toks.len() < n + 3 {
            continue;
        }
        let key = toks[0];
        if key.len() < 6 || key.len() > 9 || !key.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let state_idx = toks.len() - n - 1;
        let state = toks[state_idx];
        if state.len() != 2 || !state.chars().all(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        let mut row_counts = [0i64; N];
        let mut ok = true;
        for (i, tok) in toks[state_idx + 1..].iter().enumerate() {
            match tok.replace(',', "").parse::<i64>() {
                Ok(v) => row_counts[cols[i]] = v,
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }

        let blob = &toks[1..state_idx];
        let ver = ffl.version_for_year(key, year);
        let (name, street, city, resolved) = split_fields(blob, ver);
        if !resolved {
            unsplit += 1;
        }

        let entry = recs.entry(key.to_string()).or_insert_with(|| {
            order.push(key.to_string());
            Rec::default()
        });
        if entry.name.is_empty() {
            entry.name = name;
        }
        if entry.street.is_empty() {
            entry.street = street;
        }
        if entry.city.is_empty() {
            entry.city = city;
        }
        if entry.state.is_empty() {
            entry.state = state.to_string();
        }
        entry.resolved |= resolved;
        for j in 0..N {
            if row_counts[j] != 0 {
                entry.counts[j] = row_counts[j];
            }
        }
    }

    if sections == 0 {
        return Ok(Outcome::Unusable(
            "no per-manufacturer 'RDS Key' sections found (aggregate-only report?)".into(),
        ));
    }

    let mut stats = Stats {
        rows: 0,
        sections,
        ffl_matched: 0,
        unsplit,
    };
    let mut out = Vec::new();
    for key in order {
        let rec = recs.remove(&key).unwrap();
        let ver = ffl.version_for_year(&key, year);
        if ver.is_some() {
            stats.ffl_matched += 1;
        }
        out.push(AfmerRow {
            rds_key: key,
            app_lic_type: String::new(),
            app_license_name: rec.name,
            app_premise_street: rec.street,
            app_premise_city: rec.city,
            app_premise_state: rec.state,
            counts: rec.counts,
            ffl_version_first_seen: ver.map(|v| v.first_seen.clone()),
            name_source: if rec.resolved {
                NameSource::FflPrefix
            } else {
                NameSource::Heuristic
            },
        });
        stats.rows += 1;
    }
    Ok(Outcome::Parsed(out, stats))
}
