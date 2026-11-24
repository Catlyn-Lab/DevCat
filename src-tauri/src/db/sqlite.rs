//! SQLite driver built on sqlx.
//!
//! A single connection is shared by everything (behind a mutex) so `ATTACH`,
//! temp tables and explicit transactions behave like one sqlite3 shell session.
//! Values are converted by their runtime storage class (SQLite is dynamically
//! typed). Long-running statements can be interrupted through a progress handler.
use super::sql_common::{self as sc};
use super::*;
use crate::error::{AppError, AppResult};
use crate::models::{ConnectionConfig, ConnectionKind};
use async_trait::async_trait;
use futures::TryStreamExt;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteRow};
use sqlx::{AssertSqlSafe, Column, ConnectOptions, Connection, Either, Executor, Row, SqlSafeStr, Statement, TypeInfo, ValueRef};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, MutexGuard};

const D: Dialect = Dialect::Sqlite;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_ROWS: usize = 1000;
const JS_MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

pub struct SqliteDriver {
    conn: Mutex<Option<SqliteConnection>>,
    read_only: bool,
    /// Set by `cancel`; the progress handler interrupts the running statement.
    interrupt: Arc<AtomicBool>,
}

fn q(s: &str) -> String {
    sc::quote_ident(D, s)
}

impl SqliteDriver {
    pub async fn connect(cfg: &ConnectionConfig) -> AppResult<Self> {
        let create = cfg.options.get("create").is_some_and(|v| v == "true");
        let opts = match cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) => SqliteConnectOptions::from_str(u)?,
            None => {
                let path = cfg
                    .file_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| AppError::msg("No SQLite database file selected"))?;
                if path == ":memory:" {
                    SqliteConnectOptions::from_str("sqlite::memory:")?
                } else {
                    SqliteConnectOptions::new().filename(path)
                }
            }
        };
        let opts = opts
            .create_if_missing(create && !cfg.read_only)
            .read_only(cfg.read_only)
            .busy_timeout(Duration::from_secs(5))
            .statement_cache_capacity(32);
        let mut conn = tokio::time::timeout(CONNECT_TIMEOUT, opts.connect())
            .await
            .map_err(|_| AppError::msg("Timed out opening the SQLite database"))??;
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        conn.lock_handle().await?.set_progress_handler(1000, move || !flag.load(Ordering::Relaxed));
        // Opening is lazy in SQLite; make sure the file is really a database.
        sqlx::raw_sql("SELECT count(*) FROM sqlite_master").execute(&mut conn).await?;
        Ok(SqliteDriver { conn: Mutex::new(Some(conn)), read_only: cfg.read_only, interrupt })
    }

    /// Lock the connection for one operation (and clear a stale cancel request).
    async fn lock(&self) -> AppResult<MutexGuard<'_, Option<SqliteConnection>>> {
        let g = self.conn.lock().await;
        if g.is_none() {
            return Err(AppError::NotConnected("SQLite database is closed".into()));
        }
        self.interrupt.store(false, Ordering::SeqCst);
        Ok(g)
    }

    fn db_of<'a>(&self, t: Option<&'a str>) -> &'a str {
        db_of(t)
    }

    /// Run one statement, collecting at most `max_rows` rows.
    async fn run_statement(conn: &mut SqliteConnection, sql: &str, max_rows: usize) -> Result<QueryResult, sqlx::Error> {
        let started = Instant::now();
        let mut columns: Option<Vec<ColumnInfo>> = None;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut truncated = false;
        let mut affected = 0u64;
        {
            let mut stream = sqlx::raw_sql(AssertSqlSafe(sql.to_string())).fetch_many(&mut *conn);
            while let Some(item) = stream.try_next().await? {
                match item {
                    Either::Left(done) => affected += done.rows_affected(),
                    Either::Right(row) => {
                        if columns.is_none() {
                            columns = Some(
                                row.columns()
                                    .iter()
                                    .map(|c| ColumnInfo { name: c.name().to_string(), type_name: c.type_info().name().to_string() })
                                    .collect(),
                            );
                        }
                        if rows.len() < max_rows {
                            rows.push(convert_row(&row));
                        } else {
                            truncated = true;
                            // Stop stepping a side-effect free query (dropping the stream
                            // resets the statement); anything else runs to completion.
                            if sc::can_stop_early(D, sql) {
                                break;
                            }
                        }
                    }
                }
            }
        }
        if columns.is_none() && sc::may_return_rows(D, sql) {
            if let Ok(stmt) = (&mut *conn).prepare(AssertSqlSafe(sql.to_string()).into_sql_str()).await {
                if !stmt.columns().is_empty() {
                    columns = Some(
                        stmt.columns()
                            .iter()
                            .map(|c| ColumnInfo { name: c.name().to_string(), type_name: c.type_info().name().to_string() })
                            .collect(),
                    );
                }
            }
        }
        let has_result_set = columns.is_some();
        Ok(QueryResult {
            statement: sql.to_string(),
            columns: columns.unwrap_or_default(),
            rows,
            rows_affected: if has_result_set { None } else { Some(affected) },
            elapsed_ms: started.elapsed().as_millis() as u64,
            truncated,
            message: None,
        })
    }
}

