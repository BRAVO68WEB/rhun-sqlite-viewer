//! Patches and the overview picture. rhun draws the tree, the table, and the forms.

use rhun_ext::picture::Align;
use rhun_ext::views::{Column, Field, FieldType, Icon, Kids, Node, Patch, State, ViewColor};
use rhun_ext::{Color, Picture, TextSize};

use crate::db::{Cell, IndexInfo, Page, PageRow, RelKind, Relation, Schema};

/// Columns drawn in the results table. The row form shows more.
pub const DISPLAY_COLS: usize = 16;
/// Columns offered in the row form.
pub const FORM_COLS: usize = 60;

/// Text for one results cell.
pub fn grid_text(cell: &Cell) -> String {
    let raw = match cell {
        Cell::Null => return "NULL".into(),
        Cell::Blob(len) => return format!("blob ({len} bytes)"),
        Cell::Text(text) if text.is_empty() => return "(empty)".into(),
        Cell::Text(text) => text,
    };
    let flat: String = raw
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            _ => ch,
        })
        .collect();
    let mut chars = flat.chars();
    let short: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        let mut cut: String = short.chars().take(199).collect();
        cut.push('…');
        cut
    } else {
        short
    }
}

pub fn format_bytes(len: u64) -> String {
    if len < 1024 {
        format!("{len} bytes")
    } else if len < 1024 * 1024 {
        format!("{:.1} KB", len as f64 / 1024.0)
    } else {
        format!("{:.1} MB", len as f64 / (1024.0 * 1024.0))
    }
}

/// The relation a schema click names, when it is a table or a view.
pub fn activated<'a>(schema: &'a Schema, id: &str) -> Option<&'a Relation> {
    let (prefix, hashed, rest) = split_node(id)?;
    let rels: Vec<&Relation> = match prefix {
        "t" => schema.tables.iter().collect(),
        "v" => schema.views.iter().collect(),
        _ => return None,
    };
    if hashed {
        rels.into_iter()
            .find(|rel| format!("{:016x}", fnv64(rel.name.as_bytes())) == rest)
    } else {
        let name = hex_decode(rest)?;
        rels.into_iter().find(|rel| rel.name == name)
    }
}

/// The index a schema click names.
pub fn activated_index<'a>(schema: &'a Schema, id: &str) -> Option<&'a IndexInfo> {
    let (prefix, hashed, rest) = split_node(id)?;
    if prefix != "x" {
        return None;
    }
    if hashed {
        schema
            .indexes
            .iter()
            .find(|index| format!("{:016x}", fnv64(index.name.as_bytes())) == rest)
    } else {
        let name = hex_decode(rest)?;
        schema.indexes.iter().find(|index| index.name == name)
    }
}

pub fn schema_patch(schema: &Schema) -> Patch {
    let mut patch = Patch::new();
    patch.state(State::Ready);
    let roots = schema_children(schema, b"");
    patch.kids(b"", &roots, false);
    if !schema.indexes.is_empty() {
        let children = schema_children(schema, b"indexes");
        patch.kids(b"indexes", &children, false);
    }
    for rel in schema.tables.iter().chain(schema.views.iter()) {
        let id = relation_id(rel);
        let children = schema_children(schema, id.as_bytes());
        patch.kids(id.as_bytes(), &children, false);
    }
    patch
}

pub fn schema_children(schema: &Schema, parent: &[u8]) -> Vec<Node> {
    let parent = String::from_utf8_lossy(parent);
    if parent.is_empty() {
        let mut nodes: Vec<Node> = schema
            .tables
            .iter()
            .chain(schema.views.iter())
            .map(relation_node)
            .collect();
        if !schema.indexes.is_empty() {
            nodes.push(
                Node::new("indexes", "Indexes")
                    .kids(Kids::Closed)
                    .icon(Icon::named("search"), Some(ViewColor::Muted)),
            );
        }
        return nodes;
    }
    if parent == "indexes" {
        return schema.indexes.iter().map(index_node).collect();
    }
    let Some(rel) = activated(schema, &parent) else {
        return Vec::new();
    };
    rel.columns
        .iter()
        .enumerate()
        .map(|(index, column)| column_node(&rel.name, index, column))
        .collect()
}

