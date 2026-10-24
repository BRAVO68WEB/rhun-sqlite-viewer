//! A SQLite database kept in a private file, with schema, queries, row edits, and undo.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::sql::{quote_ident, quote_lit, split_sql};

/// Files larger than this stay on their real path and open read-only.
pub const MAX_COPY: u64 = 256 * 1024 * 1024;
/// Rows in one table page. One extra row is fetched to know whether another page exists.
pub const PAGE_LEN: i64 = 100;
/// Rows kept from the last statement of a script.
pub const QUERY_CAP: usize = 500;
const UNDO_MAX: usize = 20;
const SNAP_MAX: u64 = 256 * 1024 * 1024;

fn msg<T, E: ToString>(result: Result<T, E>) -> Result<T, String> {
    result.map_err(|err| err.to_string())
}

/// A table or view column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: String,
    pub notnull: bool,
    pub pk: i32,
}

/// What a schema entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelKind {
    Table,
    View,
    Virtual,
}

/// A table, view, or virtual table.
#[derive(Clone, Debug)]
pub struct Relation {
    pub name: String,
    pub kind: RelKind,
    pub columns: Vec<ColumnInfo>,
    pub without_rowid: bool,
}

/// An index and the table it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub table: String,
}

/// Tables, views, and indexes of the main schema.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub tables: Vec<Relation>,
    pub views: Vec<Relation>,
    pub indexes: Vec<IndexInfo>,
}

impl Schema {
    /// The table or view with this name. ASCII case is ignored when the exact name is absent.
    pub fn find(&self, name: &str) -> Option<&Relation> {
        self.tables
            .iter()
            .chain(self.views.iter())
            .find(|rel| rel.name == name)
            .or_else(|| {
                self.tables
                    .iter()
                    .chain(self.views.iter())
                    .find(|rel| rel.name.eq_ignore_ascii_case(name))
            })
    }
}

/// One value from a result cell. Blobs keep only their length.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Null,
    Text(String),
    Blob(usize),
}

/// How a browsed row is found again for UPDATE and DELETE.
#[derive(Clone, Debug, PartialEq)]
pub enum RowKey {
    None,
    Rowid(i64),
    Pk(Vec<(String, Option<String>)>),
}

/// One row of a table page.
#[derive(Clone, Debug)]
pub struct PageRow {
    pub key: RowKey,
    pub cells: Vec<Cell>,
}

/// A page of a table or view.
#[derive(Clone, Debug)]
pub struct Page {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<PageRow>,
    pub more: bool,
    /// Rows can be updated: the database is writable, this is not a view, and rows have a key.
    pub editable: bool,
}

impl Page {
    pub fn empty() -> Page {
        Page {
            columns: Vec::new(),
            rows: Vec::new(),
            more: false,
            editable: false,
        }
    }

    /// A query result. Those rows are not updated in place.
    pub fn from_query(columns: Vec<String>, rows: Vec<Vec<Cell>>) -> Page {
        Page {
            columns: columns
                .into_iter()
                .map(|name| ColumnInfo {
                    name,
                    ty: String::new(),
                    notnull: false,
                    pk: 0,
                })
                .collect(),
            rows: rows
                .into_iter()
                .map(|cells| PageRow {
                    key: RowKey::None,
                    cells,
                })
                .collect(),
            more: false,
            editable: false,
        }
    }
}

/// What the user changed in one column. `None` writes NULL.
#[derive(Clone, Debug)]
pub struct RowChange {
    pub index: usize,
    pub value: Option<String>,
}

/// The outcome of a script.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub message: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    pub wrote: bool,
}

struct Hist {
    before: u64,
    after: u64,
    snap: PathBuf,
    bytes: u64,
    sql: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum History {
    /// A user edit: snapshot, then keep it for undo.
    Keep,
    /// Redo: run the stored SQL without a new snapshot.
    Redo,
    /// A read: refuse statements that write.
    Read,
}

/// An open database. `work` is removed on [`Database::cleanup`]; read-only opens have none.
pub struct Database {
    path: PathBuf,
    work: Option<PathBuf>,
    conn: Connection,
    writable: bool,
    state_id: u64,
    saved_id: u64,
    undo: Vec<Hist>,
    redo: Vec<Hist>,
    snap_seq: u64,
    schema: Schema,
}

impl Database {
    /// Open `work/db.sqlite` read-write. A missing or empty file becomes a database and starts dirty.
    pub fn open_owned(work: PathBuf) -> Result<Database, String> {
        fs::create_dir_all(&work).map_err(|err| err.to_string())?;
        let path = work.join("db.sqlite");
        let before = fingerprint(&path);
        let conn = msg(Connection::open(&path))?;
        configure(&conn, true)?;
        let after = fingerprint(&path);
        let saved_id = if before == after { 1 } else { 0 };
        let mut db = Database {
            path,
            work: Some(work),
            conn,
            writable: true,
            state_id: 1,
            saved_id,
            undo: Vec::new(),
            redo: Vec::new(),
            snap_seq: 1,
            schema: Schema::default(),
        };
        db.reload_schema()?;
        Ok(db)
    }