// -------------------------------------------------------------------------------------------------
// The catalog, over any SQLite session
//
// Reading the catalog (tree, structure, DDL, schema snapshot) is the same SQL whether the database
// is a local file (sqlx) or a file on an SSH host (its `sqlite3`, `sqlite_ssh.rs`); only how a query
// with parameters runs differs. So it is written once, against [`Rows`].

/// A SQLite session that runs a query with positional parameters (`?1`, `?2`, … or `?`) and
/// returns its rows as values: integers and reals as numbers, text as strings, NULL as null.
#[async_trait]
pub(super) trait Rows: Send {
    async fn rows(&mut self, sql: &str, params: &[Value]) -> AppResult<Vec<Vec<Value>>>;
}

#[async_trait]
impl Rows for SqliteConnection {
    async fn rows(&mut self, sql: &str, params: &[Value]) -> AppResult<Vec<Vec<Value>>> {
        let mut qy = sqlx::query(AssertSqlSafe(sql.to_string()));
        for p in params {
            qy = match p {
                Value::Null => qy.bind(None::<String>),
                Value::Number(n) => match n.as_i64() {
                    Some(i) => qy.bind(i),
                    None => qy.bind(n.as_f64().unwrap_or_default()),
                },
                Value::String(s) => qy.bind(s.clone()),
                other => qy.bind(other.to_string()),
            };
        }
        Ok(qy.fetch_all(&mut *self).await?.iter().map(convert_row).collect())
    }
}

/// `main` unless a database is named.
pub(super) fn db_of(t: Option<&str>) -> &str {
    t.map(str::trim).filter(|d| !d.is_empty()).unwrap_or("main")
}

fn txt(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn opt_txt(v: &Value) -> Option<String> {
    (!v.is_null()).then(|| txt(v))
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or_default() as i64),
        Value::String(s) => s.trim().parse().unwrap_or_default(),
        Value::Bool(b) => i64::from(*b),
        _ => 0,
    }
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn opt(v: Option<&str>) -> Value {
    v.map(s).unwrap_or(Value::Null)
}

pub(super) async fn server_version(c: &mut (impl Rows + ?Sized)) -> AppResult<String> {
    let v = c.rows("SELECT sqlite_version()", &[]).await?;
    Ok(v.first().and_then(|r| r.first()).map(txt).unwrap_or_default())
}

pub(super) async fn list_databases(c: &mut (impl Rows + ?Sized)) -> AppResult<Vec<String>> {
    Ok(c.rows("SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq", &[]).await?.iter().map(|r| txt(&r[0])).collect())
}

/// The primary key's columns, in key order.
pub(super) async fn primary_key(c: &mut (impl Rows + ?Sized), db: &str, table: &str) -> AppResult<Vec<String>> {
    Ok(c.rows("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk", &[s(table), s(db)])
        .await?
        .iter()
        .map(|r| txt(&r[0]))
        .collect())
}

/// Declared type of each column, upper-cased.
pub(super) async fn column_types(c: &mut (impl Rows + ?Sized), db: &str, table: &str) -> AppResult<HashMap<String, String>> {
    Ok(c.rows("SELECT name, upper(type) FROM pragma_table_xinfo(?1, ?2)", &[s(table), s(db)])
        .await?
        .iter()
        .map(|r| (txt(&r[0]), txt(&r[1])))
        .collect())
}