pub fn schema_message(note: &str) -> Patch {
    let mut patch = Patch::new();
    patch.state(State::Empty(note.to_string(), Vec::new()));
    patch
}

pub fn results_patch(page: &Page, note: &str, sorted: Option<(u32, bool)>) -> Patch {
    let mut patch = Patch::new();
    patch.state(State::Ready);
    patch.message(&column_note(&page.columns, note));
    patch.columns(&display_columns(&page.columns));
    patch.sorted(sorted);
    let nodes = row_nodes(&page.rows, 0);
    patch.kids(b"", &nodes, page.more);
    patch
}

pub fn results_add(parent: &[u8], rows: &[PageRow], id_base: usize, more: bool) -> Patch {
    let mut patch = Patch::new();
    patch.add(parent, &row_nodes(rows, id_base), more);
    patch
}

pub fn results_message(note: &str) -> Patch {
    let mut patch = Patch::new();
    patch.state(State::Empty(note.to_string(), Vec::new()));
    patch
}

/// One field of the row form.
pub struct RowField {
    pub label: String,
    pub value: String,
    pub placeholder: String,
    pub seq: u64,
    pub disabled: bool,
}

/// The side form for the selected row.
pub struct RowForm {
    pub hint: String,
    /// Find box: the text and the field's user sequence.
    pub find: Option<(String, u64)>,
    pub fields: Vec<RowField>,
    pub save: bool,
    pub delete: bool,
    pub insert: bool,
}

pub fn row_patch(form: &RowForm) -> Patch {
    let mut fields = Vec::new();
    let mut row = 1u32;
    if let Some((text, seq)) = &form.find {
        fields.push(
            Field::new("find", FieldType::Text, row, 12, "Find")
                .value(Some(*seq), text)
                .placeholder("Find in this table"),
        );
        row += 1;
    }
    fields.push(Field::new("hint", FieldType::Label, row, 12, "Statement").value(None, &form.hint));
    row += 1;
    let shown = form.fields.len().min(FORM_COLS);
    let first_field = row;
    for (index, field) in form.fields.iter().take(FORM_COLS).enumerate() {
        let mut item = Field::new(
            &format!("c{index}"),
            FieldType::Text,
            first_field + index as u32,
            12,
            &field.label,
        )
        .value(Some(field.seq), &field.value);
        if !field.placeholder.is_empty() {
            item = item.placeholder(&field.placeholder);
        }
        if field.disabled {
            item = item.disabled();
        }
        fields.push(item);
    }
    let row = first_field + shown as u32;
    if form.save {
        fields.push(Field::new("save", FieldType::Button, row, 4, "Commit").primary());
    }
    if form.delete {
        fields.push(Field::new(
            "delete",
            FieldType::Button,
            row,
            4,
            "Delete row",
        ));
    }
    if form.insert {
        fields.push(Field::new("insert", FieldType::Button, row, 4, "New row"));
    }
    let mut patch = Patch::new();
    patch.state(State::Ready);
    patch.form(&fields);
    patch
}

pub fn sql_form(sql: &str, status: &str) -> Patch {
    let mut patch = Patch::new();
    patch.state(State::Ready);
    patch.form(&[
        Field::new("sql", FieldType::Area, 1, 12, "SQL")
            .lines(8)
            .value(None, sql),
        Field::new("run", FieldType::Button, 2, 12, "Run").primary(),
        status_field(status),
    ]);
    patch
}

pub fn status_field(status: &str) -> Field {
    Field::new("status", FieldType::Label, 3, 12, "Status").value(None, status)
}

/// What a click in the database tab does.
#[derive(Clone, Debug, PartialEq)]
pub enum Hot {
    Schema,
    Query,
    Find,
    Insert,
    Header(usize),
    Cell(usize, usize),
    Prev,
    Next,
    Cols(i32),
}