    /// Open an existing file read-only. Nothing is copied and nothing is editable.
    pub fn open_readonly(path: PathBuf) -> Result<Database, String> {
        let conn = msg(Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        ))?;
        configure(&conn, false)?;
        let mut db = Database {
            path,
            work: None,
            conn,
            writable: false,
            state_id: 1,
            saved_id: 1,
            undo: Vec::new(),
            redo: Vec::new(),
            snap_seq: 1,
            schema: Schema::default(),
        };
        db.reload_schema()?;
        Ok(db)
    }

    /// Close the connection and delete the private directory.
    pub fn cleanup(self) {
        let Database { conn, work, .. } = self;
        drop(conn);
        if let Some(dir) = work {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    pub fn is_dirty(&self) -> bool {
        self.writable && self.state_id != self.saved_id
    }

    pub fn state_id(&self) -> u64 {
        self.state_id
    }

    pub fn file_len(&self) -> u64 {
        fs::metadata(&self.path).map(|meta| meta.len()).unwrap_or(0)
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn mark_saved(&mut self) {
        self.saved_id = self.state_id;
    }

    /// A restored backup is unsaved even when its bytes already form a database.
    pub fn mark_unsaved(&mut self) {
        if self.writable {
            self.saved_id = self.state_id.wrapping_sub(1);
        }
    }

    pub fn reload_schema(&mut self) -> Result<(), String> {
        self.schema = load_schema(&self.conn)?;
        Ok(())
    }

    /// Re-read the file after it changes on disk.
    pub fn reload(&mut self) -> Result<(), String> {
        self.reopen()?;
        self.reload_schema()
    }

    /// Replace the private file with `bytes` and drop undo history.
    pub fn replace_with(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.writable {
            return self.reload();
        }
        self.close_conn()?;
        let rescue = self.path.with_file_name("rescue.sqlite");
        let had = self.path.exists();
        if had {
            msg(fs::copy(&self.path, &rescue))?;
        }
        let wrote = (|| {
            let mut file = File::create(&self.path)?;
            file.write_all(bytes)?;
            file.flush()
        })();
        if let Err(err) = wrote {
            if had {
                let _ = fs::copy(&rescue, &self.path);
            }
            let _ = fs::remove_file(&rescue);
            let _ = self.reopen();
            return Err(err.to_string());
        }
        remove_sidecars(&self.path);
        if let Err(err) = self.reopen().and_then(|_| self.reload_schema()) {
            if had {
                let _ = fs::copy(&rescue, &self.path);
                let _ = self.reopen();
                let _ = self.reload_schema();
            }
            let _ = fs::remove_file(&rescue);
            return Err(err);
        }
        let _ = fs::remove_file(&rescue);
        self.clear_history();
        self.state_id = 1;
        self.saved_id = 1;
        Ok(())
    }

    /// Run a script. The first write is snapshotted; a later failure restores that snapshot.
    pub fn apply_sql(&mut self, sql: &str) -> Result<RunResult, String> {
        self.exec(sql, History::Keep)
    }

    /// Run a script that must not write. Used to show the last query again after undo.
    pub fn query(&mut self, sql: &str) -> Result<RunResult, String> {
        self.exec(sql, History::Read)
    }

    pub fn undo(&mut self) -> Result<(), String> {
        if !self.writable {
            return Err("This database is read-only.".into());
        }
        let hist = self.undo.pop().ok_or("Nothing to undo.")?;
        if let Err(err) = self.restore_file(&hist.snap) {
            self.undo.push(hist);
            return Err(err);
        }
        self.state_id = hist.before;
        self.redo.push(hist);
        self.reload_schema()?;
        Ok(())
    }

    pub fn redo(&mut self) -> Result<RunResult, String> {
        if !self.writable {
            return Err("This database is read-only.".into());
        }
        let hist = self.redo.pop().ok_or("Nothing to redo.")?;
        match self.exec(&hist.sql, History::Redo) {
            Ok(mut result) => {
                self.state_id = hist.after;
                self.undo.push(hist);
                if let Err(err) = self.reload_schema() {
                    result
                        .message
                        .push_str(&format!(" Schema was not reloaded: {err}"));
                }
                Ok(result)
            }
            Err(err) => {
                let _ = self.restore_file(&hist.snap);
                self.state_id = hist.before;
                let _ = self.reload_schema();
                self.redo.push(hist);
                Err(err)
            }
        }
    }

    /// One page of a table or view. `order` is a column name and whether it is descending.
    pub fn sqlite_version(&self) -> String {
        self.conn
            .query_row("SELECT sqlite_version()", [], |row| row.get(0))
            .unwrap_or_else(|_| "SQLite".into())
    }

    /// `CREATE` statement for a table or view, then its `CREATE INDEX` statements.
    pub fn schema_script(&self, name: &str) -> String {
        let mut lines = Vec::new();
        if let Ok(Some(sql)) = self.conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name = ?1 AND type IN ('table', 'view')",
            [name],
            |row| row.get::<_, Option<String>>(0),
        ) {
            lines.push(sql);
        }
        if let Ok(mut stmt) = self.conn.prepare(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
             AND sql IS NOT NULL ORDER BY name",
        ) {
            if let Ok(mut rows) = stmt.query([name]) {
                while let Ok(Some(row)) = rows.next() {
                    if let Ok(Some(sql)) = row.get::<_, Option<String>>(0) {
                        lines.push(sql);
                    }
                }
            }
        }
        lines.join(";\n")
    }

    pub fn browse(
        &self,
        name: &str,
        order: Option<(&str, bool)>,
        offset: i64,
        filter: Option<&str>,
    ) -> Result<Page, String> {
        let rel = self
            .schema
            .find(name)
            .ok_or_else(|| format!("No table named {name}."))?
            .clone();
        let offset = offset.max(0);
        let limit = PAGE_LEN + 1;
        let order_sql = match order {
            Some((column, desc)) if rel.columns.iter().any(|info| info.name == column) => {
                format!(
                    " ORDER BY {} {}",
                    quote_ident(column),
                    if desc { "DESC" } else { "ASC" }
                )
            }
            _ => String::new(),
        };
        let ident = quote_ident(&rel.name);
        let where_sql = filter_sql(&rel, filter);
        let with_rowid = rel.kind != RelKind::View && !rel.without_rowid;
        let sql_with = format!(
            "SELECT *, rowid AS \"_rhun_rowid\" FROM {ident}{where_sql}{order_sql} LIMIT {limit} OFFSET {offset}"
        );
        let sql_without =
            format!("SELECT * FROM {ident}{where_sql}{order_sql} LIMIT {limit} OFFSET {offset}");
        let (mut columns, rows, mut used_rowid) = if with_rowid {
            match query_rows(&self.conn, &sql_with) {
                Ok((columns, rows)) => (columns, rows, true),
                Err(_) => {
                    let (columns, rows) = query_rows(&self.conn, &sql_without)?;
                    (columns, rows, false)
                }
            }
        } else {
            let (columns, rows) = query_rows(&self.conn, &sql_without)?;
            (columns, rows, false)
        };
        if used_rowid
            && (columns.last().map(|column| column.name.as_str()) == Some("_rhun_rowid")
                || columns.len() == rel.columns.len() + 1)
        {
            columns.pop();
        } else if used_rowid {
            used_rowid = false;
        }
        let mut page_rows = Vec::with_capacity(rows.len());
        for mut cells in rows {
            let key = if used_rowid {
                match cells.pop().and_then(|cell| cell_i64(&cell)) {
                    Some(rowid) => RowKey::Rowid(rowid),
                    None => RowKey::None,
                }
            } else if rel.kind == RelKind::View {
                RowKey::None
            } else {
                pk_key(&rel, &cells)
            };
            page_rows.push(PageRow { key, cells });
        }
        let more = page_rows.len() > PAGE_LEN as usize;
        if more {
            page_rows.truncate(PAGE_LEN as usize);
        }
        let keyed = used_rowid || rel.columns.iter().any(|info| info.pk > 0);
        if columns.len() == rel.columns.len() {
            for (info, column) in rel.columns.iter().zip(columns.iter_mut()) {
                if column.name == info.name {
                    column.ty = info.ty.clone();
                    column.notnull = info.notnull;
                    column.pk = info.pk;
                }
            }
        }
        Ok(Page {
            columns,
            rows: page_rows,
            more,
            editable: self.writable && rel.kind != RelKind::View && keyed,
        })
    }

    pub fn update_row(
        &mut self,
        table: &str,
        key: &RowKey,
        changes: &[RowChange],
    ) -> Result<RunResult, String> {
        let rel = self.require_table(table)?;
        if changes.is_empty() {
            return Ok(plain("Nothing to update."));
        }
        let sets = assignments(&rel, changes)?;
        let sql = format!(
            "UPDATE {} SET {sets} WHERE {}",
            quote_ident(&rel.name),
            where_sql(key)?
        );
        self.apply_sql(&sql)
    }

    pub fn insert_row(&mut self, table: &str, changes: &[RowChange]) -> Result<RunResult, String> {
        let rel = self.require_table(table)?;
        let sql = if changes.is_empty() {
            format!("INSERT INTO {} DEFAULT VALUES", quote_ident(&rel.name))
        } else {
            let mut names = Vec::new();
            let mut values = Vec::new();
            for change in changes {
                let column = rel
                    .columns
                    .get(change.index)
                    .ok_or("Column is out of range.")?;
                names.push(quote_ident(&column.name));
                values.push(quote_lit(&change.value));
            }
            format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quote_ident(&rel.name),
                names.join(", "),
                values.join(", ")
            )
        };
        self.apply_sql(&sql)
    }

    pub fn delete_row(&mut self, table: &str, key: &RowKey) -> Result<RunResult, String> {
        let rel = self.require_table(table)?;
        let sql = format!(
            "DELETE FROM {} WHERE {}",
            quote_ident(&rel.name),
            where_sql(key)?
        );
        self.apply_sql(&sql)
    }

    /// A consistent single-file copy for Save. The live connection stays on the working file.
    pub fn snapshot_file(&mut self) -> Result<PathBuf, String> {
        if !self.writable {
            return Err("This database is read-only.".into());
        }
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        if !self.conn.is_autocommit() {
            msg(self.conn.execute("COMMIT", []))?;
        }
        let dest = self
            .path
            .parent()
            .ok_or("The database has no directory.")?
            .join("save.sqlite");
        self.backup_to(&dest)?;
        Ok(dest)
    }

    fn require_table(&self, name: &str) -> Result<Relation, String> {
        match self.schema.find(name) {
            Some(rel) if rel.kind == RelKind::View => Err("Views are read-only.".into()),
            Some(rel) => Ok(rel.clone()),
            None => Err(format!("No table named {name}.")),
        }
    }

    fn exec(&mut self, sql: &str, history: History) -> Result<RunResult, String> {
        let statements = split_sql(sql);
        if statements.is_empty() {
            return Err("Enter a SQL statement.".into());
        }
        let mut snap: Option<PathBuf> = None;
        let mut wrote = false;
        let mut changed = 0u64;
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let mut saw_rows = false;
        let mut truncated = false;
        for statement in &statements {
            let writes = match statement_writes(&self.conn, statement) {
                Ok(writes) => writes,
                Err(err) => {
                    self.rollback_snap(snap.as_deref());
                    return Err(err);
                }
            };
            if writes && history == History::Read {
                self.rollback_snap(snap.as_deref());
                return Err("This statement changes the database.".into());
            }
            if writes && !self.writable {
                self.rollback_snap(snap.as_deref());
                return Err("This database is read-only.".into());
            }
            if writes && history == History::Keep && snap.is_none() {
                snap = Some(self.take_snapshot()?);
            }
            match run_one(&self.conn, statement) {
                Ok(one) => {
                    if one.wrote {
                        wrote = true;
                        changed = changed.saturating_add(one.changed);
                    }
                    if one.has_rows {
                        saw_rows = true;
                        columns = one.columns;
                        rows = one.rows;
                        truncated = one.truncated;
                    }
                }
                Err(err) => {
                    self.rollback_snap(snap.as_deref());
                    return Err(err);
                }
            }
        }
        if !self.conn.is_autocommit() {
            if let Err(err) = self.conn.execute("COMMIT", []) {
                self.rollback_snap(snap.as_deref());
                return Err(err.to_string());
            }
        }
        if history == History::Keep {
            if wrote {
                if let Some(path) = snap.take() {
                    let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                    let before = self.state_id;
                    let after = before.wrapping_add(1);
                    self.clear_redo();
                    self.state_id = after;
                    self.undo.push(Hist {
                        before,
                        after,
                        snap: path,
                        bytes,
                        sql: sql.to_string(),
                    });
                    self.trim_undo();
                }
                if let Err(err) = self.reload_schema() {
                    let mut result =
                        result_of(wrote, changed, &columns, &rows, saw_rows, truncated);
                    result
                        .message
                        .push_str(&format!(" Schema was not reloaded: {err}"));
                    return Ok(result);
                }
            } else if let Some(path) = snap {
                let _ = fs::remove_file(path);
            }
        }
        Ok(result_of(
            wrote, changed, &columns, &rows, saw_rows, truncated,
        ))
    }

    fn take_snapshot(&mut self) -> Result<PathBuf, String> {
        // Leave an open transaction alone. Committing it here would make the script's own
        // COMMIT fail, and the backup still sees the pages from before this write.
        let dir = self
            .path
            .parent()
            .ok_or("The database has no directory.")?
            .join("undo");
        fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let dest = dir.join(format!("{}.sqlite", self.snap_seq));
        self.snap_seq = self.snap_seq.wrapping_add(1);
        self.backup_to(&dest)?;
        Ok(dest)
    }

    fn backup_to(&self, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let _ = fs::remove_file(dest);
        let mut dst = msg(Connection::open(dest))?;
        {
            let backup = msg(Backup::new(&self.conn, &mut dst))?;
            let mut busy = 0;
            loop {
                match msg(backup.step(128))? {
                    StepResult::Done => break,
                    StepResult::More => busy = 0,
                    StepResult::Busy | StepResult::Locked => {
                        busy += 1;
                        if busy > 20 {
                            return Err("The database is busy.".into());
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    _ => return Err("The database backup stopped.".into()),
                }
            }
        }
        dst.close().map_err(|(_, err)| err.to_string())?;
        Ok(())
    }

    fn rollback_snap(&mut self, snap: Option<&Path>) {
        if !self.conn.is_autocommit() {
            let _ = self.conn.execute("ROLLBACK", []);
        }
        if let Some(path) = snap {
            let _ = self.restore_file(path);
            let _ = fs::remove_file(path);
        }
    }

    fn restore_file(&mut self, snap: &Path) -> Result<(), String> {
        self.close_conn()?;
        remove_sidecars(&self.path);
        msg(fs::copy(snap, &self.path))?;
        remove_sidecars(&self.path);
        self.reopen()?;
        Ok(())
    }

    fn close_conn(&mut self) -> Result<(), String> {
        let memory = msg(Connection::open_in_memory())?;
        let old = std::mem::replace(&mut self.conn, memory);
        old.close().map_err(|(_, err)| err.to_string())
    }

    fn reopen(&mut self) -> Result<(), String> {
        let conn = if self.writable {
            msg(Connection::open(&self.path))?
        } else {
            msg(Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            ))?
        };
        configure(&conn, self.writable)?;
        self.conn = conn;
        Ok(())
    }

    fn clear_redo(&mut self) {
        for hist in self.redo.drain(..) {
            let _ = fs::remove_file(hist.snap);
        }
    }

    fn clear_history(&mut self) {
        self.clear_redo();
        for hist in self.undo.drain(..) {
            let _ = fs::remove_file(hist.snap);
        }
    }

    fn trim_undo(&mut self) {
        loop {
            let bytes: u64 = self.undo.iter().map(|hist| hist.bytes).sum();
            let too_many = self.undo.len() > UNDO_MAX;
            let too_big = bytes > SNAP_MAX && self.undo.len() > 1;
            if !too_many && !too_big {
                break;
            }
            let old = self.undo.remove(0);
            let _ = fs::remove_file(old.snap);
        }
    }
}

fn plain(message: &str) -> RunResult {
    RunResult {
        message: message.to_string(),
        columns: Vec::new(),
        rows: Vec::new(),
        wrote: false,
    }
}

fn result_of(
    wrote: bool,
    changed: u64,
    columns: &[String],
    rows: &[Vec<Cell>],
    saw_rows: bool,
    truncated: bool,
) -> RunResult {
    let message = if !saw_rows {
        if wrote {
            if changed > 0 {
                format!("{changed} row(s) changed.")
            } else {
                "Statement finished.".into()
            }
        } else {
            "Done.".into()
        }
    } else if truncated {
        let mut message = format!("Showing {QUERY_CAP} rows.");
        if wrote {
            message.push_str(&format!(" {changed} row(s) changed."));
        }
        message
    } else {
        let mut message = format!("{} row(s).", rows.len());
        if wrote {
            message.push_str(&format!(" {changed} row(s) changed."));
        }
        message
    };
    RunResult {
        message,
        columns: if saw_rows {
            columns.to_vec()
        } else {
            Vec::new()
        },
        rows: if saw_rows { rows.to_vec() } else { Vec::new() },
        wrote,
    }
}

fn configure(conn: &Connection, writable: bool) -> Result<(), String> {
    msg(conn.busy_timeout(Duration::from_secs(5)))?;
    msg(conn.execute_batch("PRAGMA foreign_keys = ON;"))?;
    if writable {
        msg(conn.execute_batch("PRAGMA journal_mode = DELETE;"))?;
    } else {
        let _ = conn.execute_batch("PRAGMA query_only = ON;");
    }
    Ok(())
}

fn fingerprint(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut hash = 0xcbf29ce484222325u64;
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        for byte in &buf[..n] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    Some(hash)
}

fn remove_sidecars(path: &Path) {
    let _ = fs::remove_file(sidecar(path, "-wal"));
    let _ = fs::remove_file(sidecar(path, "-shm"));
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Copy a `-wal` and `-shm` next to `dest` when they sit next to `src` and fit in `budget`.
pub fn copy_sidecars(src: &Path, dest: &Path, budget: u64) -> u64 {
    let mut used = 0u64;
    for suffix in ["-wal", "-shm"] {
        let from = sidecar(src, suffix);
        let len = fs::metadata(&from).map(|meta| meta.len()).unwrap_or(0);
        if len == 0 || used.saturating_add(len) > budget {
            continue;
        }
        if fs::copy(&from, sidecar(dest, suffix)).is_ok() {
            used = used.saturating_add(len);
        }
    }
    used
}

/// Bytes in the `-wal` file, or 0 when there is none.
pub fn wal_len(path: &Path) -> u64 {
    fs::metadata(sidecar(path, "-wal"))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

fn load_schema(conn: &Connection) -> Result<Schema, String> {
    let listed = load_pragma_list(conn);
    let (entries, indexes) = match listed {
        Ok(entries) => (entries, load_indexes(conn)?),
        Err(_) => (load_master_list(conn)?, load_indexes(conn)?),
    };
    let mut tables = Vec::new();
    let mut views = Vec::new();
    for (name, kind, without_rowid) in entries {
        let columns = load_columns(conn, &name).unwrap_or_default();
        let rel = Relation {
            name,
            kind,
            columns,
            without_rowid,
        };
        if kind == RelKind::View {
            views.push(rel);
        } else {
            tables.push(rel);
        }
    }
    Ok(Schema {
        tables,
        views,
        indexes,
    })
}

fn load_pragma_list(conn: &Connection) -> Result<Vec<(String, RelKind, bool)>, String> {
    let mut stmt = msg(conn.prepare(
        "SELECT name, type, wr FROM pragma_table_list \
         WHERE schema = 'main' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ))?;
    let mut rows = msg(stmt.query([]))?;
    let mut out = Vec::new();
    while let Some(row) = msg(rows.next())? {
        let name: String = msg(row.get(0))?;
        let kind_name: String = msg(row.get(1))?;
        let wr: i64 = msg(row.get(2))?;
        let kind = match kind_name.as_str() {
            "view" => RelKind::View,
            "shadow" => continue,
            "virtual" => RelKind::Virtual,
            _ => RelKind::Table,
        };
        out.push((name, kind, wr != 0));
    }
    Ok(out)
}

fn load_master_list(conn: &Connection) -> Result<Vec<(String, RelKind, bool)>, String> {
    let mut stmt = msg(conn.prepare(
        "SELECT name, type, sql FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ))?;
    let mut rows = msg(stmt.query([]))?;
    let mut out = Vec::new();
    while let Some(row) = msg(rows.next())? {
        let name: String = msg(row.get(0))?;
        let kind_name: String = msg(row.get(1))?;
        let sql: Option<String> = msg(row.get(2))?;
        let without = sql
            .unwrap_or_default()
            .to_ascii_uppercase()
            .contains("WITHOUT ROWID");
        let kind = if kind_name == "view" {
            RelKind::View
        } else {
            RelKind::Table
        };
        out.push((name, kind, without));
    }
    Ok(out)
}

fn load_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>, String> {
    let mut stmt =
        msg(conn.prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?)"))?;
    let mut rows = msg(stmt.query([table]))?;
    let mut columns = Vec::new();
    while let Some(row) = msg(rows.next())? {
        let name: String = msg(row.get(0))?;
        let ty: String = msg(row.get(1))?;
        let notnull: i64 = msg(row.get(2))?;
        let pk: i64 = msg(row.get(3))?;
        columns.push(ColumnInfo {
            name,
            ty,
            notnull: notnull != 0,
            pk: pk as i32,
        });
    }
    Ok(columns)
}

fn load_indexes(conn: &Connection) -> Result<Vec<IndexInfo>, String> {
    let mut stmt = msg(conn.prepare(
        "SELECT name, tbl_name FROM sqlite_master \
         WHERE type = 'index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ))?;
    let mut rows = msg(stmt.query([]))?;
    let mut indexes = Vec::new();
    while let Some(row) = msg(rows.next())? {
        indexes.push(IndexInfo {
            name: msg(row.get(0))?,
            table: msg(row.get(1))?,
        });
    }
    Ok(indexes)
}

struct One {
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
    has_rows: bool,
    wrote: bool,
    changed: u64,
    truncated: bool,
}

fn statement_writes(conn: &Connection, sql: &str) -> Result<bool, String> {
    let stmt = msg(conn.prepare(sql))?;
    Ok(!stmt.readonly())
}

fn run_one(conn: &Connection, sql: &str) -> Result<One, String> {
    let mut stmt = msg(conn.prepare(sql))?;
    let wrote = !stmt.readonly();
    let count = stmt.column_count();
    if count == 0 {
        let changed = msg(stmt.execute([]))? as u64;
        return Ok(One {
            columns: Vec::new(),
            rows: Vec::new(),
            has_rows: false,
            wrote,
            changed,
            truncated: false,
        });
    }
    let mut columns = Vec::with_capacity(count);
    for index in 0..count {
        columns.push(msg(stmt.column_name(index))?.to_string());
    }
    let mut rows_iter = msg(stmt.query([]))?;
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = msg(rows_iter.next())? {
        if rows.len() >= QUERY_CAP {
            truncated = true;
            if !wrote {
                break;
            }
            continue;
        }
        let mut cells = Vec::with_capacity(count);
        for index in 0..count {
            cells.push(cell_of(row, index)?);
        }
        rows.push(cells);
    }
    let changed = if wrote { conn.changes() } else { 0 };
    Ok(One {
        columns,
        rows,
        has_rows: true,
        wrote,
        changed,
        truncated,
    })
}

fn query_rows(conn: &Connection, sql: &str) -> Result<(Vec<ColumnInfo>, Vec<Vec<Cell>>), String> {
    let mut stmt = msg(conn.prepare(sql))?;
    let count = stmt.column_count();
    let mut columns = Vec::with_capacity(count);
    for index in 0..count {
        columns.push(ColumnInfo {
            name: msg(stmt.column_name(index))?.to_string(),
            ty: String::new(),
            notnull: false,
            pk: 0,
        });
    }
    let mut rows_iter = msg(stmt.query([]))?;
    let mut rows = Vec::new();
    while let Some(row) = msg(rows_iter.next())? {
        let mut cells = Vec::with_capacity(count);
        for index in 0..count {
            cells.push(cell_of(row, index)?);
        }
        rows.push(cells);
    }
    Ok((columns, rows))
}

fn cell_of(row: &rusqlite::Row<'_>, index: usize) -> Result<Cell, String> {
    match msg(row.get_ref(index))? {
        ValueRef::Null => Ok(Cell::Null),
        ValueRef::Integer(value) => Ok(Cell::Text(value.to_string())),
        ValueRef::Real(value) => Ok(Cell::Text(value.to_string())),
        ValueRef::Text(bytes) => Ok(Cell::Text(String::from_utf8_lossy(bytes).into_owned())),
        ValueRef::Blob(bytes) => Ok(Cell::Blob(bytes.len())),
    }
}

fn cell_i64(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::Text(text) => text.parse().ok(),
        _ => None,
    }
}

fn pk_key(rel: &Relation, cells: &[Cell]) -> RowKey {
    let mut columns: Vec<_> = rel
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.pk > 0)
        .collect();
    if columns.is_empty() {
        return RowKey::None;
    }
    columns.sort_by_key(|(_, column)| column.pk);
    let mut pairs = Vec::new();
    for (index, column) in columns {
        match cells.get(index) {
            Some(Cell::Blob(_)) => return RowKey::None,
            Some(Cell::Text(text)) => pairs.push((column.name.clone(), Some(text.clone()))),
            _ => pairs.push((column.name.clone(), None)),
        }
    }
    RowKey::Pk(pairs)
}

pub fn predicate(key: &RowKey) -> String {
    where_sql(key).unwrap_or_else(|_| "1 = 0".into())
}

fn filter_sql(rel: &Relation, filter: Option<&str>) -> String {
    let Some(filter) = filter.map(str::trim).filter(|text| !text.is_empty()) else {
        return String::new();
    };
    let pattern = quote_lit(&Some(like_pattern(filter)));
    let parts: Vec<String> = rel
        .columns
        .iter()
        .filter(|column| !column.ty.to_ascii_uppercase().contains("BLOB"))
        .map(|column| format!("{} LIKE {pattern} ESCAPE '\\'", quote_ident(&column.name)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", parts.join(" OR "))
    }
}

fn like_pattern(filter: &str) -> String {
    let mut out = String::from("%");
    for ch in filter.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

fn where_sql(key: &RowKey) -> Result<String, String> {
    match key {
        RowKey::None => Err("This row has no key to update.".into()),
        RowKey::Rowid(rowid) => Ok(format!("rowid = {rowid}")),
        RowKey::Pk(columns) if columns.is_empty() => Err("This row has no key to update.".into()),
        RowKey::Pk(columns) => Ok(columns
            .iter()
            .map(|(name, value)| match value {
                None => format!("{} IS NULL", quote_ident(name)),
                Some(text) => format!("{} = {}", quote_ident(name), quote_lit(&Some(text.clone()))),
            })
            .collect::<Vec<_>>()
            .join(" AND ")),
    }
}

fn assignments(rel: &Relation, changes: &[RowChange]) -> Result<String, String> {
    let mut parts = Vec::new();
    for change in changes {
        let column = rel
            .columns
            .get(change.index)
            .ok_or("Column is out of range.")?;
        parts.push(format!(
            "{} = {}",
            quote_ident(&column.name),
            quote_lit(&change.value)
        ));
    }
    Ok(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Tmp(PathBuf);

    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "rhun-sqlite-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn notes(dir: &Path) {
        let conn = Connection::open(dir.join("db.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);
             INSERT INTO notes(id, body) VALUES (1, 'hello');",
        )
        .unwrap();
    }

    fn body(db: &mut Database) -> Cell {
        let result = db.apply_sql("SELECT body FROM notes WHERE id = 1").unwrap();
        result.rows[0][0].clone()
    }

    #[test]
    fn empty_file_starts_dirty() {
        let tmp = Tmp::new();
        let db = Database::open_owned(tmp.0.clone()).unwrap();
        assert!(db.is_dirty());
    }

    #[test]
    fn existing_database_stays_clean_until_an_edit() {
        let tmp = Tmp::new();
        notes(&tmp.0);
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        assert!(!db.is_dirty(), "pragma setup rewrote the file");
        assert_eq!(body(&mut db), Cell::Text("hello".into()));
        db.apply_sql("UPDATE notes SET body = 'edited' WHERE id = 1")
            .unwrap();
        assert!(db.is_dirty());
        assert_eq!(body(&mut db), Cell::Text("edited".into()));
        db.undo().unwrap();
        assert_eq!(body(&mut db), Cell::Text("hello".into()));
        assert!(!db.is_dirty());
        db.redo().unwrap();
        assert_eq!(body(&mut db), Cell::Text("edited".into()));
        assert!(db.is_dirty());
        let saved = db.snapshot_file().unwrap();
        let check = Connection::open(saved).unwrap();
        let value: String = check
            .query_row("SELECT body FROM notes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "edited");
    }

    #[test]
    fn failed_script_restores_the_snapshot() {
        let tmp = Tmp::new();
        notes(&tmp.0);
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        let err = db
            .apply_sql("UPDATE notes SET body = 'gone' WHERE id = 1; SELECT * FROM missing;")
            .unwrap_err();
        assert!(err.contains("missing"), "{err}");
        assert_eq!(body(&mut db), Cell::Text("hello".into()));
    }

    #[test]
    fn readonly_refuses_writes_and_still_reads() {
        let tmp = Tmp::new();
        notes(&tmp.0);
        let mut db = Database::open_readonly(tmp.0.join("db.sqlite")).unwrap();
        assert!(!db.writable());
        assert!(db
            .apply_sql("UPDATE notes SET body = 'x'")
            .unwrap_err()
            .contains("read-only"));
        assert_eq!(body(&mut db), Cell::Text("hello".into()));
    }

    #[test]
    fn clearing_a_field_writes_null_and_undoes() {
        let tmp = Tmp::new();
        notes(&tmp.0);
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        let page = db.browse("notes", None, 0, None).unwrap();
        let key = page.rows[0].key.clone();
        db.update_row(
            "notes",
            &key,
            &[RowChange {
                index: 1,
                value: None,
            }],
        )
        .unwrap();
        let page = db.browse("notes", None, 0, None).unwrap();
        assert_eq!(page.rows[0].cells[1], Cell::Null);
        db.undo().unwrap();
        assert_eq!(body(&mut db), Cell::Text("hello".into()));
        let page = db.browse("notes", None, 0, Some("ell")).unwrap();
        assert_eq!(page.rows.len(), 1);
        let page = db.browse("notes", None, 0, Some("missing")).unwrap();
        assert!(page.rows.is_empty());
    }

    #[test]
    fn browse_pages_and_sorts() {
        let tmp = Tmp::new();
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        let mut sql = String::from("CREATE TABLE n(i INTEGER);");
        for i in 0..101 {
            sql.push_str(&format!("INSERT INTO n(i) VALUES ({i});"));
        }
        db.apply_sql(&sql).unwrap();
        let page = db.browse("n", None, 0, None).unwrap();
        assert_eq!(page.rows.len(), 100);
        assert!(page.more);
        assert!(page.editable);
        let page = db.browse("n", Some(("i", true)), 0, None).unwrap();
        assert_eq!(page.rows[0].cells[0], Cell::Text("100".into()));
        let page = db.browse("n", None, 100, None).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert!(!page.more);
    }

    #[test]
    fn without_rowid_updates_by_primary_key() {
        let tmp = Tmp::new();
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        db.apply_sql(
            "CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;
             INSERT INTO kv(k, v) VALUES ('a', 'b');",
        )
        .unwrap();
        let page = db.browse("kv", None, 0, None).unwrap();
        assert!(matches!(page.rows[0].key, RowKey::Pk(_)));
        db.update_row(
            "kv",
            &page.rows[0].key,
            &[RowChange {
                index: 1,
                value: Some("c".into()),
            }],
        )
        .unwrap();
        let result = db.apply_sql("SELECT v FROM kv").unwrap();
        assert_eq!(result.rows[0][0], Cell::Text("c".into()));
    }

    #[test]
    fn trigger_script_and_transaction_undo() {
        let tmp = Tmp::new();
        notes(&tmp.0);
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        db.apply_sql(
            "CREATE TABLE log(body TEXT);
             CREATE TEMP TRIGGER t AFTER UPDATE ON notes BEGIN
               INSERT INTO log(body) SELECT CASE WHEN 1 THEN new.body END;
             END;
             UPDATE notes SET body = 'z' WHERE id = 1;",
        )
        .unwrap();
        let log = db.apply_sql("SELECT body FROM log").unwrap();
        assert_eq!(log.rows[0][0], Cell::Text("z".into()));
        db.apply_sql("BEGIN; UPDATE notes SET body = 'q' WHERE id = 1; COMMIT;")
            .unwrap();
        assert_eq!(body(&mut db), Cell::Text("q".into()));
        db.undo().unwrap();
        assert_eq!(body(&mut db), Cell::Text("z".into()));
    }

    #[test]
    fn semicolon_inside_a_literal_is_one_insert() {
        let tmp = Tmp::new();
        notes(&tmp.0);
        let mut db = Database::open_owned(tmp.0.clone()).unwrap();
        db.apply_sql("INSERT INTO notes(id, body) VALUES (2, 'a;b');")
            .unwrap();
        let result = db.apply_sql("SELECT body FROM notes WHERE id = 2").unwrap();
        assert_eq!(result.rows[0][0], Cell::Text("a;b".into()));
    }
}
