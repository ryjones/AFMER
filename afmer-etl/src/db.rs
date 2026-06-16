//! SQLite schema and writers. The `afmer` table mirrors the column layout of the
//! reference AFMER-*.xlsx workbooks; `ffl_version` is the temporal license
//! dimension; views join the two and expose the version history.

use crate::ffl::FflStore;
use crate::pdf::{AfmerRow, COUNT_COLS};
use anyhow::{Context, Result};
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

        -- Manually curated links between RDS keys that belong to the same
        -- manufacturer (e.g. a licensee re-issued a new key after a region or
        -- ownership change). `group_id` is the cluster's canonical key; a key
        -- with no link is simply its own group.
        CREATE TABLE IF NOT EXISTS rds_link (
            rds_key  TEXT PRIMARY KEY,
            group_id TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_rds_link_group ON rds_link(group_id);

        -- Optional friendly name for a linked group (e.g. Sturm Ruger
        -- Aggregate), keyed by the cluster's canonical group_id.
        CREATE TABLE IF NOT EXISTS rds_group (
            group_id TEXT PRIMARY KEY,
            name     TEXT NOT NULL
        );

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

        -- AFMER rows with a stable grouping key: the linked group when one
        -- exists, else the row's own RDS key, plus the group's friendly name
        -- when one was supplied. Aggregating by `group_key` rolls up a
        -- manufacturer's full history across re-issued keys.
        CREATE VIEW IF NOT EXISTS afmer_grouped AS
            SELECT a.*,
                   COALESCE(l.group_id, a.rds_key) AS group_key,
                   g.name AS group_name
            FROM afmer a
            LEFT JOIN rds_link l ON l.rds_key = a.rds_key
            LEFT JOIN rds_group g ON g.group_id = COALESCE(l.group_id, a.rds_key);
        "
    ))?;
    Ok(())
}

/// Load manually-curated RDS-key links into the `rds_link` table (and any group
/// names into `rds_group`). The file is JSON: an array of groups (or
/// `{"groups": [...]}`), where each group is either
///
///   * a bare array of keys — `["k1", "k2"]` (canonical key = first, no name), or
///   * an object — `{"name": "Sturm Ruger Aggregate", "keys": ["k1", "k2"]}`
///     with an optional explicit `"id"` for the canonical key.
///
/// The optional `"id"` is the group's `group_id` and is treated as a standalone
/// (typically synthetic) identifier — it is **not** added to the member keys, so
/// the aggregate isn't conflated with any one real licensee row. Every member key
/// rolls up to that `group_id` in the `afmer_grouped` view, exposed alongside the
/// optional `group_name`. The file fully replaces any previously-loaded links.
/// Returns the number of member keys linked.
/// Turn JSONC into plain JSON so a hand-curated links file can carry a comment
/// per key (e.g. the full FFL row) and survive ordinary editing: strip `//` line
/// and `/* … */` block comments, then drop trailing commas. Both passes ignore
/// delimiters inside string literals and operate on bytes, so multi-byte UTF-8 in
/// values and comments is preserved. Comments are removed first so a comment
/// sitting between a comma and its closing `]`/`}` still leaves a droppable
/// trailing comma.
fn strip_jsonc(src: &str) -> String {
    drop_trailing_commas(&strip_comments(src))
}

fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

fn drop_trailing_commas(src: &str) -> String {
    let b = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == b',' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && (b[j] == b']' || b[j] == b'}') {
                i += 1; // skip the trailing comma
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

pub fn write_links(conn: &mut Connection, path: &Path) -> Result<usize> {
    let raw = std::fs::read_to_string(path)?;
    let text = strip_jsonc(&raw);
    let root: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing links file {}", path.display()))?;
    let groups = root.get("groups").unwrap_or(&root);
    let arr = groups.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "links file must be a JSON array of key-groups (or {{\"groups\": [...]}}) in {}",
            path.display()
        )
    })?;

    let str_keys = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut n = 0usize;
    let tx = conn.transaction()?;
    {
        // The links file is authoritative: start from a clean slate so removed
        // groups don't linger.
        tx.execute("DELETE FROM rds_link", [])?;
        tx.execute("DELETE FROM rds_group", [])?;

        let mut link =
            tx.prepare("INSERT OR REPLACE INTO rds_link (rds_key, group_id) VALUES (?1, ?2)")?;
        let mut group =
            tx.prepare("INSERT OR REPLACE INTO rds_group (group_id, name) VALUES (?1, ?2)")?;
        for g in arr {
            // Each group is either a bare key array or {name?, id?, keys}.
            let (keys, name, explicit_id) = if g.is_array() {
                (str_keys(g), None, None)
            } else if let Some(obj) = g.as_object() {
                let keys = obj.get("keys").map(str_keys).unwrap_or_default();
                let name = obj.get("name").and_then(|v| v.as_str()).map(str::to_string);
                let id = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                (keys, name, id)
            } else {
                anyhow::bail!("each group must be an array of RDS keys or an object with \"keys\"");
            };
            if keys.is_empty() {
                continue;
            }
            // Canonical id: an explicit (often synthetic) id, else the first key.
            let group_id = explicit_id.unwrap_or_else(|| keys[0].clone());
            for k in &keys {
                link.execute(params![k, group_id])?;
                n += 1;
            }
            if let Some(name) = name {
                group.execute(params![group_id, name])?;
            }
        }
    }
    tx.commit()?;
    Ok(n)
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

/// Read the linked RDS groups as a ready-to-embed JSON array body
/// (`{"id":…,"name":…,"keys":[…]}, …`). Unnamed groups fall back to their
/// canonical key as the name so the SPA always has a label to show.
fn groups_json_body(conn: &Connection) -> Result<String> {
    use std::collections::{BTreeMap, HashMap};
    let mut names: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT group_id, name FROM rds_group")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, n) = row?;
            names.insert(id, n);
        }
    }
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT group_id, rds_key FROM rds_link ORDER BY group_id, rds_key")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, k) = row?;
            groups.entry(id).or_default().push(k);
        }
    }
    let mut parts = Vec::new();
    for (id, keys) in &groups {
        let mut o = String::from("{\"id\": ");
        json_str(&mut o, id);
        o.push_str(", \"name\": ");
        json_str(&mut o, names.get(id).map(String::as_str).unwrap_or(id));
        o.push_str(", \"keys\": [");
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            json_str(&mut o, k);
        }
        o.push_str("]}");
        parts.push(o);
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
    meta.push_str(&format!("\n  ],\n  \"total_rows\": {total},\n  \"groups\": ["));
    meta.push_str(&groups_json_body(conn)?);
    meta.push_str("],\n  \"labels\": {");
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