/// A clickable rectangle in the database tab.
#[derive(Clone, Debug)]
pub struct Zone {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub hot: Hot,
}

pub fn hit(zones: &[Zone], x: f32, y: f32) -> Option<Hot> {
    zones.iter().rev().find_map(|zone| {
        if x >= zone.x && x < zone.x + zone.w && y >= zone.y && y < zone.y + zone.h {
            Some(zone.hot.clone())
        } else {
            None
        }
    })
}

/// How many data rows fit under the toolbar and above the statement bar.
pub fn grid_rows(height: f32) -> i64 {
    let body = height - 36.0 - 46.0 - 52.0;
    (body / 22.0).floor().max(1.0) as i64
}

/// The database tab: `SELECT * FROM` a table, a typed grid, and the statement under it.
pub struct Sheet<'a> {
    pub file: &'a str,
    pub version: &'a str,
    pub table: Option<&'a str>,
    pub columns: &'a [crate::db::ColumnInfo],
    pub rows: &'a [PageRow],
    pub offset: i64,
    pub more: bool,
    pub schema: bool,
    pub schema_text: &'a str,
    pub statement: &'a str,
    pub selected: Option<(usize, usize)>,
    pub find_on: bool,
    pub readonly: bool,
    pub col_at: usize,
}

pub fn spreadsheet(width: f32, height: f32, sheet: &Sheet<'_>) -> (Picture, Vec<Zone>) {
    let width = if width.is_finite() && width > 1.0 {
        width
    } else {
        800.0
    };
    let height = if height.is_finite() && height > 1.0 {
        height
    } else {
        600.0
    };
    let mut picture = Picture::new();
    let mut zones = Vec::new();
    picture.rect(0.0, 0.0, width, height, Color::Bg);
    picture.rect(0.0, 0.0, width, 3.0, Color::Accent);
    draw_toolbar(&mut picture, &mut zones, width, sheet);
    if sheet.schema {
        draw_schema(&mut picture, width, height, sheet.schema_text);
    } else {
        draw_grid(&mut picture, &mut zones, width, height, sheet);
    }
    draw_footer(&mut picture, width, height, sheet);
    (picture, zones)
}

fn draw_toolbar(picture: &mut Picture, zones: &mut Vec<Zone>, width: f32, sheet: &Sheet<'_>) {
    picture.text(
        12.0,
        10.0,
        TextSize::Small,
        Align::Left,
        Color::Accent,
        "SELECT *",
    );
    picture.text(72.0, 10.0, TextSize::Small, Align::Left, Color::Fg, "FROM");
    let table = sheet.table.unwrap_or("—");
    let name = fit(table, 180.0);
    let chip = name.chars().count() as f32 * 7.0 + 16.0;
    picture.rrect(112.0, 6.0, chip, 22.0, 4.0, Color::Panel);
    picture.text(
        120.0,
        10.0,
        TextSize::Small,
        Align::Left,
        Color::Accent,
        &name,
    );
    let mut x = 112.0 + chip + 12.0;
    x = tool(picture, zones, x, "Schema", Hot::Schema, sheet.schema);
    x = tool(picture, zones, x, "Query", Hot::Query, false);
    x = tool(picture, zones, x, "Find", Hot::Find, sheet.find_on);
    if !sheet.readonly {
        tool(picture, zones, x, "Insert", Hot::Insert, false);
    }
    let version = format!("{}   sqlite {}", sheet.file, sheet.version);
    picture.text(
        width - 12.0,
        10.0,
        TextSize::Small,
        Align::Right,
        Color::Muted,
        &fit(&version, 220.0),
    );
}

