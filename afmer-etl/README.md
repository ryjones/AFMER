# afmer-etl

A Rust ETL that converts ATF **AFMER** (Annual Firearms Manufacturing and
Exportation Report) PDFs into a SQLite database whose layout mirrors the
reference `AFMER-*.xlsx` workbooks, enriched with a **temporal FFL license
dimension** built from every FFL snapshot in `FFLS/`.

## What it does

1. **Loads every FFL snapshot** (`FFLS/*.{csv,xlsx}` plus `rds-*`), keyed by the
   AFMER RDS key (`region + 2-digit district + 5-digit sequence`). Snapshots are
   folded into a time series: for each license, consecutive identical snapshots
   collapse into one *version* with a `first_seen`/`last_seen` window, so changes
   over time (address, expiration, name) are preserved.
2. **Parses AFMER PDFs.** Each data line is parsed deterministically from the
   right edge (22 integer counts + 2-letter state); the ambiguous
   license-name / premise-street boundary is resolved against the FFL version
   **effective for the report year**, so historic rows line up with how the
   license looked at the time. Falls back to a house-number heuristic when no
   FFL match exists.
3. **Writes SQLite** with the AFMER table, the FFL version history, and
   convenience views.

## Build

```sh
cargo build --release            # from afmer-etl/
```

## Usage

```sh
# Ingest a directory of reports, newest-first, stopping at the first format change:
./target/release/afmer-etl --ffl ../FFLS --pdf-dir ../SOURCE/NU --db ../afmer.db

# Or ingest explicit files:
./target/release/afmer-etl --ffl ../FFLS --pdf ../SOURCE/NU/AFMER-2023.pdf --db ../afmer.db
```

```sh
# Full build covering 2000–2024 (FFL + modern PDFs + historic workbooks + historic PDFs):
./target/release/afmer-etl --ffl ../FFLS --pdf-dir ../SOURCE/NU \
  --xlsx ../AFMER-20[012]*.xlsx --hist-pdf ../SOURCE/base/*.pdf --db ../afmer.db
```

| Flag | Meaning |
|------|---------|
| `--ffl <paths...>` | FFL files or directories (CSV/XLSX). A directory is scanned for all snapshots. |
| `--pdf <files...>` | Explicit modern-format AFMER PDFs (year parsed from filename). |
| `--pdf-dir <dir>` | Walk-back mode: discover `AFMER-YYYY.pdf`, ingest newest→oldest. |
| `--from <year>` | Start year for walk-back (default: newest found). |
| `--xlsx <files...>` | Structured AFMER workbooks (10-sheet historic or flat); auto-detected. |
| `--hist-pdf <files...>` | Historic per-manufacturer "RDS Key" report PDFs (2013–2019, 2024). |
| `--export-json <dir>` | Export per-year gzipped JSON + `meta.json` for the SPA (usable alone). |
| `--labels <file>` | Friendly field-label JSON (default `./activity-labels.json` if present). |
| `--links <file>` | JSON groups of RDS keys that are the same manufacturer (usable alone). |
| `--no-ingest` | Skip ingest; only post-process the existing DB (labels/links/export). |
| `--db <path>` | Output SQLite file (default `afmer.db`). |

Across `--xlsx` and `--hist-pdf`, a year already loaded in the DB is left untouched, so a validated source is never clobbered by a thinner one. Order of precedence in a single run: `--pdf` / `--pdf-dir`, then `--xlsx`, then `--hist-pdf`.

## Formats & coverage

The tool ingests every AFMER layout the data set ships, mapping all count columns
by name onto a single canonical set of 22. Current coverage is **2000–2024**
(25 years); only 1998–1999 are excluded (aggregate-only prose, no per-licensee
table).

| Year(s) | Source / layout | Ingester |
|---------|-----------------|----------|
| 2022–2023 | modern 27-col flat PDF (matches `AFMER-2023.xlsx`) | `pdf` (walk-back) |
| 2000–2012 | 10-sheet historic workbooks | `xlsx` |
| 2020–2021 | flat single-sheet workbooks (`APP_PREMISE_STREET` variants) | `xlsx` |
| 2013–2019, 2024 | paginated per-manufacturer "RDS Key" PDF tables | `hist-pdf` |
| 1998–1999 | aggregate-only prose | not row-level |

The modern PDF walk-back validates each header and stops at the first format
change (2021 differs with an extra `APP_PREMISE_STREET` column; 2020/≤2019/2024
differ more), which is why those years are loaded from the workbooks and historic
PDFs instead. The historic-PDF ingester reads each section's column header to
handle per-year ordering differences (e.g. the revolver caliber order differs
between 2018 and 2024).