pub(super) async fn structures(c: &mut (impl Rows + ?Sized), db: &str, table: Option<&str>) -> AppResult<Vec<TableStructure>> {
    let master = format!("{}.sqlite_master", q(db));
    let tables = c
        .rows(
            &format!(
                "SELECT name, sql FROM {master} WHERE (?1 IS NULL AND type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\') \
                 OR (name = ?1 AND type IN ('table', 'view')) ORDER BY name"
            ),
            &[opt(table)],
        )
        .await?;
    let mut out: Vec<TableStructure> = Vec::with_capacity(tables.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut autoinc: HashMap<String, bool> = HashMap::new();
    for r in tables {
        let name = txt(&r[0]);
        index.insert(name.clone(), out.len());
        autoinc.insert(name.clone(), txt(&r[1]).to_ascii_uppercase().contains("AUTOINCREMENT"));
        out.push(TableStructure { table: Some(TableRef { database: Some(db.to_string()), schema: None, name }), ..Default::default() });
    }
    if out.is_empty() {
        return Ok(out);
    }
    let filter = "(?2 IS NULL AND m.type = 'table' AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\') OR (m.name = ?2 AND m.type IN ('table', 'view'))";
    let cols = c
        .rows(
            &format!(
                "SELECT m.name, p.name, p.type, p.\"notnull\", p.dflt_value, p.pk, p.hidden \
                 FROM {master} m JOIN pragma_table_xinfo(m.name, ?1) p WHERE {filter} ORDER BY m.name, p.cid"
            ),
            &[s(db), opt(table)],
        )
        .await?;
    let mut pk_count: HashMap<String, usize> = HashMap::new();
    for c in cols.iter().filter(|c| int(&c[5]) > 0) {
        *pk_count.entry(txt(&c[0])).or_default() += 1;
    }
    for r in cols {
        let (t, name, ty, notnull, default, pk, hidden) = (txt(&r[0]), txt(&r[1]), txt(&r[2]), int(&r[3]), opt_txt(&r[4]), int(&r[5]), int(&r[6]));
        let Some(&ix) = index.get(&t) else { continue };
        // INTEGER PRIMARY KEY is an alias of the rowid and auto-assigned.
        let rowid_alias = pk > 0 && pk_count.get(&t) == Some(&1) && ty.eq_ignore_ascii_case("INTEGER");
        let ts = &mut out[ix];
        let default = match hidden {
            2 | 3 => Some(format!("GENERATED ALWAYS AS (...) {}", if hidden == 3 { "STORED" } else { "VIRTUAL" })),
            _ => default,
        };
        ts.columns.push(ColumnDef {
            name,
            data_type: ty,
            nullable: notnull == 0 && pk == 0,
            default,
            is_primary_key: pk > 0,
            auto_increment: rowid_alias || (pk > 0 && autoinc.get(&t) == Some(&true)),
            comment: None,
            ordinal: ts.columns.len() as u32 + 1,
        });
    }
    let idx = c
        .rows(
            &format!(
                "SELECT m.name, il.name, il.\"unique\", il.origin, ii.name \
                 FROM {master} m JOIN pragma_index_list(m.name, ?1) il JOIN pragma_index_xinfo(il.name, ?1) ii \
                 WHERE ({filter}) AND ii.key = 1 ORDER BY m.name, il.name, ii.seqno"
            ),
            &[s(db), opt(table)],
        )
        .await?;
    for r in idx {
        let (t, name, unique, origin, col) = (txt(&r[0]), txt(&r[1]), int(&r[2]), txt(&r[3]), opt_txt(&r[4]));
        let Some(&ix) = index.get(&t) else { continue };
        let ts = &mut out[ix];
        let col = col.unwrap_or_else(|| "(expression)".into());
        match ts.indexes.iter_mut().find(|i| i.name == name) {
            Some(i) => i.columns.push(col),
            None => ts.indexes.push(IndexDef { name, columns: vec![col], unique: unique != 0, primary: origin == "pk", method: None }),
        }
    }
    let fks = c
        .rows(
            &format!(
                "SELECT m.name, fk.id, fk.\"table\", fk.\"from\", fk.\"to\", fk.on_update, fk.on_delete \
                 FROM {master} m JOIN pragma_foreign_key_list(m.name, ?1) fk WHERE {filter} ORDER BY m.name, fk.id, fk.seq"
            ),
            &[s(db), opt(table)],
        )
        .await?;
    for r in fks {
        let (t, id, ref_table, from, to, on_update, on_delete) = (txt(&r[0]), int(&r[1]), txt(&r[2]), txt(&r[3]), opt_txt(&r[4]), txt(&r[5]), txt(&r[6]));
        let Some(&ix) = index.get(&t) else { continue };
        let name = format!("fk_{t}_{id}");
        let to = match to {
            Some(c) => c,
            // FOREIGN KEY ... REFERENCES t (no columns) targets the primary key.
            None => primary_key(c, db, &ref_table)
                .await?
                .into_iter()
                .nth(out[ix].foreign_keys.iter().find(|f| f.name == name).map(|f| f.columns.len()).unwrap_or(0))
                .unwrap_or_default(),
        };
        let ts = &mut out[ix];
        match ts.foreign_keys.iter_mut().find(|f| f.name == name) {
            Some(f) => {
                f.columns.push(from);
                f.ref_columns.push(to);
            }
            None => ts.foreign_keys.push(ForeignKeyDef {
                name,
                columns: vec![from],
                ref_schema: None,
                ref_table,
                ref_columns: vec![to],
                on_update: Some(on_update),
                on_delete: Some(on_delete),
            }),
        }
    }
    Ok(out)
}

pub(super) async fn table_structure(c: &mut (impl Rows + ?Sized), table: &TableRef) -> AppResult<TableStructure> {
    let db = db_of(table.database.as_deref().or(table.schema.as_deref())).to_string();
    structures(c, &db, Some(&table.name))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::msg(format!("Table \"{}\" not found", table.name)))
}