fn tool(
    picture: &mut Picture,
    zones: &mut Vec<Zone>,
    x: f32,
    label: &str,
    hot: Hot,
    on: bool,
) -> f32 {
    let w = label.len() as f32 * 7.0 + 16.0;
    picture.rrect(x, 6.0, w, 22.0, 4.0, Color::Panel);
    if on {
        picture.rect(x, 26.0, w, 2.0, Color::Accent);
    }
    picture.text(
        x + 8.0,
        10.0,
        TextSize::Small,
        Align::Left,
        Color::Fg,
        label,
    );
    zones.push(Zone {
        x,
        y: 6.0,
        w,
        h: 22.0,
        hot,
    });
    x + w + 8.0
}

fn draw_schema(picture: &mut Picture, width: f32, height: f32, text: &str) {
    let mut y = 44.0;
    let limit = height - 60.0;
    if text.is_empty() {
        picture.text(
            16.0,
            y,
            TextSize::Normal,
            Align::Left,
            Color::Muted,
            "This table has no stored schema.",
        );
        return;
    }
    for line in text.lines() {
        if y > limit {
            break;
        }
        picture.text(
            16.0,
            y,
            TextSize::Mono,
            Align::Left,
            Color::Fg,
            &fit(line, width - 32.0),
        );
        y += 18.0;
    }
}

fn draw_grid(
    picture: &mut Picture,
    zones: &mut Vec<Zone>,
    width: f32,
    height: f32,
    sheet: &Sheet<'_>,
) {
    let gutter = 48.0;
    let top = 40.0;
    let header = 40.0;
    let footer = 52.0;
    let avail = (width - gutter - 16.0).max(80.0);
    let total = sheet.columns.len();
    let shown = ((avail / 120.0).floor() as usize)
        .clamp(1, total.max(1))
        .min(DISPLAY_COLS);
    let shown = shown.min(total.max(1));
    let start = if total == 0 {
        0
    } else {
        sheet.col_at.min(total.saturating_sub(shown))
    };
    let col_w = if shown == 0 {
        avail
    } else {
        avail / shown as f32
    };
    picture.rect(8.0, top, width - 16.0, header, Color::Panel);
    picture.text(
        12.0,
        top + 12.0,
        TextSize::Small,
        Align::Left,
        Color::Muted,
        "#",
    );
    for index in 0..shown {
        let column = match sheet.columns.get(start + index) {
            Some(column) => column,
            None => break,
        };
        let x = gutter + index as f32 * col_w;
        picture.text(
            x + 6.0,
            top + 4.0,
            TextSize::Small,
            Align::Left,
            Color::Fg,
            &fit(&column.name, col_w - 10.0),
        );
        picture.text(
            x + 6.0,
            top + 20.0,
            TextSize::Small,
            Align::Left,
            Color::Muted,
            &fit(&column_type_line(column), col_w - 10.0),
        );
        zones.push(Zone {
            x,
            y: top,
            w: col_w,
            h: header,
            hot: Hot::Header(start + index),
        });
    }
    if start > 0 {
        zones.push(Zone {
            x: width - 36.0,
            y: top,
            w: 16.0,
            h: header,
            hot: Hot::Cols(-1),
        });
        picture.text(
            width - 28.0,
            top + 12.0,
            TextSize::Small,
            Align::Left,
            Color::Accent,
            "<",
        );
    }
    if start + shown < total {
        zones.push(Zone {
            x: width - 18.0,
            y: top,
            w: 16.0,
            h: header,
            hot: Hot::Cols(1),
        });
        picture.text(
            width - 14.0,
            top + 12.0,
            TextSize::Small,
            Align::Left,
            Color::Accent,
            ">",
        );
    }
    let capacity = grid_rows(height).max(1) as usize;
    let body_top = top + header;
    if sheet.rows.is_empty() {
        picture.text(
            gutter + 6.0,
            body_top + 8.0,
            TextSize::Normal,
            Align::Left,
            Color::Muted,
            "No rows.",
        );
        return;
    }
    for (index, row) in sheet.rows.iter().take(capacity).enumerate() {
        let y = body_top + index as f32 * 22.0;
        let stripe = if index % 2 == 0 {
            Color::Bg
        } else {
            Color::Panel
        };
        picture.rect(8.0, y, width - 16.0, 22.0, stripe);
        picture.text(
            12.0,
            y + 4.0,
            TextSize::Small,
            Align::Left,
            Color::Muted,
            &format!("{}", sheet.offset + index as i64 + 1),
        );
        for col in 0..shown {
            let abs = start + col;
            let x = gutter + col as f32 * col_w;
            let selected = sheet.selected == Some((index, abs));
            if selected {
                picture.rect(x, y, 3.0, 22.0, Color::Accent);
            }
            let text = row
                .cells
                .get(abs)
                .map(grid_text)
                .unwrap_or_default();
            picture.text(
                x + 6.0,
                y + 4.0,
                TextSize::Small,
                Align::Left,
                cell_color(row.cells.get(abs)),
                &fit(&text, col_w - 12.0),
            );
            zones.push(Zone {
                x,
                y,
                w: col_w,
                h: 22.0,
                hot: Hot::Cell(index, abs),
            });
        }
    }
    if sheet.offset > 0 {
        let y = height - footer - 18.0;
        picture.text(10.0, y, TextSize::Small, Align::Left, Color::Accent, "prev");
        zones.push(Zone {
            x: 8.0,
            y,
            w: 32.0,
            h: 16.0,
            hot: Hot::Prev,
        });
    }
    if sheet.more {
        let y = height - footer - 18.0;
        picture.text(46.0, y, TextSize::Small, Align::Left, Color::Accent, "more");
        zones.push(Zone {
            x: 44.0,
            y,
            w: 36.0,
            h: 16.0,
            hot: Hot::Next,
        });
    }
}

