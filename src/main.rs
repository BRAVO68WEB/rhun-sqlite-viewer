//! Browse and edit a SQLite database in rhun.
//!
//! The database tab is an owned custom editor: a short overview. The sidebar lists the schema,
//! the bottom panel runs SQL and shows rows, and the side form edits one row. Files up to 256 MB
//! are copied into the state directory so Save and Undo can put the bytes back.

mod db;
mod sql;
mod ui;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rhun_ext::custom_editor::{PART_MAX, SAVE_MAX};
use rhun_ext::views::{ChildrenRequest, Patch, RetainedEvent, ViewOpen};
use rhun_ext::{
    run, CDoc, Color, Doc, EditorOpen, EditorSave, Ext, Feature, Handler, Invocation, Opened,
    SaveKind, View, ViewEvent,
};

use db::{Cell, ColumnInfo, Database, Page, RowChange, RowKey, RunResult, MAX_COPY};
use ui::{Overview, RowField, RowForm, DISPLAY_COLS, FORM_COLS};

fn main() {
    let features = [
        Feature::CommandArgs,
        Feature::CustomEditors,
        Feature::Views,
        Feature::ViewKeys,
        Feature::ViewTree,
        Feature::ViewTable,
        Feature::ViewForm,
    ];
    if let Err(err) = run(&features, App::default()) {
        eprintln!("rhun-sqlite-viewer: {err}");
    }
}

#[derive(Default)]
struct App {
    sessions: BTreeMap<CDoc, Session>,
    active: Option<CDoc>,
    views: Views,
    sql: String,
    sql_seq: u64,
    sql_ready: bool,
    sql_docs: BTreeMap<Doc, String>,
    row_seqs: BTreeMap<String, u64>,
}

#[derive(Default)]
struct Views {
    schema: Option<View>,
    sql: Option<View>,
    results: Option<View>,
    row: Option<View>,
}

struct Session {
    view: View,
    path: String,
    width: f32,
    height: f32,
    error: Option<String>,
    db: Option<Database>,
    readonly_note: Option<String>,
    table: Option<String>,
    order: Option<(String, bool)>,
    offset: i64,
    grid: Grid,
    page: Page,
    row: Option<RowEdit>,
    note: String,
    zones: Vec<ui::Zone>,
    show_schema: bool,
    find: String,
    find_on: bool,
    version: String,
    selected: Option<(usize, usize)>,
    col_at: usize,
    /// Original bytes when the file is not a database, so Save writes them back unchanged.
    raw_file: Option<PathBuf>,
    raw_dir: Option<PathBuf>,
    save_path: Option<PathBuf>,
    save_state: u64,
}

impl Session {
    fn new(view: View, path: String) -> Session {
        Session {
            view,
            path,
            width: 800.0,
            height: 600.0,
            error: None,
            db: None,
            readonly_note: None,
            table: None,
            order: None,
            offset: 0,
            grid: Grid::Message,
            page: Page::empty(),
            row: None,
            note: String::new(),
            zones: Vec::new(),
            show_schema: false,
            find: String::new(),
            find_on: false,
            version: String::new(),
            selected: None,
            col_at: 0,
            raw_file: None,
            raw_dir: None,
            save_path: None,
            save_state: 0,
        }
    }
}

#[derive(Clone)]
enum Grid {
    Browse,
    Query(String),
    Shown,
    Message,
}

struct RowEdit {
    table: String,
    key: RowKey,
    columns: Vec<ColumnInfo>,
    original: Vec<Cell>,
    current: Vec<Option<String>>,
    touched: Vec<bool>,
}

impl RowEdit {
    fn from_row(table: String, key: RowKey, columns: Vec<ColumnInfo>, cells: Vec<Cell>) -> RowEdit {
        let current = cells
            .iter()
            .map(|cell| match cell {
                Cell::Text(text) => Some(text.clone()),
                Cell::Null | Cell::Blob(_) => None,
            })
            .collect();
        let touched = vec![false; cells.len()];
        RowEdit {
            table,
            key,
            columns,
            original: cells,
            current,
            touched,
        }
    }

    fn blank(table: String, columns: Vec<ColumnInfo>) -> RowEdit {
        let count = columns.len();
        RowEdit {
            table,
            key: RowKey::None,
            columns,
            original: vec![Cell::Null; count],
            current: vec![None; count],
            touched: vec![false; count],
        }
    }

    fn changes(&self) -> Vec<RowChange> {
        self.touched
            .iter()
            .enumerate()
            .filter_map(|(index, touched)| {
                if !touched || matches!(self.original.get(index), Some(Cell::Blob(_))) {
                    return None;
                }
                let value = self.current.get(index).cloned().flatten();
                Some(RowChange { index, value })
            })
            .collect()
    }
}

impl Handler for App {
    fn event(&mut self, _ext: &Ext, ev: &rhun_ext::Event) {
        if ev.name == "opened" {
            if let Some(opened) = Opened::parse(ev) {
                if is_sql_path(&opened.path) {
                    self.sql_docs.insert(opened.doc, opened.path);
                }
            }
        } else if ev.name == "closed" {
            self.sql_docs.remove(&ev.num(0));
        }
    }

    fn invoke(&mut self, ext: &Ext, inv: &Invocation) {
        let result = match inv.name.as_str() {
            "run" => self.cmd_run(ext, inv),
            "refresh" => self.cmd_refresh(ext),
            "insert" => self.cmd_insert(ext),
            "delete" => self.cmd_delete(ext),
            "undo" => self.cmd_undo(ext),
            "redo" => self.cmd_redo(ext),
            _ => return,
        };
        if let Err(err) = result {
            let _ = ext.toast(&err);
            self.set_status(ext, &err);
        }
    }

    fn view(&mut self, ext: &Ext, view: View, ev: ViewEvent) {
        match ev {
            ViewEvent::Size { width, height } => {
                if let Some(cdoc) = self.cdoc_for_view(view) {
                    if let Some(session) = self.sessions.get_mut(&cdoc) {
                        session.width = width;
                        session.height = height;
                    }
                    self.redraw(ext, cdoc);
                }
            }
            ViewEvent::Metrics(_) => {
                if let Some(cdoc) = self.cdoc_for_view(view) {
                    self.redraw(ext, cdoc);
                }
            }
            ViewEvent::Focus(true) => {
                if let Some(cdoc) = self.cdoc_for_view(view) {
                    self.active = Some(cdoc);
                    self.sync_passive(ext);
                    self.redraw(ext, cdoc);
                }
            }
            ViewEvent::Click { x, y } => {
                if let Some(cdoc) = self.cdoc_for_view(view) {
                    self.hit_tab(ext, cdoc, x, y);
                }
            }
            ViewEvent::Wheel { dy, .. } => {
                if let Some(cdoc) = self.cdoc_for_view(view) {
                    self.scroll_tab(ext, cdoc, dy);
                }
            }
            _ => {}
        }
    }