## Schema

- **`afmer`** — one row per report line (surrogate `row_id`; duplicate RDS keys
  per year are preserved, matching the source). Columns mirror the XLSX: the 5
  lead columns + 22 integer counts, plus `source_year`, `source_file`,
  `ffl_version_first_seen` (which FFL version enriched it) and `name_source`
  (`ffl-prefix` / `ffl-suffix` / `heuristic`).
- **`ffl_version`** — temporal license dimension: `(rds_key, version_seq,
  first_seen, last_seen, …license fields…)`.
- **`ffl_snapshot`** — one row per ingested snapshot file.
- **`label`** — friendly names for column/activity codes (`code, name, json`),
  seeded from `activity-labels.json` and kept in the DB for continuity.
- **`rds_link`** — manually-curated links between RDS keys belonging to the same
  manufacturer (`rds_key, group_id`); loaded by `--links`.
- **`rds_group`** — optional friendly name per linked group (`group_id, name`).
- **Views:** `ffl_current` (latest version per license), `ffl_changes` (licenses
  with >1 version), `afmer_enriched` (AFMER joined to its effective FFL version),
  `afmer_grouped` (AFMER with a `group_key` that collapses linked keys plus the
  group's `group_name`, so a manufacturer's history rolls up across re-issued keys).

### Linking RDS keys

Over time a manufacturer files under several RDS keys — separate plants and a
re-issued key after a region/ownership change (e.g. Sturm Ruger files under ~9
keys across NH, CT, NC, NY and AZ). `--links` takes a JSON file of key-groups,
records them in `rds_link`/`rds_group`, and the `afmer_grouped` view then
aggregates a licensee's full history under one `group_key`/`group_name`.

A group is either a **bare array** of keys (canonical key = first, no name) or an
**object** with an optional `name` and an optional explicit `id`. The `id` is a
standalone (typically **synthetic**) canonical key — it is *not* added to the
member keys, so the aggregate isn't conflated with any one real licensee row. A
handy convention is region 9 / district 99 + a sequence (`99900001`, …). See
[`links.example.json`](links.example.json), generated from a plain key list:

```jsonc
{ "groups": [
  { "id": "99900001", "name": "Sturm Ruger Aggregate",
    "keys": ["98614472", "60200735", "60600763", "15609063", "..."] },
  ["99201609", "57401497"]            // unnamed group; first key is canonical
] }
// A bare top-level array (no "groups" wrapper) is also accepted.
// The file is authoritative — loading it replaces all previously-loaded links.
```

```sh
# Link + re-export from the existing DB without re-running ingest:
./target/release/afmer-etl --no-ingest --db ../afmer.db \
  --links ../links.json --export-json ../AFMER-SPA/data
# then, e.g.:  SELECT group_key, group_name, SUM(rifle_mfg)
#              FROM afmer_grouped GROUP BY group_key;
```

`--export-json` writes the groups (`id`, `name`, `keys`) into `meta.json` so the
SPA can offer them in its **Aggregate group** dropdown.

## Labels & JSON export

`activity-labels.json` (repo root) maps each column/activity code to a friendly
name (and optional `group`/`activity`/`caliber`). It is the editable source:
`--labels` loads it into the `label` table, and `--export-json` writes it into
the SPA's `data/meta.json` under `labels`, so the SPA shows friendly names.

`--export-json <dir>` writes `meta.json` (columns, count columns, states, the
per-year file list, and labels) plus one `afmer-YYYY.json.gz` per year (a compact
array of rows). It can run alone against an existing DB:

```sh
./target/release/afmer-etl --db ../afmer.db --labels ../activity-labels.json \
  --export-json ../AFMER-SPA/data
```

## Validation

`validate.py` (run via the repo `.venv`) compares the database against the
reference workbooks:

```sh
.venv/bin/python afmer-etl/validate.py afmer.db SOURCE/NU
```

Results:
- **2022 & 2023** (modern PDF): raw row counts, the 22 counts, license type and
  state match the XLSX **100%**; name/street agree ~99.5%/99%.
- **2000–2012** (historic XLSX): the 10-sheet→unified join matches the
  independent `csv/` dump exactly (e.g. 2012: 0 mismatches, 1,496/1,496 keys).
- **2013–2019, 2024** (historic PDF): validated by internal consistency —
  per-caliber sums equal the separately-parsed Total columns for ~every row,
  across both revolver column orderings. The lone exception (1 row in 2024) is a
  data error in the ATF report itself (Axon: `.25=77023` but printed `Total=0`),
  which the parser faithfully reproduces.
