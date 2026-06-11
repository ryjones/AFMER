//! Parses an ATF AFMER production-report PDF into structured rows.
//!
//! Each data line in the PDF is one manufacturer record laid out as:
//!
//!   RDS_KEY  LIC_TYPE  <license name ...>  <premise street ...>  STATE  c1 c2 ... c22
//!
//! The trailing 22 tokens are integer production/export counts and the token
//! before them is the 2-letter state, so the record is parsed deterministically
//! from the right edge. The only ambiguous part is the boundary between the
//! license name and the street (both contain spaces with no delimiter); that
//! split is resolved against the temporal FFL metadata, falling back to a
//! house-number heuristic when no license version is available.

use crate::ffl::{FflStore, FflVersion};
use anyhow::{Context, Result};
use std::path::Path;

/// The 22 count columns, in source order after the state field.
pub const COUNT_COLS: [&str; 22] = [
    "pstl_22", "pstl_25", "pstl_32", "pstl_380", "pstl_9mm", "pstl_50", "pstl_totl", "rvlr_22",
    "rvlr_32", "rvlr_38", "rvlr_357", "rvlr_44", "rvlr_50", "rvlr_totl", "rifle_mfg", "shotgun_mfg",
    "mis_fam", "pistol_exp", "revolver_exp", "rifle_exp", "shotgun_exp", "misc_fa_exp",
];

const N_COUNTS: usize = COUNT_COLS.len();

/// The five non-count columns of the current AFMER table format, in order.
pub const LEAD_COLS: [&str; 5] = [
    "APP_RDS_KEY",
    "APP_LIC_TYPE",
    "APP_LICENSE_NAME",
    "APP_PREMISE_CITY",
    "APP_PREMISE_STATE",
];

/// Outcome of inspecting a PDF: either parsed rows, or a header that does not
/// match the current format (an older/variant report needing a new ingester).
pub enum Outcome {
    Parsed(Vec<AfmerRow>, ParseStats),
    FormatMismatch { found: Option<String> },
}

/// The canonical current-format header line (27 columns).
fn canonical_header() -> Vec<String> {
    LEAD_COLS
        .iter()
        .map(|s| s.to_string())
        .chain(COUNT_COLS.iter().map(|s| s.to_ascii_uppercase()))
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
pub enum NameSource {
    FflPrefix,
    FflSuffix,
    Heuristic,
    /// Name/address taken verbatim from a structured XLSX source.
    Xlsx,
}

impl NameSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NameSource::FflPrefix => "ffl-prefix",
            NameSource::FflSuffix => "ffl-suffix",
            NameSource::Heuristic => "heuristic",
            NameSource::Xlsx => "xlsx",
        }
    }
}

pub struct AfmerRow {
    pub rds_key: String,
    pub app_lic_type: String,
    pub app_license_name: String,
    pub app_premise_street: String, // populated for XLSX sources that carry it
    pub app_premise_city: String,   // in PDF sources this column holds the street
    pub app_premise_state: String,
    pub counts: [i64; N_COUNTS],
    pub ffl_version_first_seen: Option<String>,
    pub name_source: NameSource,
}

/// Map any AFMER count-column header (across PDF and XLSX format variants) to its
/// canonical column name, or `None` if it is not a count column.
pub fn canonical_count(header: &str) -> Option<&'static str> {
    // Normalize: uppercase, drop '/', treat separators as spaces, collapse.
    let cleaned: String = header
        .to_ascii_uppercase()
        .chars()
        .map(|c| if c == '/' { ' ' } else if c == '_' { ' ' } else { c })
        .collect();
    let n = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(match n.as_str() {
        "PSTL 22" | "PISTOL 22" | "PISTOL 22 MFG" => "pstl_22",
        "PSTL 25" | "PISTOL 25" | "PISTOL 25 MFG" => "pstl_25",
        "PSTL 32" | "PISTOL 32" | "PISTOL 32 MFG" => "pstl_32",
        "PSTL 380" | "PISTOL 380" | "PISTOL 380 MFG" => "pstl_380",
        "PSTL 9MM" | "PISTOL 9MM" | "PISTOL 9 MFG" | "PISTOL 9MM MFG" => "pstl_9mm",
        "PSTL 50" | "PISTOL 50" | "PISTOL 50 MFG" => "pstl_50",
        "PSTL TOTL" | "PISTOL TOTAL" | "PISTOL MFG TOTAL" => "pstl_totl",
        "RVLR 22" | "RVLR 22 MFG" | "REVOLVER 22" => "rvlr_22",
        "RVLR 32" | "RVLR 32 MFG" | "REVOLVER 32" => "rvlr_32",
        "RVLR 38" | "RVLR 38 MFG" | "REVOLVER 38" => "rvlr_38",
        "RVLR 357" | "RVLR 357 MFG" | "REVOLVER 357" => "rvlr_357",
        "RVLR 44" | "RVLR 44 MFG" | "REVOLVER 44" => "rvlr_44",
        "RVLR 50" | "RVLR 50 MFG" | "REVOLVER 50" => "rvlr_50",
        "RVLR TOTL" | "RVLR TOTAL" | "RVLR MFG TOTAL" | "REVOLVER TOTAL" | "REVOLVER TOTL" => {
            "rvlr_totl"
        }
        "RIFLE MFG" | "RIFLE MANUFACTURED" | "RIFLES MANUFACTURED" => "rifle_mfg",
        "SHOTGUN MFG" | "SHOTGUN MANUFACTURED" | "SHOTGUNS MANUFACTURED" => "shotgun_mfg",
        "MIS FAM" | "MISC FA MFG" | "MISCELLANEOUS FIREARMS MANUFACTURED"
        | "MISC FIREARMS MANUFACTURED" | "MICELLANEOUS FIREARMS MANUFACTURED" => "mis_fam",
        "PISTOL EXP" | "PISTOLS EXP" | "PISTOLS EXPORTED" => "pistol_exp",
        "RVLR EXP" | "REVOLVER EXP" | "REVOLVERS EXP" | "REVOLVERS EXPORTED" => "revolver_exp",
        "RIFLE EXP" | "RIFLES EXP" | "RIFLES EXPORTED" => "rifle_exp",
        "SHOTGUN EXP" | "SHOTGUNS EXP" | "SHOTGUNS EXPORTED" => "shotgun_exp",
        "MISC FA EXP" | "MISCELLANEOUS FIREARMS EXPORTED" | "MICELLANEOUS FIREARMS EXPORTED"
        | "MISC FIREARMS EXPORTED" => "misc_fa_exp",
        _ => return None,
    })
}

