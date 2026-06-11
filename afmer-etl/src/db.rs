//! SQLite schema and writers. The `afmer` table mirrors the column layout of the
//! reference AFMER-*.xlsx workbooks; `ffl_version` is the temporal license
//! dimension; views join the two and expose the version history.

use crate::ffl::FflStore;
use crate::pdf::{AfmerRow, COUNT_COLS};
use anyhow::Result;
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::{params, Connection};
use std::io::Write;
use std::path::Path;

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    create_schema(&conn)?;
    Ok(conn)
}

fn create_schema(conn: &Connection) -> Result<()> {
    let count_cols = COUNT_COLS
        .iter()
        .map(|c| format!("  {c} INTEGER NOT NULL DEFAULT 0,"))
        .collect::<Vec<_>>()
        .join("\n");

    conn.execute_batch(&format!(
        "
        CREATE TABLE IF NOT EXISTS ffl_snapshot (
            snapshot    TEXT PRIMARY KEY,
            source_file TEXT NOT NULL,
            row_count   INTEGER NOT NULL
        );

        -- Friendly names for column/activity codes, seeded from activity-labels.json
        -- and kept in the DB for continuity. `json` holds the full label object.
        CREATE TABLE IF NOT EXISTS label (
            code TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS ffl_version (
            rds_key        TEXT NOT NULL,
            version_seq    INTEGER NOT NULL,
            first_seen     TEXT NOT NULL,
            last_seen      TEXT NOT NULL,
            lic_regn       TEXT, lic_dist TEXT, lic_cnty TEXT, lic_type TEXT,
            lic_xprdte     TEXT, lic_seqn TEXT,
            license_name   TEXT, business_name TEXT,
            premise_street TEXT, premise_city TEXT, premise_state TEXT, premise_zip TEXT,
            mail_street    TEXT, mail_city TEXT, mail_state TEXT, mail_zip TEXT,
            voice_phone    TEXT,
            PRIMARY KEY (rds_key, first_seen)
        );
        CREATE INDEX IF NOT EXISTS idx_ffl_version_key ON ffl_version(rds_key);

        -- A license may file more than one line in a year (e.g. separate 07/10
        -- licenses share an RDS key), so the key is a surrogate id and several
        -- rows per (source_year, rds_key) are allowed — matching the source.
        CREATE TABLE IF NOT EXISTS afmer (
            row_id            INTEGER PRIMARY KEY AUTOINCREMENT,
            source_year       INTEGER NOT NULL,
            source_file       TEXT NOT NULL,
            rds_key           TEXT NOT NULL,
            app_lic_type      TEXT,
            app_license_name  TEXT,
            app_premise_street TEXT,
            app_premise_city  TEXT,
            app_premise_state TEXT,
{count_cols}
            ffl_version_first_seen TEXT,
            name_source       TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_afmer_year_key ON afmer(source_year, rds_key);

        -- Latest known metadata per license.
        CREATE VIEW IF NOT EXISTS ffl_current AS
            SELECT v.*
            FROM ffl_version v
            JOIN (SELECT rds_key, MAX(version_seq) AS ms FROM ffl_version GROUP BY rds_key) m
              ON v.rds_key = m.rds_key AND v.version_seq = m.ms;

        -- Licenses whose metadata changed at least once across snapshots.
        CREATE VIEW IF NOT EXISTS ffl_changes AS
            SELECT rds_key,
                   COUNT(*)        AS versions,
                   MIN(first_seen) AS first_seen,
                   MAX(last_seen)  AS last_seen
            FROM ffl_version
            GROUP BY rds_key
            HAVING COUNT(*) > 1;

        -- AFMER rows joined to the FFL version effective for the report year.
        CREATE VIEW IF NOT EXISTS afmer_enriched AS
            SELECT a.*,
                   v.license_name   AS ffl_license_name,
                   v.business_name  AS ffl_business_name,
                   v.premise_street AS ffl_premise_street,
                   v.premise_city   AS ffl_premise_city,
                   v.premise_state  AS ffl_premise_state,
                   v.premise_zip    AS ffl_premise_zip,
                   v.lic_cnty       AS ffl_lic_cnty,
                   v.lic_xprdte     AS ffl_lic_xprdte,
                   v.voice_phone    AS ffl_voice_phone
            FROM afmer a
            LEFT JOIN ffl_version v
              ON v.rds_key = a.rds_key AND v.first_seen = a.ffl_version_first_seen;
        "
    ))?;
    Ok(())
}

pub fn write_ffl(conn: &mut Connection, store: &FflStore) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut snap = tx.prepare(
            "INSERT OR REPLACE INTO ffl_snapshot (snapshot, source_file, row_count)
             VALUES (?1, ?2, ?3)",
        )?;
        for s in &store.snapshots {
            snap.execute(params![s.label, s.source_file, s.row_count as i64])?;
        }

        let mut ver = tx.prepare(
            "INSERT OR REPLACE INTO ffl_version (
                rds_key, version_seq, first_seen, last_seen,
                lic_regn, lic_dist, lic_cnty, lic_type, lic_xprdte, lic_seqn,
                license_name, business_name,
                premise_street, premise_city, premise_state, premise_zip,
                mail_street, mail_city, mail_state, mail_zip, voice_phone
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
             )",
        )?;
        for (key, chain) in &store.versions {
            for v in chain {
                let r = &v.record;
                ver.execute(params![
                    key, v.seq, v.first_seen, v.last_seen,
                    r.lic_regn, r.lic_dist, r.lic_cnty, r.lic_type, r.lic_xprdte, r.lic_seqn,
                    r.license_name, r.business_name,
                    r.premise_street, r.premise_city, r.premise_state, r.premise_zip,
                    r.mail_street, r.mail_city, r.mail_state, r.mail_zip, r.voice_phone,
                ])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// Append a JSON-escaped string literal (with surrounding quotes) to `out`.
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Load friendly field labels from `activity-labels.json` (or a flat `{code:…}`
/// object) into the `label` table for continuity. The full label object is kept
/// per code so any extra fields the user adds survive a round-trip.
pub fn write_labels(conn: &Connection, path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(path)?;
    let root: serde_json::Value = serde_json::from_str(&text)?;
    // Accept either { "labels": {…} } or a flat { code: … } object.
    let labels = root.get("labels").unwrap_or(&root);
    let obj = labels
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("'labels' is not a JSON object in {}", path.display()))?;

    let mut stmt =
        conn.prepare("INSERT OR REPLACE INTO label (code, name, json) VALUES (?1, ?2, ?3)")?;
    let mut n = 0;
    for (code, val) in obj {
        if code.starts_with('_') {
            continue; // skip comment keys like "_comment"
        }
        let name = val
            .as_str()
            .map(str::to_string)
            .or_else(|| val.get("name").and_then(|x| x.as_str()).map(str::to_string))
            .unwrap_or_else(|| code.clone());
        stmt.execute(params![code, name, serde_json::to_string(val)?])?;
        n += 1;
    }
    Ok(n)
}

/// Read the `label` table as a ready-to-embed JSON object body (`"code": {…}, …`).
fn labels_json_body(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT code, json FROM label ORDER BY code")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut parts = Vec::new();
    for row in rows {
        let (code, json) = row?;
        let mut key = String::new();
        json_str(&mut key, &code);
        parts.push(format!("{key}: {json}"));
    }
    Ok(parts.join(", "))
}

/// Export the `afmer` table for a static site: one gzipped JSON file per year
/// (`afmer-<year>.json.gz`, an array of compact rows) plus a `meta.json` index
/// describing columns, years (with row counts), and the distinct states. The
/// per-year split keeps every file small enough for static hosting and lets the
/// SPA load only the years a query needs.
pub fn export_json(conn: &Connection, dir: &Path) -> Result<(usize, usize)> {
    std::fs::create_dir_all(dir)?;

    // Output column order. Identity fields, then the 22 counts, then provenance.
    let lead = ["year", "rds_key", "lic_type", "name", "street", "city", "state"];
    let select = "SELECT source_year, rds_key, app_lic_type, app_license_name, \
                  app_premise_street, app_premise_city, app_premise_state, "
        .to_string()
        + &COUNT_COLS.join(", ")
        + ", name_source FROM afmer WHERE source_year = ?1 ORDER BY rds_key";

    let years: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT DISTINCT source_year FROM afmer ORDER BY source_year")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let states: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT app_premise_state FROM afmer \
             WHERE app_premise_state <> '' ORDER BY app_premise_state",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let n_text = lead.len(); // string columns at the front
    let n_cols = lead.len() + COUNT_COLS.len() + 1; // + name_source
    let mut total = 0usize;
    let mut year_meta: Vec<(i64, usize)> = Vec::new();

    for &year in &years {
        let mut stmt = conn.prepare(&select)?;
        let mut json = String::from("[");
        let mut count = 0usize;
        let mut rows = stmt.query(params![year])?;
        while let Some(row) = rows.next()? {
            if count > 0 {
                json.push(',');
            }
            json.push('[');
            for i in 0..n_cols {
                if i > 0 {
                    json.push(',');
                }
                if i == 0 {
                    // year (integer)
                    json.push_str(&row.get::<_, i64>(0)?.to_string());
                } else if i < n_text {
                    json_str(&mut json, &row.get::<_, String>(i)?);
                } else if i < n_cols - 1 {
                    // a count column (integer)
                    json.push_str(&row.get::<_, i64>(i)?.to_string());
                } else {
                    json_str(&mut json, &row.get::<_, String>(i)?);
                }
            }
            json.push(']');
            count += 1;
        }
        json.push(']');

        let path = dir.join(format!("afmer-{year}.json.gz"));
        let file = std::fs::File::create(&path)?;
        let mut gz = GzEncoder::new(file, Compression::best());
        gz.write_all(json.as_bytes())?;
        gz.finish()?;
        total += count;
        year_meta.push((year, count));
    }

    // meta.json (uncompressed; small).
    let mut columns: Vec<String> = lead.iter().map(|s| s.to_string()).collect();
    columns.extend(COUNT_COLS.iter().map(|s| s.to_string()));
    columns.push("name_source".to_string());

    let mut meta = String::from("{\n  \"columns\": [");
    for (i, c) in columns.iter().enumerate() {
        if i > 0 {
            meta.push(',');
        }
        json_str(&mut meta, c);
    }
    meta.push_str("],\n  \"count_columns\": [");
    for (i, c) in COUNT_COLS.iter().enumerate() {
        if i > 0 {
            meta.push(',');
        }
        json_str(&mut meta, c);
    }
    meta.push_str("],\n  \"states\": [");
    for (i, s) in states.iter().enumerate() {
        if i > 0 {
            meta.push(',');
        }
        json_str(&mut meta, s);
    }
    meta.push_str("],\n  \"years\": [");
    for (i, (y, n)) in year_meta.iter().enumerate() {
        if i > 0 {
            meta.push(',');
        }
        meta.push_str(&format!(
            "\n    {{\"year\": {y}, \"rows\": {n}, \"file\": \"afmer-{y}.json.gz\"}}"
        ));
    }
    meta.push_str(&format!("\n  ],\n  \"total_rows\": {total},\n  \"labels\": {{"));
    meta.push_str(&labels_json_body(conn)?);
    meta.push_str("}\n}\n");
    std::fs::write(dir.join("meta.json"), meta)?;

    Ok((years.len(), total))
}

/// Whether any AFMER rows already exist for the given year.
pub fn year_present(conn: &Connection, year: i32) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM afmer WHERE source_year = ?1",
        params![year as i64],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn write_afmer(
    conn: &mut Connection,
    year: i32,
    source_file: &str,
    rows: &[AfmerRow],
) -> Result<()> {
    // 8 leading columns + counts + 2 trailing columns.
    let placeholders = (1..=COUNT_COLS.len() + 10)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO afmer (
            source_year, source_file, rds_key, app_lic_type,
            app_license_name, app_premise_street, app_premise_city, app_premise_state,
            {count_cols},
            ffl_version_first_seen, name_source
         ) VALUES ({placeholders})",
        count_cols = COUNT_COLS.join(", ")
    );

    let tx = conn.transaction()?;
    // Idempotent reload: clear any prior rows for this year first.
    tx.execute("DELETE FROM afmer WHERE source_year = ?1", params![year as i64])?;
    {
        let mut stmt = tx.prepare(&sql)?;
        for row in rows {
            let mut vals: Vec<rusqlite::types::Value> = Vec::with_capacity(COUNT_COLS.len() + 8);
            use rusqlite::types::Value;
            vals.push(Value::Integer(year as i64));
            vals.push(Value::Text(source_file.to_string()));
            vals.push(Value::Text(row.rds_key.clone()));
            vals.push(Value::Text(row.app_lic_type.clone()));
            vals.push(Value::Text(row.app_license_name.clone()));
            vals.push(Value::Text(row.app_premise_street.clone()));
            vals.push(Value::Text(row.app_premise_city.clone()));
            vals.push(Value::Text(row.app_premise_state.clone()));
            for c in row.counts {
                vals.push(Value::Integer(c));
            }
            vals.push(match &row.ffl_version_first_seen {
                Some(s) => Value::Text(s.clone()),
                None => Value::Null,
            });
            vals.push(Value::Text(row.name_source.as_str().to_string()));
            stmt.execute(rusqlite::params_from_iter(vals.iter()))?;
        }
    }
    tx.commit()?;
    Ok(())
}