    fn view_open(&mut self, ext: &Ext, open: &ViewOpen) {
        match open.name.as_str() {
            "schema" => self.views.schema = Some(open.view),
            "sql" => {
                self.views.sql = Some(open.view);
                self.sql_ready = false;
            }
            "results" => self.views.results = Some(open.view),
            "row" => self.views.row = Some(open.view),
            _ => {}
        }
        self.sync_passive(ext);
    }

    fn view_event(&mut self, ext: &Ext, view: View, ev: &RetainedEvent) {
        let result = match ev {
            RetainedEvent::Activate { id, .. } if self.views.schema == Some(view) => {
                self.activate_schema(ext, id)
            }
            RetainedEvent::Activate { id, .. } if self.views.results == Some(view) => {
                self.select_row(ext, id);
                Ok(())
            }
            RetainedEvent::Select { ids, .. } if self.views.results == Some(view) => {
                if let Some(id) = ids.first() {
                    self.select_row(ext, id);
                }
                Ok(())
            }
            RetainedEvent::Sort { col, desc, .. } if self.views.results == Some(view) => {
                self.sort_results(ext, *col, *desc)
            }
            RetainedEvent::Change {
                key, seq, value, ..
            } => {
                let text = String::from_utf8_lossy(value);
                if self.views.sql == Some(view) && key == "sql" {
                    self.sql = text.into_owned();
                    self.sql_seq = *seq;
                } else if self.views.row == Some(view) && key == "find" {
                    self.row_seqs.insert("find".into(), *seq);
                    if let Err(err) = self.set_find(ext, &text) {
                        let _ = ext.toast(&err);
                    }
                } else if self.views.row == Some(view) {
                    self.touch_field(key, *seq, &text);
                }
                Ok(())
            }
            RetainedEvent::Press { key, .. } | RetainedEvent::Submit { key, .. } => {
                if self.views.sql == Some(view) && (key == "run" || key == "sql") {
                    let sql = self.sql.clone();
                    self.run_sql(ext, sql)
                } else if self.views.row == Some(view) {
                    match key.as_str() {
                        "save" => self.save_row(ext),
                        "delete" => self.cmd_delete(ext),
                        "insert" => self.cmd_insert(ext),
                        _ => Ok(()),
                    }
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            let _ = ext.toast(&err);
            self.set_status(ext, &err);
        }
    }

    fn view_children(&mut self, _ext: &Ext, req: &ChildrenRequest) -> Result<Patch, String> {
        if self.views.schema == Some(req.view) {
            let nodes = self
                .active
                .and_then(|cdoc| self.sessions.get(&cdoc))
                .and_then(|session| session.db.as_ref())
                .map(|db| ui::schema_children(db.schema(), &req.node))
                .unwrap_or_default();
            let mut patch = Patch::new();
            patch.kids(&req.node, &nodes, false);
            return Ok(patch);
        }
        if self.views.results == Some(req.view) && req.node.is_empty() {
            return Ok(self.results_patch());
        }
        let mut patch = Patch::new();
        patch.kids(&req.node, &[], false);
        Ok(patch)
    }

    fn view_more(&mut self, _ext: &Ext, req: &ChildrenRequest) -> Result<Patch, String> {
        if self.views.results == Some(req.view) {
            if let Some(patch) = self.append_page(&req.node) {
                return Ok(patch);
            }
        }
        let mut patch = Patch::new();
        patch.add(&req.node, &[], false);
        Ok(patch)
    }

    fn editor_open(&mut self, ext: &Ext, req: &EditorOpen) -> Result<(), String> {
        let mut session = Session::new(req.view, req.path.clone());
        match open_database(ext, req) {
            Ok(Loaded::Ready(database, note)) => {
                let mut database = *database;
                if req.source == "backup" {
                    database.mark_unsaved();
                }
                let dirty = database.is_dirty();
                session.readonly_note = note;
                session.db = Some(database);
                if dirty {
                    let _ = ext.editor_dirty(req.cdoc, true);
                }
            }
            Ok(Loaded::Keep { error, file, dir }) => {
                session.error = Some(error);
                session.raw_file = Some(file);
                session.raw_dir = Some(dir);
            }
            Err(err) => session.error = Some(err),
        }
        session.version = session
            .db
            .as_ref()
            .map(|db| db.sqlite_version())
            .unwrap_or_default();
        let first = session.db.as_ref().and_then(|db| {
            db.schema()
                .tables
                .first()
                .or_else(|| db.schema().views.first())
                .map(|rel| rel.name.clone())
        });
        self.sessions.insert(req.cdoc, session);
        self.active = Some(req.cdoc);
        if let Some(name) = first {
            let _ = self.browse_name(ext, req.cdoc, name);
        } else {
            self.sync_passive(ext);
            self.redraw(ext, req.cdoc);
        }
        let _ = ext.vshow("schema", false);
        let _ = ext.vshow("sql", false);
        Ok(())
    }

    fn editor_save(&mut self, ext: &Ext, req: &EditorSave) -> Result<(bool, Vec<u8>), String> {
        if !self.sessions.contains_key(&req.cdoc) {
            return Ok((false, Vec::new()));
        }
        if req.part == 0 {
            self.prepare_save(req.cdoc)?;
            let dirty = self
                .sessions
                .get(&req.cdoc)
                .and_then(|session| session.db.as_ref())
                .map(|db| db.is_dirty())
                .unwrap_or(false);
            if matches!(req.kind, SaveKind::Backup) && dirty {
                let _ = ext.editor_dirty(req.cdoc, true);
            }
        }
        let (more, bytes, mark, stay_dirty) = self.read_save_part(req)?;
        if mark {
            if let Some(db) = self
                .sessions
                .get_mut(&req.cdoc)
                .and_then(|session| session.db.as_mut())
            {
                db.mark_saved();
            }
        }
        if stay_dirty {
            let _ = ext.editor_dirty(req.cdoc, true);
        }
        if !more {
            self.redraw(ext, req.cdoc);
            self.set_status_active(ext);
        }
        Ok((more, bytes))
    }

    fn editor_undo(&mut self, ext: &Ext, cdoc: CDoc) -> Result<(), String> {
        if self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .is_none()
        {
            return Ok(());
        }
        self.active = Some(cdoc);
        match self.undo_cdoc(ext, cdoc) {
            Err(err) if err == "Nothing to undo." => Ok(()),
            other => other,
        }
    }

    fn editor_redo(&mut self, ext: &Ext, cdoc: CDoc) -> Result<(), String> {
        if self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .is_none()
        {
            return Ok(());
        }
        self.active = Some(cdoc);
        match self.redo_cdoc(ext, cdoc) {
            Err(err) if err == "Nothing to redo." => Ok(()),
            other => other,
        }
    }

    fn editor_revert(&mut self, ext: &Ext, cdoc: CDoc) -> Result<(), String> {
        if self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .is_none()
        {
            return Ok(());
        }
        self.reload_from_disk(ext, cdoc, "Reverted.")
    }

    fn editor_changed(&mut self, ext: &Ext, cdoc: CDoc) {
        let dirty = self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .map(|db| db.is_dirty())
            .unwrap_or(false);
        if dirty {
            let _ = ext.toast("The database file changed on disk.");
            return;
        }
        if let Err(err) = self.reload_from_disk(ext, cdoc, "Reloaded.") {
            let _ = ext.toast(&err);
        }
    }

    fn editor_path(&mut self, ext: &Ext, cdoc: CDoc, path: &str) {
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.path = path.to_string();
        }
        self.redraw(ext, cdoc);
        self.set_status_active(ext);
    }

    fn editor_closed(&mut self, ext: &Ext, cdoc: CDoc) {
        if let Some(session) = self.sessions.remove(&cdoc) {
            if let Some(db) = session.db {
                db.cleanup();
            }
            if let Some(dir) = session.raw_dir {
                let _ = fs::remove_dir_all(dir);
            }
        }
        if self.active == Some(cdoc) {
            self.active = self.sessions.keys().next_back().copied();
        }
        self.sync_passive(ext);
    }
}

impl App {
    fn cmd_run(&mut self, ext: &Ext, inv: &Invocation) -> Result<(), String> {
        let sql = self.sql_from_invocation(ext, inv)?;
        self.run_sql(ext, sql)
    }