pub(super) async fn children(c: &mut (impl Rows + ?Sized), path: &[String]) -> AppResult<Vec<TreeNode>> {
    match path {
        [] => {
            let dbs = c.rows("SELECT seq, name, file FROM pragma_database_list WHERE name <> 'temp'", &[]).await?;
            Ok(dbs
                .into_iter()
                .map(|r| {
                    let (name, file) = (txt(&r[1]), txt(&r[2]));
                    TreeNode {
                        path: vec![name.clone()],
                        label: name,
                        kind: NodeKind::Database,
                        has_children: true,
                        detail: (!file.is_empty()).then_some(file),
                        table: None,
                    }
                })
                .collect())
        }
        [_] => Ok(vec![
            folder(path, "tables", "Tables"),
            folder(path, "views", "Views"),
            folder(path, "indexes", "Indexes"),
            folder(path, "triggers", "Triggers"),
        ]),
        [db, f] => {
            let ty = match f.as_str() {
                "tables" => "table",
                "views" => "view",
                "indexes" => "index",
                "triggers" => "trigger",
                other => return Err(AppError::msg(format!("Unknown folder \"{other}\""))),
            };
            let rows = c
                .rows(
                    &format!(
                        "SELECT name, tbl_name FROM {}.sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
                        q(db)
                    ),
                    &[s(ty)],
                )
                .await?;
            Ok(rows
                .into_iter()
                .map(|r| {
                    let (name, tbl) = (txt(&r[0]), txt(&r[1]));
                    match ty {
                        "table" => TreeNode {
                            path: [path, &[name.clone()]].concat(),
                            label: name.clone(),
                            kind: NodeKind::Table,
                            has_children: true,
                            detail: None,
                            table: Some(tref(db, &name)),
                        },
                        "view" => leaf(path, &name, NodeKind::View, None, Some(tref(db, &name))),
                        "index" => leaf(path, &name, NodeKind::Index, Some(tbl), Some(tref(db, &name))),
                        _ => leaf(path, &name, NodeKind::Trigger, Some(tbl), Some(tref(db, &name))),
                    }
                })
                .collect())
        }
        [_, f, _] if f == "tables" => Ok(vec![
            folder(path, "columns", "Columns"),
            folder(path, "indexes", "Indexes"),
            folder(path, "foreign_keys", "Foreign Keys"),
            folder(path, "triggers", "Triggers"),
        ]),
        [db, f, table, sub] if f == "tables" => {
            if sub == "triggers" {
                let names = c
                    .rows(&format!("SELECT name FROM {}.sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name", q(db)), &[s(table)])
                    .await?;
                return Ok(names.iter().map(|r| txt(&r[0])).map(|n| leaf(path, &n, NodeKind::Trigger, None, Some(tref(db, &n)))).collect());
            }
            let st = structures(c, db, Some(table)).await?.into_iter().next().unwrap_or_default();
            Ok(match sub.as_str() {
                "columns" => st
                    .columns
                    .iter()
                    .map(|c| {
                        let mut d = c.data_type.clone();
                        if c.is_primary_key {
                            d.push_str(" PK");
                        }
                        if !c.nullable && !c.is_primary_key {
                            d.push_str(" NOT NULL");
                        }
                        leaf(path, &c.name, NodeKind::Column, Some(d.trim().to_string()), None)
                    })
                    .collect(),
                "indexes" => st
                    .indexes
                    .iter()
                    .map(|i| {
                        let d = format!(
                            "{}({})",
                            if i.primary { "PRIMARY " } else if i.unique { "UNIQUE " } else { "" },
                            i.columns.join(", ")
                        );
                        leaf(path, &i.name, NodeKind::Index, Some(d), Some(tref(db, &i.name)))
                    })
                    .collect(),
                "foreign_keys" => st
                    .foreign_keys
                    .iter()
                    .map(|f| {
                        let d = format!("({}) → {}({})", f.columns.join(", "), f.ref_table, f.ref_columns.join(", "));
                        leaf(path, &f.name, NodeKind::ForeignKey, Some(d), None)
                    })
                    .collect(),
                other => return Err(AppError::msg(format!("Unknown folder \"{other}\""))),
            })
        }
        _ => Ok(vec![]),
    }
}

pub(super) async fn object_ddl(c: &mut (impl Rows + ?Sized), table: &TableRef, kind: &NodeKind) -> AppResult<String> {
    let db = db_of(table.database.as_deref().or(table.schema.as_deref())).to_string();
    let ty = match kind {
        NodeKind::Table => "table",
        NodeKind::View => "view",
        NodeKind::Index => "index",
        NodeKind::Trigger => "trigger",
        other => return Err(AppError::Unsupported(format!("DDL for {other:?}"))),
    };
    let master = format!("{}.sqlite_master", q(&db));
    let sql = c
        .rows(&format!("SELECT sql FROM {master} WHERE type = ?1 AND name = ?2"), &[s(ty), s(&table.name)])
        .await?
        .first()
        .and_then(|r| opt_txt(&r[0]));
    let sql = sql.ok_or_else(|| AppError::msg(format!("No definition found for {ty} \"{}\" (auto-created objects have none)", table.name)))?;
    let mut ddl = format!("{};\n", sql.trim_end().trim_end_matches(';'));
    if ty == "table" {
        let extra = c
            .rows(
                &format!(
                    "SELECT sql FROM {master} WHERE tbl_name = ?1 AND type IN ('index', 'trigger') AND sql IS NOT NULL \
                     ORDER BY type = 'trigger', name"
                ),
                &[s(&table.name)],
            )
            .await?;
        for e in extra {
            ddl.push_str(&format!("\n{};\n", txt(&e[0]).trim_end().trim_end_matches(';')));
        }
    }
    Ok(ddl)
}