fn draw_footer(picture: &mut Picture, width: f32, height: f32, sheet: &Sheet<'_>) {
    let y = height - 48.0;
    picture.rect(0.0, y, width, 48.0, Color::Panel);
    let line = if sheet.statement.is_empty() {
        "Choose a table."
    } else {
        sheet.statement
    };
    let keyword = line.split_whitespace().next().unwrap_or("");
    picture.text(
        12.0,
        y + 6.0,
        TextSize::Small,
        Align::Left,
        Color::Accent,
        keyword,
    );
    let rest = line.get(keyword.len()..).unwrap_or("");
    picture.text(
        12.0,
        y + 24.0,
        TextSize::Mono,
        Align::Left,
        Color::Fg,
        &fit(rest.trim_start(), width - 24.0),
    );
}

fn column_type_line(column: &crate::db::ColumnInfo) -> String {
    let mut line = if column.ty.is_empty() {
        "ANY".into()
    } else {
        column.ty.clone()
    };
    if column.pk > 0 {
        line.push_str(" PRIMARY KEY");
    }
    if column.notnull {
        line.push_str(" NOT NULL");
    }
    line
}

fn cell_color(cell: Option<&Cell>) -> Color {
    match cell {
        None | Some(Cell::Null) => Color::Muted,
        Some(Cell::Blob(_)) => Color::Warning,
        Some(Cell::Text(text)) if looks_number(text) => Color::Accent,
        Some(Cell::Text(_)) => Color::Fg,
    }
}

fn looks_number(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    text.parse::<i64>().is_ok() || (text.parse::<f64>().is_ok() && text.contains('.'))
}