    fn cmd_refresh(&mut self, ext: &Ext) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.reload_schema()?;
        }
        self.ensure_table(cdoc);
        if matches!(
            self.sessions.get(&cdoc).map(|session| &session.grid),
            Some(Grid::Browse)
        ) && self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.table.as_ref())
            .is_some()
        {
            self.reload_browse(cdoc)?;
        }
        self.publish(ext, cdoc, true);
        Ok(())
    }

    fn cmd_insert(&mut self, ext: &Ext) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        let table = self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.table.clone())
            .ok_or("Choose a table first.")?;
        let columns = {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            if !db.writable() {
                return Err("This database is read-only.".into());
            }
            let rel = db
                .schema()
                .find(&table)
                .cloned()
                .ok_or("Choose a table first.")?;
            if rel.kind == db::RelKind::View {
                return Err("Views are read-only.".into());
            }
            rel.columns
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = Some(RowEdit::blank(table, columns));
            session.note = "New row.".into();
        }
        self.sync_row(ext);
        self.set_status_active(ext);
        let _ = ext.vshow("row", false);
        Ok(())
    }

    fn cmd_delete(&mut self, ext: &Ext) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        let (table, key) = {
            let row = self
                .sessions
                .get(&cdoc)
                .and_then(|session| session.row.as_ref())
                .ok_or("Choose a row first.")?;
            (row.table.clone(), row.key.clone())
        };
        if key == RowKey::None {
            if let Some(session) = self.sessions.get_mut(&cdoc) {
                session.row = None;
                session.note = "New row discarded.".into();
            }
            self.sync_row(ext);
            self.set_status_active(ext);
            return Ok(());
        }
        let result = {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.delete_row(&table, &key)?
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = None;
        }
        self.note_result(cdoc, &result)?;
        self.publish(ext, cdoc, true);
        let _ = ext.toast(&result.message);
        let _ = ext.vshow("results", false);
        Ok(())
    }

    fn cmd_undo(&mut self, ext: &Ext) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        self.undo_cdoc(ext, cdoc)?;
        let _ = ext.toast("Undone.");
        Ok(())
    }

    fn cmd_redo(&mut self, ext: &Ext) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        self.redo_cdoc(ext, cdoc)?;
        let _ = ext.toast("Redone.");
        Ok(())
    }

    fn run_sql(&mut self, ext: &Ext, sql: String) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        let result = {
            let session = self.session_mut(cdoc)?;
            db_mut(session)?.apply_sql(&sql)?
        };
        self.note_result(cdoc, &result)?;
        if !result.wrote {
            if let Some(session) = self.sessions.get_mut(&cdoc) {
                if matches!(session.grid, Grid::Query(_)) {
                    session.grid = Grid::Query(sql);
                }
            }
        }
        self.publish(ext, cdoc, true);
        if result.wrote {
            let _ = ext.toast(&result.message);
        }
        let _ = ext.vshow("results", false);
        Ok(())
    }

    fn undo_cdoc(&mut self, ext: &Ext, cdoc: CDoc) -> Result<(), String> {
        {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.undo()?;
        }
        self.after_history(ext, cdoc)
    }

    fn redo_cdoc(&mut self, ext: &Ext, cdoc: CDoc) -> Result<(), String> {
        let result = {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.redo()?
        };
        self.note_result(cdoc, &result)?;
        self.publish(ext, cdoc, true);
        let _ = ext.vshow("results", false);
        Ok(())
    }

    fn after_history(&mut self, ext: &Ext, cdoc: CDoc) -> Result<(), String> {
        self.ensure_table(cdoc);
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = None;
        }
        let grid = self
            .sessions
            .get(&cdoc)
            .map(|session| session.grid.clone())
            .unwrap_or(Grid::Message);
        match grid {
            Grid::Browse => {
                if self
                    .sessions
                    .get(&cdoc)
                    .and_then(|session| session.table.as_ref())
                    .is_some()
                {
                    self.reload_browse(cdoc)?;
                }
            }
            Grid::Query(sql) => {
                let result = {
                    let session = self.session_mut(cdoc)?;
                    let db = db_mut(session)?;
                    db.query(&sql)?
                };
                if let Some(session) = self.sessions.get_mut(&cdoc) {
                    session.note = result.message;
                    session.page = Page::from_query(result.columns, result.rows);
                    session.grid = Grid::Query(sql);
                }
            }
            Grid::Shown | Grid::Message => {
                let has_table = self
                    .sessions
                    .get(&cdoc)
                    .and_then(|session| session.table.clone())
                    .is_some();
                if has_table {
                    if let Some(session) = self.sessions.get_mut(&cdoc) {
                        session.grid = Grid::Browse;
                    }
                    self.reload_browse(cdoc)?;
                }
            }
        }
        self.publish(ext, cdoc, true);
        Ok(())
    }

    fn note_result(&mut self, cdoc: CDoc, result: &RunResult) -> Result<(), String> {
        self.ensure_table(cdoc);
        let table_alive = self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.table.as_ref())
            .is_some();
        if !result.columns.is_empty() {
            if let Some(session) = self.sessions.get_mut(&cdoc) {
                session.note = result.message.clone();
                session.page = Page::from_query(result.columns.clone(), result.rows.clone());
                session.grid = if result.wrote {
                    Grid::Shown
                } else {
                    Grid::Query(String::new())
                };
                session.row = None;
            }
            return Ok(());
        }
        {
            let session = self.session_mut(cdoc)?;
            session.note = result.message.clone();
            session.row = None;
            if !(result.wrote && table_alive) {
                session.grid = Grid::Message;
                session.page = Page::empty();
                return Ok(());
            }
            session.grid = Grid::Browse;
        }
        self.reload_browse(cdoc)
    }

    fn activate_schema(&mut self, ext: &Ext, id: &[u8]) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        let name = {
            let session = self.sessions.get(&cdoc).ok_or("No database is open.")?;
            let db = session.db.as_ref().ok_or_else(|| self.missing_db(cdoc))?;
            let text = String::from_utf8_lossy(id);
            if let Some(rel) = ui::activated(db.schema(), &text) {
                Some(rel.name.clone())
            } else {
                ui::activated_index(db.schema(), &text).map(|index| index.table.clone())
            }
        };
        let Some(name) = name else {
            return Ok(());
        };
        self.browse_name(ext, cdoc, name)
    }

    fn browse_name(&mut self, ext: &Ext, cdoc: CDoc, name: String) -> Result<(), String> {
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.table = Some(name);
            session.order = None;
            session.offset = 0;
            session.grid = Grid::Browse;
            session.row = None;
            session.selected = None;
            session.col_at = 0;
            session.show_schema = false;
            session.find.clear();
        }
        self.reload_browse(cdoc)?;
        self.prepare_insert(cdoc);
        self.publish(ext, cdoc, false);
        let _ = ext.vshow("results", false);
        let _ = ext.vshow("row", false);
        Ok(())
    }

    fn sort_results(&mut self, ext: &Ext, col: u32, desc: bool) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        let name = {
            let session = self.sessions.get(&cdoc).ok_or("No database is open.")?;
            if !matches!(session.grid, Grid::Browse) {
                return Ok(());
            }
            let index = col.saturating_sub(1) as usize;
            if index >= DISPLAY_COLS {
                return Ok(());
            }
            session
                .page
                .columns
                .get(index)
                .map(|column| column.name.clone())
        };
        let Some(name) = name else {
            return Ok(());
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.order = Some((name, desc));
            session.offset = 0;
        }
        self.reload_browse(cdoc)?;
        self.sync_results(ext);
        Ok(())
    }

    fn select_row(&mut self, ext: &Ext, id: &[u8]) {
        let Some(index) = row_index(id) else {
            return;
        };
        let Some(cdoc) = self.active else {
            return;
        };
        let built = {
            let Some(session) = self.sessions.get(&cdoc) else {
                return;
            };
            let writable = session.db.as_ref().map(|db| db.writable()).unwrap_or(false);
            let browsing = matches!(session.grid, Grid::Browse);
            let Some(row) = session.page.rows.get(index) else {
                return;
            };
            if !browsing || !writable || !session.page.editable || row.key == RowKey::None {
                None
            } else {
                Some(RowEdit::from_row(
                    session.table.clone().unwrap_or_default(),
                    row.key.clone(),
                    session.page.columns.clone(),
                    row.cells.clone(),
                ))
            }
        };
        // The note-only branches borrowed session immutably and also wanted to write the note.
        // Rebuild that write here when no row was produced and the page said why via a sentinel.
        if built.is_none() {
            self.explain_uneditable(cdoc, index);
        }
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            if built.is_some() {
                session.row = built;
            }
        }
        self.sync_row(ext);
        if self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.row.as_ref())
            .is_some()
        {
            let _ = ext.vshow("row", false);
        }
    }

    fn explain_uneditable(&mut self, cdoc: CDoc, index: usize) {
        let Some(session) = self.sessions.get_mut(&cdoc) else {
            return;
        };
        let writable = session.db.as_ref().map(|db| db.writable()).unwrap_or(false);
        let browsing = matches!(session.grid, Grid::Browse);
        let row = session.page.rows.get(index);
        session.row = None;
        session.note = if !browsing {
            "Query results are not editable. Choose a table to edit rows.".into()
        } else if row.is_none() {
            session.note.clone()
        } else if !writable {
            "This database is read-only. You can browse and run queries.".into()
        } else {
            "This row cannot be edited.".into()
        };
    }

    fn save_row(&mut self, ext: &Ext) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        let (table, key, changes) = {
            let row = self
                .sessions
                .get(&cdoc)
                .and_then(|session| session.row.as_ref())
                .ok_or("Choose a row first.")?;
            (row.table.clone(), row.key.clone(), row.changes())
        };
        let inserting = key == RowKey::None;
        let result = {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            if inserting {
                db.insert_row(&table, &changes)?
            } else {
                db.update_row(&table, &key, &changes)?
            }
        };
        self.note_result(cdoc, &result)?;
        if inserting {
            if let Some(session) = self.sessions.get_mut(&cdoc) {
                session.row = None;
            }
        } else if result.wrote {
            self.reselect(cdoc, &key);
        }
        self.publish(ext, cdoc, true);
        if result.wrote {
            let _ = ext.toast(&result.message);
        }
        let _ = ext.vshow("results", false);
        Ok(())
    }

    fn reselect(&mut self, cdoc: CDoc, key: &RowKey) {
        let built = {
            let Some(session) = self.sessions.get(&cdoc) else {
                return;
            };
            let Some(row) = session.page.rows.iter().find(|row| &row.key == key) else {
                return;
            };
            let table = session.table.clone().unwrap_or_default();
            RowEdit::from_row(
                table,
                row.key.clone(),
                session.page.columns.clone(),
                row.cells.clone(),
            )
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = Some(built);
        }
    }

    fn reload_from_disk(&mut self, ext: &Ext, cdoc: CDoc, note: &str) -> Result<(), String> {
        if self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .is_none()
        {
            return Ok(());
        }
        self.active = Some(cdoc);
        let readonly = self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .map(|db| !db.writable())
            .unwrap_or(false);
        if readonly {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.reload()?;
        } else {
            let bytes = read_capped(ext, cdoc)?;
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.replace_with(&bytes)?;
        }
        self.ensure_table(cdoc);
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = None;
            session.note = note.to_string();
            session.order = None;
            session.offset = 0;
        }
        if self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.table.as_ref())
            .is_some()
        {
            if let Some(session) = self.sessions.get_mut(&cdoc) {
                session.grid = Grid::Browse;
            }
            let _ = self.reload_browse(cdoc);
        } else if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.grid = Grid::Message;
            session.page = Page::empty();
        }
        let _ = ext.editor_dirty(cdoc, false);
        self.publish(ext, cdoc, false);
        Ok(())
    }

    fn prepare_save(&mut self, cdoc: CDoc) -> Result<(), String> {
        let has_db = self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .is_some();
        if !has_db {
            let path = self
                .sessions
                .get(&cdoc)
                .and_then(|session| session.raw_file.clone())
                .ok_or("No database is open.")?;
            let len = fs::metadata(&path).map_err(|err| err.to_string())?.len();
            if len > SAVE_MAX as u64 {
                return Err("The file is larger than 1 GB.".into());
            }
            let session = self.session_mut(cdoc)?;
            session.save_path = Some(path);
            session.save_state = 0;
            return Ok(());
        }
        let session = self.session_mut(cdoc)?;
        let db = db_mut(session)?;
        let path = if db.writable() {
            db.snapshot_file()?
        } else {
            let path = db.path().to_path_buf();
            let len = fs::metadata(&path).map_err(|err| err.to_string())?.len();
            if len > SAVE_MAX as u64 {
                return Err("The file is larger than 1 GB.".into());
            }
            path
        };
        let len = fs::metadata(&path).map_err(|err| err.to_string())?.len();
        if len > SAVE_MAX as u64 {
            return Err("The file is larger than 1 GB.".into());
        }
        let state = self
            .sessions
            .get(&cdoc)
            .and_then(|session| session.db.as_ref())
            .map(|db| db.state_id())
            .unwrap_or(0);
        let session = self.session_mut(cdoc)?;
        session.save_path = Some(path);
        session.save_state = state;
        Ok(())
    }

    fn read_save_part(&self, req: &EditorSave) -> Result<(bool, Vec<u8>, bool, bool), String> {
        let session = self.sessions.get(&req.cdoc).ok_or("No database is open.")?;
        let path = session.save_path.clone().ok_or("Save was not started.")?;
        let state_now = session
            .db
            .as_ref()
            .map(|db| db.state_id())
            .unwrap_or(session.save_state);
        let (more, bytes) = read_part(&path, req.part)?;
        let user_save = !matches!(req.kind, SaveKind::Backup);
        let mark = !more && user_save && state_now == session.save_state;
        let stay_dirty = !more && user_save && state_now != session.save_state;
        Ok((more, bytes, mark, stay_dirty))
    }

    fn reload_browse(&mut self, cdoc: CDoc) -> Result<(), String> {
        let (table, order, offset, filter) = {
            let session = self.sessions.get(&cdoc).ok_or("No database is open.")?;
            let table = session.table.clone().ok_or("Choose a table first.")?;
            (
                table,
                session.order.clone(),
                session.offset,
                session.find.clone(),
            )
        };
        let page = {
            let session = self.session_mut(cdoc)?;
            let db = db_mut(session)?;
            db.browse(
                &table,
                order
                    .as_ref()
                    .map(|(column, desc)| (column.as_str(), *desc)),
                offset,
                if filter.is_empty() {
                    None
                } else {
                    Some(filter.as_str())
                },
            )?
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.page = page;
            session.grid = Grid::Browse;
        }
        Ok(())
    }

    fn append_page(&mut self, parent: &[u8]) -> Option<Patch> {
        let cdoc = self.active?;
        let (table, order, offset, have, filter) = {
            let session = self.sessions.get(&cdoc)?;
            if !matches!(session.grid, Grid::Browse) || !session.page.more {
                return None;
            }
            let table = session.table.clone()?;
            let have = session.page.rows.len();
            (
                table,
                session.order.clone(),
                session.offset + have as i64,
                have,
                session.find.clone(),
            )
        };
        let page = {
            let session = self.sessions.get_mut(&cdoc)?;
            let db = session.db.as_mut()?;
            db.browse(
                &table,
                order
                    .as_ref()
                    .map(|(column, desc)| (column.as_str(), *desc)),
                offset,
                if filter.is_empty() {
                    None
                } else {
                    Some(filter.as_str())
                },
            )
            .ok()?
        };
        let more = page.more;
        let added = page.rows;
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.page.more = more;
            session.page.rows.extend(added.iter().cloned());
        }
        Some(ui::results_add(parent, &added, have, more))
    }

    fn ensure_table(&mut self, cdoc: CDoc) {
        let gone = {
            let Some(session) = self.sessions.get(&cdoc) else {
                return;
            };
            match &session.table {
                Some(name) => session
                    .db
                    .as_ref()
                    .map(|db| db.schema().find(name).is_none())
                    .unwrap_or(true),
                None => false,
            }
        };
        if !gone {
            return;
        }
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.table = None;
            session.order = None;
            session.offset = 0;
            session.page = Page::empty();
            session.grid = Grid::Message;
            session.row = None;
        }
    }

    fn touch_field(&mut self, key: &str, seq: u64, value: &str) {
        self.row_seqs.insert(key.to_string(), seq);
        let Some(index) = key
            .strip_prefix('c')
            .and_then(|text| text.parse::<usize>().ok())
        else {
            return;
        };
        let Some(cdoc) = self.active else {
            return;
        };
        let Some(row) = self
            .sessions
            .get_mut(&cdoc)
            .and_then(|session| session.row.as_mut())
        else {
            return;
        };
        if index >= row.current.len() || matches!(row.original.get(index), Some(Cell::Blob(_))) {
            return;
        }
        row.touched[index] = true;
        row.current[index] = if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        };
    }

    fn publish(&mut self, ext: &Ext, cdoc: CDoc, tell_dirty: bool) {
        if tell_dirty {
            if let Some(db) = self
                .sessions
                .get(&cdoc)
                .and_then(|session| session.db.as_ref())
            {
                let _ = ext.editor_dirty(cdoc, db.is_dirty());
            }
        }
        self.redraw(ext, cdoc);
        self.sync_passive(ext);
    }

    fn sync_passive(&mut self, ext: &Ext) {
        self.sync_schema(ext);
        self.sync_results(ext);
        self.sync_row(ext);
        self.sync_sql(ext);
        self.set_status_active(ext);
    }

    fn sync_schema(&self, ext: &Ext) {
        let Some(view) = self.views.schema else {
            return;
        };
        let patch = match self.active.and_then(|cdoc| self.sessions.get(&cdoc)) {
            Some(session) => match &session.db {
                Some(db) => ui::schema_patch(db.schema()),
                None => ui::schema_message(
                    session
                        .error
                        .as_deref()
                        .unwrap_or("Open a SQLite database."),
                ),
            },
            None => ui::schema_message("Open a SQLite database."),
        };
        apply_patch(ext, view, &patch);
    }

    fn sync_results(&self, ext: &Ext) {
        let Some(view) = self.views.results else {
            return;
        };
        apply_patch(ext, view, &self.results_patch());
    }

    fn results_patch(&self) -> Patch {
        let Some(session) = self.active.and_then(|cdoc| self.sessions.get(&cdoc)) else {
            return ui::results_message("Run SQL or choose a table.");
        };
        if session.db.is_none() {
            return ui::results_message(session.error.as_deref().unwrap_or("No database is open."));
        }
        if matches!(session.grid, Grid::Message) && session.page.rows.is_empty() {
            let note = if session.note.is_empty() {
                "Run SQL or choose a table."
            } else {
                session.note.as_str()
            };
            return ui::results_message(note);
        }
        ui::results_patch(&session.page, &session.note, sorted_mark(session))
    }

    fn sync_row(&self, ext: &Ext) {
        let Some(view) = self.views.row else {
            return;
        };
        let patch = match self.active.and_then(|cdoc| self.sessions.get(&cdoc)) {
            Some(session) => ui::row_patch(&self.row_form(session)),
            None => ui::row_patch(&RowForm {
                hint: "Open a SQLite database.".into(),
                find: None,
                fields: Vec::new(),
                save: false,
                delete: false,
                insert: false,
            }),
        };
        apply_patch(ext, view, &patch);
    }

    fn row_form(&self, session: &Session) -> RowForm {
        let writable = session.db.as_ref().map(|db| db.writable()).unwrap_or(false);
        let can_edit = session.page.editable;
        let find = session.find_on.then(|| {
            (
                session.find.clone(),
                self.row_seqs.get("find").copied().unwrap_or(0),
            )
        });
        let Some(row) = &session.row else {
            return RowForm {
                hint: if can_edit {
                    session
                        .table
                        .as_deref()
                        .map(|table| sql::insert_statement(table, &[]))
                        .unwrap_or_else(|| "Choose a table.".into())
                } else if let Some(table) = &session.table {
                    format!("SELECT * FROM {}", sql::quote_ident(table))
                } else if writable {
                    "Choose a table.".into()
                } else {
                    session
                        .readonly_note
                        .clone()
                        .unwrap_or_else(|| "Choose a table.".into())
                },
                find,
                fields: Vec::new(),
                save: false,
                delete: false,
                insert: can_edit,
            };
        };
        let hint = statement_of(session);
        let mut hint = if row.columns.len() > FORM_COLS {
            format!(
                "Showing {FORM_COLS} of {} columns. {hint}",
                row.columns.len()
            )
        } else {
            hint
        };
        if row.key == RowKey::None {
            hint.push_str("  Empty fields are left out. Clear a field to store NULL.");
        }
        let fields = row
            .columns
            .iter()
            .take(FORM_COLS)
            .enumerate()
            .map(|(index, column)| {
                field_of(
                    row,
                    index,
                    column,
                    self.row_seqs
                        .get(&format!("c{index}"))
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();
        RowForm {
            hint,
            find,
            fields,
            save: can_edit,
            delete: can_edit && row.key != RowKey::None,
            insert: can_edit,
        }
    }

    fn hit_tab(&mut self, ext: &Ext, cdoc: CDoc, x: f32, y: f32) {
        let hot = self
            .sessions
            .get(&cdoc)
            .and_then(|session| ui::hit(&session.zones, x, y));
        let Some(hot) = hot else {
            return;
        };
        match hot {
            ui::Hot::Schema => {
                if let Some(session) = self.sessions.get_mut(&cdoc) {
                    session.show_schema = !session.show_schema;
                }
                self.redraw(ext, cdoc);
            }
            ui::Hot::Query => {
                let _ = ext.vshow("sql", true);
            }
            ui::Hot::Find => {
                if let Some(session) = self.sessions.get_mut(&cdoc) {
                    session.find_on = !session.find_on;
                    session.show_schema = false;
                }
                self.sync_row(ext);
                self.redraw(ext, cdoc);
                let _ = ext.vshow("row", true);
            }
            ui::Hot::Insert => {
                self.prepare_insert(cdoc);
                self.sync_row(ext);
                self.redraw(ext, cdoc);
                let _ = ext.vshow("row", true);
            }
            ui::Hot::Header(index) => {
                let name = self.sessions.get(&cdoc).and_then(|session| {
                    session
                        .page
                        .columns
                        .get(index)
                        .map(|column| column.name.clone())
                });
                let Some(name) = name else {
                    return;
                };
                if let Some(session) = self.sessions.get_mut(&cdoc) {
                    let desc = session
                        .order
                        .as_ref()
                        .is_some_and(|(current, descending)| current == &name && !descending);
                    session.order = Some((name, desc));
                    session.offset = 0;
                    session.show_schema = false;
                    session.selected = None;
                }
                if self.reload_browse(cdoc).is_ok() {
                    self.publish(ext, cdoc, false);
                }
            }
            ui::Hot::Cell(row, col) => self.select_cell(ext, cdoc, row, col),
            ui::Hot::Prev => self.page_tab(ext, cdoc, -1),
            ui::Hot::Next => self.page_tab(ext, cdoc, 1),
            ui::Hot::Cols(delta) => {
                if let Some(session) = self.sessions.get_mut(&cdoc) {
                    if delta < 0 {
                        session.col_at = session.col_at.saturating_sub(1);
                    } else {
                        session.col_at = session.col_at.saturating_add(delta as usize);
                    }
                    session.show_schema = false;
                }
                self.redraw(ext, cdoc);
            }
        }
    }

    fn select_cell(&mut self, ext: &Ext, cdoc: CDoc, row_index: usize, col: usize) {
        let built = {
            let Some(session) = self.sessions.get(&cdoc) else {
                return;
            };
            let Some(row) = session.page.rows.get(row_index) else {
                return;
            };
            RowEdit::from_row(
                session.table.clone().unwrap_or_default(),
                row.key.clone(),
                session.page.columns.clone(),
                row.cells.clone(),
            )
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = Some(built);
            session.selected = Some((row_index, col));
            session.show_schema = false;
        }
        self.sync_row(ext);
        self.redraw(ext, cdoc);
        let _ = ext.vshow("row", true);
    }

    fn prepare_insert(&mut self, cdoc: CDoc) {
        let built = {
            let Some(session) = self.sessions.get(&cdoc) else {
                return;
            };
            if !session.page.editable {
                return;
            }
            let table = session.table.clone().unwrap_or_default();
            Some(RowEdit::blank(table, session.page.columns.clone()))
        };
        let Some(row) = built else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.row = Some(row);
            session.selected = None;
            session.show_schema = false;
        }
    }

    fn set_find(&mut self, ext: &Ext, text: &str) -> Result<(), String> {
        let cdoc = self.active_cdoc()?;
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.find = text.to_string();
            session.offset = 0;
            session.selected = None;
        }
        self.reload_browse(cdoc)?;
        self.publish(ext, cdoc, false);
        Ok(())
    }

    fn scroll_tab(&mut self, ext: &Ext, cdoc: CDoc, dy: f32) {
        if dy == 0.0 {
            return;
        }
        let step = self
            .sessions
            .get(&cdoc)
            .map(|session| ui::grid_rows(session.height).max(1))
            .unwrap_or(1);
        self.shift_offset(ext, cdoc, if dy > 0.0 { step } else { -step });
    }

    fn page_tab(&mut self, ext: &Ext, cdoc: CDoc, direction: i64) {
        let step = self
            .sessions
            .get(&cdoc)
            .map(|session| ui::grid_rows(session.height).max(1))
            .unwrap_or(1);
        self.shift_offset(ext, cdoc, direction * step);
    }

    fn shift_offset(&mut self, ext: &Ext, cdoc: CDoc, delta: i64) {
        let next = {
            let Some(session) = self.sessions.get(&cdoc) else {
                return;
            };
            if session.show_schema {
                return;
            }
            if delta > 0
                && !session.page.more
                && ui::grid_rows(session.height) >= session.page.rows.len() as i64
            {
                return;
            }
            let next = session.offset.saturating_add(delta).max(0);
            if next == session.offset {
                return;
            }
            next
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.offset = next;
            session.selected = None;
        }
        if self.reload_browse(cdoc).is_ok() {
            self.publish(ext, cdoc, false);
        }
    }

    fn sync_sql(&mut self, ext: &Ext) {
        let Some(view) = self.views.sql else {
            return;
        };
        if self.sql_ready {
            return;
        }
        let status = self.status_text();
        let patch = ui::sql_form(&self.sql, &status);
        if ext.patch(view, &patch).is_ok() {
            self.sql_ready = true;
            self.sql_seq = 0;
        }
    }

    fn redraw(&mut self, ext: &Ext, cdoc: CDoc) {
        let Some(session) = self.sessions.get(&cdoc) else {
            return;
        };
        let view = session.view;
        let (picture, zones) = if session.error.is_none() && session.table.is_some() {
            let file = file_name(&session.path);
            let schema_text = if session.show_schema {
                session
                    .db
                    .as_ref()
                    .and_then(|db| session.table.as_deref().map(|name| db.schema_script(name)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let statement = statement_of(session);
            let sheet = ui::Sheet {
                file: &file,
                version: &session.version,
                table: session.table.as_deref(),
                columns: &session.page.columns,
                rows: &session.page.rows,
                offset: session.offset,
                more: session.page.more,
                schema: session.show_schema,
                schema_text: &schema_text,
                statement: &statement,
                selected: session.selected,
                find_on: session.find_on,
                readonly: session.db.as_ref().map(|db| !db.writable()).unwrap_or(true),
                col_at: session.col_at,
            };
            ui::spreadsheet(session.width, session.height, &sheet)
        } else {
            (
                ui::overview(session.width, session.height, &overview_of(session)),
                Vec::new(),
            )
        };
        if let Some(session) = self.sessions.get_mut(&cdoc) {
            session.zones = zones;
        }
        if let Err(err) = ext.draw(view, &picture) {
            eprintln!("rhun-sqlite-viewer: draw: {err}");
        }
    }

    fn set_status_active(&self, ext: &Ext) {
        let text = self.status_text();
        self.set_status(ext, &text);
    }

    fn set_status(&self, ext: &Ext, text: &str) {
        let Some(view) = self.views.sql else {
            return;
        };
        if !self.sql_ready {
            return;
        }
        let mut patch = Patch::new();
        patch.field(&ui::status_field(text));
        apply_patch(ext, view, &patch);
    }

    fn status_text(&self) -> String {
        let Some(session) = self.active.and_then(|cdoc| self.sessions.get(&cdoc)) else {
            return "Open a SQLite database, or write SQL here.".into();
        };
        if let Some(err) = &session.error {
            return err.clone();
        }
        let name = file_name(&session.path);
        if session.note.is_empty() {
            name
        } else {
            format!("{name} - {}", session.note)
        }
    }

    fn sql_from_invocation(&self, ext: &Ext, inv: &Invocation) -> Result<String, String> {
        if self.sql_docs.contains_key(&inv.doc) {
            let (bytes, _) = ext.text(inv.doc).map_err(|err| err.to_string())?;
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        Ok(self.sql.clone())
    }

    fn active_cdoc(&self) -> Result<CDoc, String> {
        self.active
            .ok_or_else(|| "Open a SQLite database first.".to_string())
    }

    fn missing_db(&self, cdoc: CDoc) -> String {
        self.sessions
            .get(&cdoc)
            .and_then(|session| session.error.clone())
            .unwrap_or_else(|| "Open a SQLite database first.".to_string())
    }

    fn session_mut(&mut self, cdoc: CDoc) -> Result<&mut Session, String> {
        self.sessions
            .get_mut(&cdoc)
            .ok_or_else(|| "No database is open.".to_string())
    }

    fn cdoc_for_view(&self, view: View) -> Option<CDoc> {
        self.sessions
            .iter()
            .find_map(|(cdoc, session)| (session.view == view).then_some(*cdoc))
    }
}

fn db_mut(session: &mut Session) -> Result<&mut Database, String> {
    if session.db.is_none() {
        return Err(session
            .error
            .clone()
            .unwrap_or_else(|| "Open a SQLite database first.".to_string()));
    }
    Ok(session.db.as_mut().unwrap())
}

fn field_of(row: &RowEdit, index: usize, column: &ColumnInfo, seq: u64) -> RowField {
    let label = column_label(column);
    match row.original.get(index) {
        Some(Cell::Blob(len)) => RowField {
            label,
            value: String::new(),
            placeholder: format!("blob ({len} bytes)"),
            seq,
            disabled: true,
        },
        _ => {
            let current = row.current.get(index).cloned().flatten();
            let placeholder = if current.is_none() {
                "NULL".into()
            } else {
                String::new()
            };
            RowField {
                label,
                value: current.unwrap_or_default(),
                placeholder,
                seq,
                disabled: false,
            }
        }
    }
}

fn statement_of(session: &Session) -> String {
    let Some(table) = session.table.as_deref() else {
        return "Choose a table.".into();
    };
    let Some(row) = &session.row else {
        return format!("SELECT * FROM {}", sql::quote_ident(table));
    };
    if row.key == RowKey::None {
        let names: Vec<String> = row
            .columns
            .iter()
            .take(FORM_COLS)
            .map(|column| column.name.clone())
            .collect();
        return sql::insert_statement(table, &names);
    }
    let index = session.selected.map(|(_, col)| col).unwrap_or(0);
    let column = row
        .columns
        .get(index)
        .map(|column| column.name.as_str())
        .unwrap_or("column");
    sql::update_statement(table, column, &db::predicate(&row.key))
}

fn column_label(column: &ColumnInfo) -> String {
    let mut label = column.name.clone();
    if !column.ty.is_empty() {
        label.push_str(" · ");
        label.push_str(&column.ty);
    }
    if column.notnull {
        label.push_str(" · required");
    }
    label
}

fn sorted_mark(session: &Session) -> Option<(u32, bool)> {
    if !matches!(session.grid, Grid::Browse) {
        return None;
    }
    let (name, desc) = session.order.as_ref()?;
    let index = session
        .page
        .columns
        .iter()
        .take(DISPLAY_COLS)
        .position(|column| &column.name == name)?;
    Some((index as u32 + 1, *desc))
}

fn overview_of(session: &Session) -> Overview {
    let title = file_name(&session.path);
    if let Some(err) = &session.error {
        return Overview {
            title,
            lines: vec![err.clone()],
            hint: "This file did not open as a SQLite database.".into(),
            bar: Color::Error,
        };
    }
    let Some(db) = &session.db else {
        return Overview {
            title,
            lines: vec!["No database is open.".into()],
            hint: "This file did not open as a SQLite database.".into(),
            bar: Color::Error,
        };
    };
    let schema = db.schema();
    let state = if !db.writable() {
        "Read-only"
    } else if db.is_dirty() {
        "Unsaved changes"
    } else {
        "Saved"
    };
    let mut lines = vec![
        format!("{state} · {}", ui::format_bytes(db.file_len())),
        format!(
            "{} tables · {} views · {} indexes",
            schema.tables.len(),
            schema.views.len(),
            schema.indexes.len()
        ),
    ];
    let names: Vec<&str> = schema
        .tables
        .iter()
        .take(8)
        .map(|rel| rel.name.as_str())
        .collect();
    if !names.is_empty() {
        lines.push(names.join(", "));
    }
    let hint = if db.writable() {
        "Schema is in the sidebar. SQL and results are in the bottom panel. Choose a row to edit it."
            .to_string()
    } else {
        session
            .readonly_note
            .clone()
            .unwrap_or_else(|| "This database is read-only. You can browse and run queries.".into())
    };
    Overview {
        title,
        lines,
        hint,
        bar: if db.writable() {
            Color::Accent
        } else {
            Color::Warning
        },
    }
}

fn apply_patch(ext: &Ext, view: View, patch: &Patch) {
    if let Err(err) = ext.patch(view, patch) {
        eprintln!("rhun-sqlite-viewer: patch: {err}");
    }
}

enum Loaded {
    Ready(Box<Database>, Option<String>),
    /// The bytes were copied, and SQLite refused them. Save streams `file` back.
    Keep {
        error: String,
        file: PathBuf,
        dir: PathBuf,
    },
}

fn open_database(ext: &Ext, req: &EditorOpen) -> Result<Loaded, String> {
    let backup = req.source == "backup";
    let src = PathBuf::from(&req.path);
    if !backup {
        if let Some(len) = fs::metadata(&src).ok().map(|meta| meta.len()) {
            if len.saturating_add(db::wal_len(&src)) > MAX_COPY {
                let database = Database::open_readonly(src)?;
                return Ok(Loaded::Ready(
                    Box::new(database),
                    Some(
                        "This database is read-only because it is larger than 256 MB. You can browse and run queries."
                            .into(),
                    ),
                ));
            }
        }
    }
    let work = state_root().join("sqlite").join(req.cdoc.to_string());
    if work.exists() {
        let _ = fs::remove_dir_all(&work);
    }
    fs::create_dir_all(&work).map_err(|err| err.to_string())?;
    let dest = work.join("db.sqlite");
    if let Err(err) = stream_to(ext, req.cdoc, &dest) {
        let _ = fs::remove_dir_all(&work);
        if !backup && src.is_file() {
            if let Ok(database) = Database::open_readonly(src) {
                return Ok(Loaded::Ready(
                    Box::new(database),
                    Some(
                        "Opened read-only because the private copy failed. You can browse and run queries."
                            .into(),
                    ),
                ));
            }
        }
        return Err(err);
    }
    if !backup {
        let _ = db::copy_sidecars(&src, &dest, MAX_COPY);
    }
    match Database::open_owned(work.clone()) {
        Ok(database) => Ok(Loaded::Ready(Box::new(database), None)),
        Err(err) => Ok(Loaded::Keep {
            error: err,
            file: dest,
            dir: work,
        }),
    }
}

fn stream_to(ext: &Ext, cdoc: CDoc, dest: &Path) -> Result<(), String> {
    let mut file = File::create(dest).map_err(|err| err.to_string())?;
    let mut offset = 0u64;
    loop {
        let part = ext
            .editor_read(cdoc, offset, PART_MAX)
            .map_err(|err| err.text)?;
        if part.is_empty() {
            break;
        }
        offset = offset.saturating_add(part.len() as u64);
        if offset > MAX_COPY {
            return Err("The database is larger than 256 MB.".into());
        }
        file.write_all(&part).map_err(|err| err.to_string())?;
    }
    file.flush().map_err(|err| err.to_string())?;
    Ok(())
}

fn read_capped(ext: &Ext, cdoc: CDoc) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    loop {
        let part = ext
            .editor_read(cdoc, data.len() as u64, PART_MAX)
            .map_err(|err| err.text)?;
        if part.is_empty() {
            break;
        }
        if data.len() as u64 + part.len() as u64 > MAX_COPY {
            return Err("The file is larger than 256 MB.".into());
        }
        data.extend_from_slice(&part);
    }
    Ok(data)
}

fn read_part(path: &Path, part: u64) -> Result<(bool, Vec<u8>), String> {
    let mut file = File::open(path).map_err(|err| err.to_string())?;
    let len = file.metadata().map_err(|err| err.to_string())?.len();
    let size = PART_MAX as u64;
    let start = part.saturating_mul(size);
    if start >= len {
        return Ok((false, Vec::new()));
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|err| err.to_string())?;
    let want = size.min(len - start) as usize;
    let mut buf = vec![0u8; want];
    file.read_exact(&mut buf).map_err(|err| err.to_string())?;
    Ok((start + (want as u64) < len, buf))
}

fn state_root() -> PathBuf {
    match std::env::var("RHUN_STATE") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => std::env::temp_dir().join("rhun-sqlite-viewer"),
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string())
}

fn is_sql_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".sql", ".psql", ".ddl"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

fn row_index(id: &[u8]) -> Option<usize> {
    std::str::from_utf8(id)
        .ok()?
        .strip_prefix("r:")?
        .parse()
        .ok()
}