pub(super) async fn schema_snapshot(c: &mut (impl Rows + ?Sized), database: Option<&str>, schema: Option<&str>) -> AppResult<SchemaSnapshot> {
    let db = db_of(database.or(schema)).to_string();
    let tables = structures(c, &db, None).await?;
    let views = c.rows(&format!("SELECT name, sql FROM {}.sqlite_master WHERE type = 'view' ORDER BY name", q(&db)), &[]).await?;
    Ok(SchemaSnapshot { tables, views: views.into_iter().map(|r| (tref(&db, &txt(&r[0])), txt(&r[1]))).collect() })
}

fn convert_row(row: &SqliteRow) -> Vec<Value> {
    (0..row.len())
        .map(|i| {
            let Ok(v) = row.try_get_raw(i) else { return Value::Null };
            if v.is_null() {
                return Value::Null;
            }
            let storage = v.type_info().name().to_string();
            match storage.as_str() {
                "INTEGER" => match row.try_get_unchecked::<i64, _>(i) {
                    Ok(n) if n.abs() <= JS_MAX_SAFE_INT => Value::from(n),
                    Ok(n) => Value::String(n.to_string()),
                    Err(_) => Value::Null,
                },
                "REAL" => match row.try_get_unchecked::<f64, _>(i) {
                    Ok(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or_else(|| Value::String(f.to_string())),
                    Err(_) => Value::Null,
                },
                "BLOB" => row.try_get_unchecked::<Vec<u8>, _>(i).map(|b| sc::binary_to_value(&b)).unwrap_or(Value::Null),
                _ => match row.try_get_unchecked::<String, _>(i) {
                    Ok(s) => Value::String(s),
                    Err(_) => row.try_get_unchecked::<Vec<u8>, _>(i).map(|b| sc::binary_to_value(&b)).unwrap_or(Value::Null),
                },
            }
        })
        .collect()
}

fn sqlite_error_text(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) if db.message().contains("interrupted") => "Query cancelled (interrupted)".into(),
        sqlx::Error::Database(db) => format!("{}", db.message()),
        other => other.to_string(),
    }
}

fn folder(base: &[String], id: &str, label: &str) -> TreeNode {
    let mut path = base.to_vec();
    path.push(id.into());
    TreeNode { path, label: label.into(), kind: NodeKind::Folder, has_children: true, detail: None, table: None }
}

fn leaf(base: &[String], name: &str, kind: NodeKind, detail: Option<String>, table: Option<TableRef>) -> TreeNode {
    let mut path = base.to_vec();
    path.push(name.into());
    TreeNode { path, label: name.into(), kind, has_children: false, detail, table }
}