fn fit(text: &str, width: f32) -> String {
    let max = ((width / 7.0).floor() as usize).saturating_sub(1).max(1);
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The custom-editor tab before a table is open: a short summary.
pub struct Overview {
    pub title: String,
    pub lines: Vec<String>,
    pub hint: String,
    pub bar: Color,
}

pub fn overview(width: f32, height: f32, info: &Overview) -> Picture {
    let width = if width.is_finite() && width > 1.0 {
        width
    } else {
        800.0
    };
    let height = if height.is_finite() && height > 1.0 {
        height
    } else {
        600.0
    };
    let mut picture = Picture::new();
    picture.rect(0.0, 0.0, width, height, Color::Bg);
    picture.rect(0.0, 0.0, width, 4.0, info.bar);
    picture.text(
        28.0,
        28.0,
        TextSize::Large,
        Align::Left,
        Color::Fg,
        &info.title,
    );
    let mut y = 64.0;
    for line in &info.lines {
        if y > height - 48.0 {
            break;
        }
        picture.text(28.0, y, TextSize::Normal, Align::Left, Color::Muted, line);
        y += 24.0;
    }
    let hint_y = (height - 36.0).max(y + 8.0);
    picture.text(
        28.0,
        hint_y,
        TextSize::Small,
        Align::Left,
        Color::Muted,
        &info.hint,
    );
    picture
}

fn relation_node(rel: &Relation) -> Node {
    let id = relation_id(rel);
    let mut node = Node::new(id, &rel.name).icon(Icon::named("file"), Some(ViewColor::Accent));
    if rel.kind == RelKind::Virtual {
        node = node.desc("virtual");
    } else if rel.kind == RelKind::View {
        node = node.desc("view");
    }
    if rel.columns.is_empty() {
        node
    } else {
        node.kids(Kids::Open)
    }
}

fn index_node(index: &IndexInfo) -> Node {
    Node::new(node_id("x", &index.name), &index.name)
        .desc(&index.table)
        .icon(Icon::named("search"), Some(ViewColor::Muted))
}

fn column_node(table: &str, index: usize, column: &crate::db::ColumnInfo) -> Node {
    let mut desc = column.ty.clone();
    if column.pk > 0 {
        if !desc.is_empty() {
            desc.push(' ');
        }
        desc.push_str("pk");
    }
    if column.notnull {
        if !desc.is_empty() {
            desc.push(' ');
        }
        desc.push_str("required");
    }
    Node::new(
        format!("c:{:016x}:{index}", fnv64(table.as_bytes())),
        &column.name,
    )
    .desc(&desc)
    .icon(Icon::named("dot"), Some(ViewColor::Muted))
}

fn relation_id(rel: &Relation) -> String {
    let prefix = if rel.kind == RelKind::View { "v" } else { "t" };
    node_id(prefix, &rel.name)
}

fn display_columns(columns: &[crate::db::ColumnInfo]) -> Vec<Column> {
    let shown: Vec<Column> = columns
        .iter()
        .take(DISPLAY_COLS)
        .map(|column| {
            let mut item = Column::new(&column_title(column)).min(8).sortable();
            if is_numeric(&column.ty) {
                item = item.right();
            }
            item
        })
        .collect();
    if shown.is_empty() {
        vec![Column::new("Result")]
    } else {
        shown
    }
}

fn column_title(column: &crate::db::ColumnInfo) -> String {
    let mut title = column.name.clone();
    if !column.ty.is_empty() {
        title.push(' ');
        title.push_str(&column.ty);
    }
    if column.pk > 0 {
        title.push_str(" PRIMARY KEY");
    }
    if column.notnull {
        title.push_str(" NOT NULL");
    }
    title
}

fn is_numeric(ty: &str) -> bool {
    let upper = ty.to_ascii_uppercase();
    upper.contains("INT")
        || upper.contains("REAL")
        || upper.contains("FLOA")
        || upper.contains("DOUB")
        || upper.contains("NUM")
}

fn column_note(columns: &[crate::db::ColumnInfo], note: &str) -> String {
    if columns.len() <= DISPLAY_COLS {
        return note.to_string();
    }
    let extra = format!("Showing {DISPLAY_COLS} of {} columns.", columns.len());
    if note.is_empty() {
        extra
    } else {
        format!("{extra} {note}")
    }
}

fn row_nodes(rows: &[PageRow], id_base: usize) -> Vec<Node> {
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let label = row
                .cells
                .first()
                .map(grid_text)
                .unwrap_or_else(|| "(row)".into());
            let mut node = Node::new(format!("r:{}", id_base + index), &label);
            for cell in row
                .cells
                .iter()
                .skip(1)
                .take(DISPLAY_COLS.saturating_sub(1))
            {
                node = node.cell(&grid_text(cell));
            }
            node
        })
        .collect()
}