/// Index of `COUNT_COLS` name -> position, for building count arrays by name.
pub fn count_index(canonical: &str) -> Option<usize> {
    COUNT_COLS.iter().position(|c| *c == canonical)
}

pub struct ParseStats {
    pub parsed: usize,
    pub skipped: usize,
    pub ffl_matched: usize,
}

/// Extract and parse every data row from an AFMER PDF for the given report year.
///
/// The PDF's header line is first validated against the current 27-column
/// format. A non-matching header yields `Outcome::FormatMismatch` so the caller
/// can stop a walk-back and report that historic reports need a new ingester.
pub fn parse(path: &Path, year: i32, ffl: &FflStore) -> Result<Outcome> {
    let text = pdf_extract::extract_text(path.to_str().context("non-utf8 pdf path")?)
        .with_context(|| format!("extracting text from {}", path.display()))?;

    // Locate and validate the table header.
    let header_line = text.lines().find(|l| l.contains("APP_RDS_KEY"));
    match header_line {
        Some(h) if h.split_whitespace().collect::<Vec<_>>() == canonical_header() => {}
        other => {
            return Ok(Outcome::FormatMismatch {
                found: other.map(|s| s.trim().to_string()),
            })
        }
    }

    let mut rows = Vec::new();
    let mut stats = ParseStats {
        parsed: 0,
        skipped: 0,
        ffl_matched: 0,
    };

    for line in text.lines() {
        match parse_line(line, year, ffl) {
            Some(row) => {
                if row.ffl_version_first_seen.is_some() {
                    stats.ffl_matched += 1;
                }
                stats.parsed += 1;
                rows.push(row);
            }
            None => {
                if is_candidate_data_line(line) {
                    stats.skipped += 1;
                }
            }
        }
    }
    Ok(Outcome::Parsed(rows, stats))
}

/// A line that looks like it *should* be data (starts with a numeric key and a
/// 2-digit license type) but failed strict parsing — worth counting as skipped.
fn is_candidate_data_line(line: &str) -> bool {
    let mut it = line.split_whitespace();
    matches!((it.next(), it.next()), (Some(a), Some(b))
        if a.chars().all(|c| c.is_ascii_digit()) && a.len() >= 6
        && b.len() == 2 && b.chars().all(|c| c.is_ascii_digit()))
}

fn parse_line(line: &str, year: i32, ffl: &FflStore) -> Option<AfmerRow> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // key + type + (>=1 name) + (>=1 street) + state + 22 counts
    if tokens.len() < N_COUNTS + 5 {
        return None;
    }
    let rds_key = tokens[0];
    if rds_key.len() < 6 || !rds_key.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let lic_type = tokens[1];
    if lic_type.len() != 2 || !lic_type.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let state_idx = tokens.len() - N_COUNTS - 1;
    let state = tokens[state_idx];
    if state.len() != 2 || !state.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }

    let mut counts = [0i64; N_COUNTS];
    for (i, tok) in tokens[state_idx + 1..].iter().enumerate() {
        counts[i] = tok.parse().ok()?;
    }

    let blob = &tokens[2..state_idx];
    let version = ffl.version_for_year(rds_key, year);
    let (name, street, source) = split_name_street(blob, version);

    Some(AfmerRow {
        rds_key: rds_key.to_string(),
        app_lic_type: lic_type.to_string(),
        app_license_name: name,
        app_premise_street: String::new(),
        app_premise_city: street,
        app_premise_state: state.to_string(),
        counts,
        ffl_version_first_seen: version.map(|v| v.first_seen.clone()),
        name_source: source,
    })
}

fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// Split the free-text blob into (license name, premise street).
///
/// When an FFL version is known, prefer the boundary where the leading tokens
/// reconstruct the authoritative license name; otherwise where the trailing
/// tokens reconstruct the premise street. With no FFL match, fall back to
/// splitting at the first token that begins with a digit (the house number).
fn split_name_street(blob: &[&str], version: Option<&FflVersion>) -> (String, String, NameSource) {
    if let Some(v) = version {
        let want_name = normalize(&v.record.license_name);
        for k in 1..blob.len() {
            if normalize(&blob[..k].join(" ")) == want_name {
                return (blob[..k].join(" "), blob[k..].join(" "), NameSource::FflPrefix);
            }
        }
        let want_street = normalize(&v.record.premise_street);
        if !want_street.is_empty() {
            for k in 1..blob.len() {
                if normalize(&blob[k..].join(" ")) == want_street {
                    return (blob[..k].join(" "), blob[k..].join(" "), NameSource::FflSuffix);
                }
            }
        }
    }

    // Heuristic: street begins at the first token starting with a digit.
    let split = blob
        .iter()
        .position(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .filter(|&i| i >= 1)
        .unwrap_or(blob.len());
    (blob[..split].join(" "), blob[split..].join(" "), NameSource::Heuristic)
}