pub(super) fn tref(db: &str, name: &str) -> TableRef {
    TableRef { database: Some(db.into()), schema: None, name: name.into() }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn kind(&self) -> ConnectionKind {
        ConnectionKind::Sqlite
    }
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }

    async fn server_version(&self) -> AppResult<String> {
        let mut g = self.lock().await?;
        let conn = g.as_mut().expect("checked");
        Ok(format!("SQLite {}", server_version(conn).await?))
    }

    async fn children(&self, path: &[String]) -> AppResult<Vec<TreeNode>> {
        let mut g = self.lock().await?;
        children(g.as_mut().expect("checked"), path).await
    }

    async fn list_databases(&self) -> AppResult<Vec<String>> {
        let mut g = self.lock().await?;
        list_databases(g.as_mut().expect("checked")).await
    }

    async fn execute(&self, script: &str, opts: &ExecOptions) -> AppResult<Vec<QueryResult>> {
        let stmts = sc::split_script(D, script);
        if stmts.is_empty() {
            return Ok(vec![]);
        }
        if self.read_only {
            if let Some(i) = stmts.iter().position(|s| !sc::is_read_only_statement(D, &s.sql)) {
                return Err(sc::read_only_error(i + 1));
            }
        }
        let max_rows = opts.max_rows.filter(|m| *m > 0).unwrap_or(DEFAULT_MAX_ROWS);
        let mut g = self.lock().await?;
        let conn = g.as_mut().expect("checked");
        let mut out = Vec::with_capacity(stmts.len());
        for (i, st) in stmts.iter().enumerate() {
            if self.interrupt.load(Ordering::SeqCst) {
                return Err(sc::statement_error(i + 1, stmts.len(), st.line(script), &st.sql, "Query cancelled"));
            }
            match Self::run_statement(conn, &st.sql, max_rows).await {
                Ok(r) => out.push(r),
                Err(e) => return Err(sc::statement_error(i + 1, stmts.len(), st.line(script), &st.sql, sqlite_error_text(&e))),
            }
        }
        Ok(out)
    }

    async fn table_data(&self, req: &TableDataRequest) -> AppResult<TableDataPage> {
        let db = self.db_of(req.table.database.as_deref().or(req.table.schema.as_deref())).to_string();
        let table = tref(&db, &req.table.name);
        let filter = req.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        if let Some(f) = filter {
            if sc::split_statements(D, f).len() > 1 {
                return Err(AppError::msg("The filter must be a single SQL expression"));
            }
        }
        let mut g = self.lock().await?;
        let conn = g.as_mut().expect("checked");
        let pk = primary_key(conn, &db, &req.table.name).await?;
        let sql = sc::select_page_sql(D, &table, filter, &req.sort, &pk, req.limit.max(1), req.offset);
        let result = Self::run_statement(conn, &sql, usize::MAX).await.map_err(|e| AppError::msg(sqlite_error_text(&e)))?;
        let total: i64 = sqlx::query_scalar(AssertSqlSafe(sc::count_sql(D, &table, filter))).fetch_one(&mut *conn).await?;
        Ok(TableDataPage { result, total: Some(total.max(0) as u64), primary_key: pk })
    }

    async fn table_structure(&self, table: &TableRef) -> AppResult<TableStructure> {
        let mut g = self.lock().await?;
        table_structure(g.as_mut().expect("checked"), table).await
    }

    async fn apply_changes(&self, table: &TableRef, changes: &[RowChange]) -> AppResult<u64> {
        if self.read_only {
            return Err(AppError::msg("This connection is read-only"));
        }
        if changes.is_empty() {
            return Ok(0);
        }
        let db = self.db_of(table.database.as_deref().or(table.schema.as_deref())).to_string();
        let t = tref(&db, &table.name);
        let mut g = self.lock().await?;
        let conn = g.as_mut().expect("checked");
        let types = column_types(conn, &db, &table.name).await?;
        let mut tx = conn.begin().await?;
        let mut total = 0u64;
        for (n, change) in changes.iter().enumerate() {
            let stmt = sc::build_change(D, &t, change, &|_, p| p)?;
            let mut qy = sqlx::query(AssertSqlSafe(stmt.sql.clone()));
            for (v, col) in stmt.params.iter().zip(&stmt.param_columns) {
                let blob = types.get(col).is_some_and(|t| t.contains("BLOB"));
                qy = match v {
                    Value::Null => qy.bind(None::<String>),
                    Value::Bool(b) => qy.bind(i64::from(*b)),
                    Value::Number(num) => match num.as_i64() {
                        Some(i) => qy.bind(i),
                        None => qy.bind(num.as_f64().unwrap_or_default()),
                    },
                    Value::String(s) if blob => match sc::parse_hex_binary(s) {
                        Some(bytes) => qy.bind(bytes),
                        None => qy.bind(s.clone()),
                    },
                    Value::String(s) => qy.bind(s.clone()),
                    other => qy.bind(other.to_string()),
                };
            }
            let affected = match qy.execute(&mut *tx).await {
                Ok(r) => r.rows_affected(),
                Err(e) => {
                    let _ = tx.rollback().await;
                    return Err(AppError::msg(format!("Change {}: {}", n + 1, sqlite_error_text(&e))));
                }
            };
            if stmt.expect_one && affected != 1 {
                let _ = tx.rollback().await;
                return Err(sc::row_count_error(n + 1, &stmt.sql, affected));
            }
            total += affected;
        }
        tx.commit().await?;
        Ok(total)
    }

    async fn object_ddl(&self, table: &TableRef, kind: &NodeKind) -> AppResult<String> {
        let mut g = self.lock().await?;
        object_ddl(g.as_mut().expect("checked"), table, kind).await
    }

    async fn schema_snapshot(&self, database: Option<&str>, schema: Option<&str>) -> AppResult<SchemaSnapshot> {
        let mut g = self.lock().await?;
        schema_snapshot(g.as_mut().expect("checked"), database, schema).await
    }

    async fn cancel(&self) -> AppResult<()> {
        self.interrupt.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn close(&self) {
        // Wake up a running statement so the lock can be taken.
        self.interrupt.store(true, Ordering::SeqCst);
        if let Some(conn) = self.conn.lock().await.take() {
            let _ = conn.close().await;
        }
    }
}

/// Integration tests against a temporary SQLite file.
/// Run with `cargo test --lib -- --ignored sqlite::integration`.
#[cfg(test)]
mod integration {
    use super::*;
    use serde_json::{json, Map};

    fn cfg(path: &str, create: bool, read_only: bool) -> ConnectionConfig {
        ConnectionConfig {
            id: "sqlite-it".into(),
            name: "sqlite".into(),
            kind: ConnectionKind::Sqlite,
            host: String::new(),
            port: None,
            user: String::new(),
            password: None,
            database: None,
            file_path: Some(path.into()),
            url: None,
            ssl_mode: None,
            ssh: None,
            color: None,
            group: None,
            project_id: None,
            options: if create { [("create".to_string(), "true".to_string())].into_iter().collect() } else { Default::default() },
            read_only,
            mcp_access: None,
        }
    }

    fn opts() -> ExecOptions {
        ExecOptions { database: None, schema: None, max_rows: None }
    }