fn node_id(prefix: &str, name: &str) -> String {
    let id = format!("{prefix}:{}", hex_encode(name));
    if id.len() <= 900 {
        id
    } else {
        format!("{prefix}#{:016x}", fnv64(name.as_bytes()))
    }
}

fn split_node(id: &str) -> Option<(&str, bool, &str)> {
    if let Some((prefix, rest)) = id.split_once('#') {
        Some((prefix, true, rest))
    } else {
        let (prefix, rest) = id.split_once(':')?;
        Some((prefix, false, rest))
    }
}

fn hex_encode(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<String> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let pair = std::str::from_utf8(&bytes[index..index + 2]).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
        index += 2;
    }
    String::from_utf8(out).ok()
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ColumnInfo, RelKind, Relation};

    #[test]
    fn grid_text_shapes_null_empty_blob_and_newlines() {
        assert_eq!(grid_text(&Cell::Null), "NULL");
        assert_eq!(grid_text(&Cell::Text(String::new())), "(empty)");
        assert_eq!(grid_text(&Cell::Blob(4)), "blob (4 bytes)");
        assert_eq!(grid_text(&Cell::Text("a\nb".into())), "a b");
    }

    #[test]
    fn schema_patch_names_the_table() {
        let schema = Schema {
            tables: vec![Relation {
                name: "notes".into(),
                kind: RelKind::Table,
                columns: vec![ColumnInfo {
                    name: "body".into(),
                    ty: "TEXT".into(),
                    notnull: false,
                    pk: 0,
                }],
                without_rowid: false,
            }],
            views: Vec::new(),
            indexes: Vec::new(),
        };
        let text = String::from_utf8(schema_patch(&schema).encode().unwrap()).unwrap();
        assert!(text.contains("notes"), "{text}");
        assert!(text.contains("body"), "{text}");
        let id = activated(&schema, &relation_id_of(&schema)).unwrap();
        assert_eq!(id.name, "notes");
    }

    fn relation_id_of(schema: &Schema) -> String {
        schema_children(schema, b"")
            .first()
            .map(|node| String::from_utf8(node.id().to_vec()).unwrap())
            .unwrap()
    }

    #[test]
    fn overview_names_the_file() {
        let picture = overview(
            400.0,
            200.0,
            &Overview {
                title: "notes.db".into(),
                lines: vec!["Saved".into()],
                hint: "Schema is in the sidebar.".into(),
                bar: Color::Accent,
            },
        );
        assert!(picture.body().contains("notes.db"));
        assert!(picture.body().contains("accent"));
    }

    #[test]
    fn spreadsheet_names_the_query_and_a_cell() {
        let columns = vec![ColumnInfo {
            name: "body".into(),
            ty: "TEXT".into(),
            notnull: false,
            pk: 0,
        }];
        let rows = vec![PageRow {
            key: crate::db::RowKey::Rowid(1),
            cells: vec![Cell::Text("hello".into())],
        }];
        let (picture, zones) = spreadsheet(
            800.0,
            500.0,
            &Sheet {
                file: "notes.db",
                version: "3.46.0",
                table: Some("notes"),
                columns: &columns,
                rows: &rows,
                offset: 0,
                more: false,
                schema: false,
                schema_text: "",
                statement: "INSERT INTO \"notes\" (\"body\") VALUES (?)",
                selected: Some((0, 0)),
                find_on: false,
                readonly: false,
                col_at: 0,
            },
        );
        let body = picture.body();
        assert!(body.contains("SELECT *"), "{body}");
        assert!(body.contains("notes"), "{body}");
        assert!(body.contains("hello"), "{body}");
        assert!(body.contains("INSERT"), "{body}");
        assert!(hit(&zones, 130.0, 50.0).is_some() || hit(&zones, 80.0, 90.0).is_some());
    }
}