    const SETUP: &str = r#"
        CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, born DATE);
        CREATE TABLE books (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            author_id INTEGER REFERENCES authors ON DELETE CASCADE,
            title VARCHAR(200) NOT NULL DEFAULT 'untitled',
            price NUMERIC,
            cover BLOB,
            score REAL,
            big INTEGER,
            title_len INTEGER GENERATED ALWAYS AS (length(title)) VIRTUAL
        );
        CREATE INDEX books_title_idx ON books (title, price DESC);
        CREATE INDEX books_lower_idx ON books (lower(title));
        CREATE TABLE tags (book_id INTEGER, tag TEXT, PRIMARY KEY (book_id, tag)) WITHOUT ROWID;
        CREATE VIEW cheap_books AS SELECT id, title FROM books WHERE price < 10;
        CREATE TRIGGER books_ai AFTER INSERT ON books BEGIN
            UPDATE books SET title = trim(title) WHERE id = NEW.id;
            SELECT CASE WHEN NEW.price < 0 THEN RAISE(ABORT, 'negative; price') END;
        END;
        INSERT INTO authors (name, born) VALUES ('Ann', '1970-01-02'), ('Bob', NULL), ('Cid', '1990-12-31');
        INSERT INTO books (author_id, title, price, cover, score, big) VALUES
            (1, 'A; tricky ''title''', 9.99, X'00FF', 1.5, 9223372036854775807),
            (2, 'Second', NULL, NULL, NULL, 5);
    "#;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[tokio::test]
    #[ignore]
    async fn sqlite_end_to_end() {
        let dir = std::env::temp_dir().join(format!("devcat-sqlite-it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let _ = std::fs::remove_file(&path);
        let p = path.to_string_lossy().to_string();
        // missing file without "create" is an error
        assert!(SqliteDriver::connect(&cfg(&p, false, false)).await.is_err());
        let d = SqliteDriver::connect(&cfg(&p, true, false)).await.expect("connect");
        d.execute(SETUP, &opts()).await.expect("setup");
        assert!(d.server_version().await.unwrap().starts_with("SQLite 3."));

        // --- tree
        let dbs = d.children(&[]).await.unwrap();
        assert_eq!(dbs[0].label, "main");
        let tables = d.children(&s(&["main", "tables"])).await.unwrap();
        assert_eq!(tables.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(), vec!["authors", "books", "tags"]);
        assert_eq!(d.children(&s(&["main", "views"])).await.unwrap()[0].label, "cheap_books");
        assert_eq!(d.children(&s(&["main", "triggers"])).await.unwrap()[0].label, "books_ai");
        assert!(d.children(&s(&["main", "indexes"])).await.unwrap().iter().any(|i| i.label == "books_title_idx"));
        let cols = d.children(&s(&["main", "tables", "books", "columns"])).await.unwrap();
        assert_eq!(cols.len(), 8);
        let idx = d.children(&s(&["main", "tables", "books", "indexes"])).await.unwrap();
        assert!(idx.iter().any(|i| i.label == "books_title_idx" && i.detail.as_deref() == Some("(title, price)")), "{idx:?}");
        assert!(idx.iter().any(|i| i.label == "books_lower_idx" && i.detail.as_deref() == Some("((expression))")), "{idx:?}");

        // --- execute (trigger body with ';' and CASE ... END)
        let r = d.execute("select * from books order by id; select 1 + 1 as two", &opts()).await.unwrap();
        let row = &r[0].rows[0];
        let col = |n: &str| r[0].columns.iter().position(|c| c.name == n).unwrap();
        assert_eq!(row[col("title")], json!("A; tricky 'title'"));
        assert_eq!(row[col("price")], json!(9.99));
        assert_eq!(row[col("cover")], json!("\\x00ff"));
        assert_eq!(row[col("big")], json!("9223372036854775807"));
        assert_eq!(row[col("title_len")], json!(17));
        assert_eq!(r[1].rows[0][0], json!(2));
        let err = d.execute("insert into books(title, price) values ('x', -1)", &opts()).await.unwrap_err().to_string();
        assert!(err.contains("negative; price"), "{err}");
        let r = d.execute("select id from books where 0", &opts()).await.unwrap();
        assert_eq!(r[0].columns.len(), 1);
        let err = d.execute("select 1;\nselect * from nope", &opts()).await.unwrap_err().to_string();
        assert!(err.contains("statement 2 of 2") && err.contains("no such table"), "{err}");
        // temp tables / transactions persist across executions
        d.execute("create temp table t(a); begin; insert into t values (1)", &opts()).await.unwrap();
        d.execute("rollback", &opts()).await.unwrap();
        assert_eq!(d.execute("select count(*) from t", &opts()).await.unwrap()[0].rows[0][0], json!(0));

        // --- table data
        let authors = TableRef { database: Some("main".into()), schema: None, name: "authors".into() };
        let page = d
            .table_data(&TableDataRequest {
                table: authors.clone(),
                offset: 0,
                limit: 2,
                filter: Some("born IS NOT NULL OR name = 'Bob'".into()),
                sort: vec![SortSpec { column: "name".into(), desc: true }],
            })
            .await
            .unwrap();
        assert_eq!(page.total, Some(3));
        assert_eq!(page.primary_key, vec!["id"]);
        assert_eq!(page.result.rows[0][1], json!("Cid"));

        // --- structure
        let books = TableRef { name: "books".into(), ..authors.clone() };
        let st = d.table_structure(&books).await.unwrap();
        assert!(st.columns[0].is_primary_key && st.columns[0].auto_increment);
        assert_eq!(st.columns[2].default.as_deref(), Some("'untitled'"));
        assert_eq!(st.columns[2].data_type, "VARCHAR(200)");
        assert!(!st.columns[2].nullable);
        assert_eq!(st.foreign_keys[0].ref_table, "authors");
        assert_eq!(st.foreign_keys[0].ref_columns, vec!["id"]);
        assert_eq!(st.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
        let tags = d.table_structure(&TableRef { name: "tags".into(), ..authors.clone() }).await.unwrap();
        assert_eq!(tags.columns.iter().filter(|c| c.is_primary_key).count(), 2);
        assert!(!tags.columns[0].auto_increment);

        // --- apply changes
        let mut ins = Map::new();
        ins.insert("author_id".into(), json!(3));
        ins.insert("title".into(), json!("  New  "));
        ins.insert("cover".into(), json!("\\x0102"));
        ins.insert("price".into(), json!(1.25));
        assert_eq!(d.apply_changes(&books, &[RowChange::Insert { values: ins }]).await.unwrap(), 1);
        let r = d.execute("select title, cover, price from books where author_id = 3", &opts()).await.unwrap();
        assert_eq!(r[0].rows[0], vec![json!("New"), json!("\\x0102"), json!(1.25)]);
        let key: Map<String, Value> = [("id".to_string(), json!(2))].into_iter().collect();
        let vals: Map<String, Value> = [("name".to_string(), json!("Bobby"))].into_iter().collect();
        assert_eq!(d.apply_changes(&authors, &[RowChange::Update { key: key.clone(), values: vals }]).await.unwrap(), 1);
        let missing: Map<String, Value> = [("id".to_string(), json!(999))].into_iter().collect();
        let err = d
            .apply_changes(&authors, &[RowChange::Delete { key: key.clone() }, RowChange::Delete { key: missing }])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("affected 0 rows"), "{err}");
        assert_eq!(d.execute("select name from authors where id = 2", &opts()).await.unwrap()[0].rows[0][0], json!("Bobby"));

        // --- DDL
        let ddl = d.object_ddl(&books, &NodeKind::Table).await.unwrap();
        assert!(ddl.starts_with("CREATE TABLE books") && ddl.contains("CREATE INDEX books_title_idx") && ddl.contains("CREATE TRIGGER books_ai"), "{ddl}");
        let v = d.object_ddl(&TableRef { name: "cheap_books".into(), ..authors.clone() }, &NodeKind::View).await.unwrap();
        assert!(v.starts_with("CREATE VIEW cheap_books"));

        // --- snapshot
        let snap = d.schema_snapshot(None, None).await.unwrap();
        assert_eq!(snap.tables.len(), 3);
        assert_eq!(snap.views.len(), 1);

        // --- cancel a long-running statement
        let d = Arc::new(d);
        let d2 = d.clone();
        let h = tokio::spawn(async move {
            d2.execute("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c", &opts()).await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        d.cancel().await.unwrap();
        let res = tokio::time::timeout(Duration::from_secs(5), h).await.expect("cancel took too long").unwrap();
        assert!(res.unwrap_err().to_string().contains("cancelled"));
        // usable again afterwards
        assert_eq!(d.execute("select 1", &opts()).await.unwrap()[0].rows[0][0], json!(1));
        d.close().await;

        // --- truncation stops stepping instead of producing every row
        {
            let d = SqliteDriver::connect(&cfg(&p, false, false)).await.unwrap();
            let o = ExecOptions { database: None, schema: None, max_rows: Some(1000) };
            let started = Instant::now();
            let r = d
                .execute("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT x FROM c; select 'next'", &o)
                .await
                .unwrap();
            let took = started.elapsed();
            eprintln!("sqlite 5M-row select truncated in {took:?}");
            assert!(took < Duration::from_millis(1000), "took {took:?}");
            assert!(r[0].truncated);
            assert_eq!(r[0].rows.len(), 1000);
            assert_eq!(r[1].rows[0][0], json!("next"));
            assert_eq!(d.execute("select 42", &o).await.unwrap()[0].rows[0][0], json!(42));
            d.close().await;
        }

        // --- read-only
        let ro = SqliteDriver::connect(&cfg(&p, false, true)).await.unwrap();
        assert!(ro.execute("select * from authors", &opts()).await.is_ok());
        assert!(ro.execute("delete from authors", &opts()).await.unwrap_err().to_string().contains("read-only"));
        assert!(ro.execute("pragma user_version = 3", &opts()).await.is_err());
        ro.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
