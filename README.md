# SQLite

Browse a SQLite database, run SQL, and edit rows in [rhun](https://rhun.app). A small Rust
program built on
[rhun-ext](https://github.com/rhun-editor/extension-protocol/tree/main/rust/rhun-ext). The
database file opens in its own tab. rhun keeps Save, Undo, and the dirty dot; this extension
keeps the bytes.

Open a `.sqlite`, `.sqlite3`, `.db`, `.sdb`, or `.db3` file. The tab is laid out like a
spreadsheet editor: `SELECT * FROM` the current table, a grid whose headers show each column's
type and key, and the `INSERT` or `UPDATE` statement along the bottom. The sidebar lists the
tables. The side column is the statement editor (Commit, or a Find box). Custom SQL stays in
the bottom panel.

- **Choose a table or view** in the sidebar, or click its name, to load its rows. Click a column
  header to sort. Click a cell to edit that column. **Schema** shows the `CREATE` statement.
  **Query** opens the SQL panel. **Insert** starts a new row. **Find** filters the table.
- **SQLite: Run SQL** runs the SQL panel. When the command is used from a `.sql`, `.psql`, or
  `.ddl` tab, it runs that file against the database you opened last. Ctrl+Shift+Enter does the
  same while a text tab or an extension view has the keyboard. Ctrl+Enter in the SQL field runs
  it too.
- **Commit** writes the insert or update. **Delete row** removes the selected row. Save the
  database tab (Ctrl+S, or Cmd+S on macOS) to write the file.
- **SQLite: Undo** and **SQLite: Redo** step through edits of the private copy. rhun's Undo and
  Redo do the same when the database tab is the active one.

Clearing a field stores NULL. A field you leave untouched keeps its value, so an empty string
stays an empty string until you edit that field. After you clear it, typing the field empty
again is NULL: the form cannot tell a cleared field from an empty string you meant to store.
Blob cells are shown and are not edited. Rows that came back from an arbitrary query are not
edited in the form; open the table instead. Views are read-only. The results table shows 16
columns, and the row form shows up to 60.

A file of at most 256 MB is copied into rhun's state directory and opened read-write. Undo keeps
up to 20 snapshots of that copy, and at most 256 MB of snapshot bytes. A larger file opens
read-only from its real path. A database in WAL mode should be checkpointed first: the copy
includes a `-wal` file beside the database when it fits in the 256 MB budget, then the copy is
stored as one file.

`.sql` files stay text documents. rhun already highlights them.

## Permissions

It reads the open `.sql` document (`documents = read`) and, for a database larger than 256 MB,
the project file itself (`project = read`). It does not use the network, your home directory, or
other programs. rhun reads and writes the database you save, in parts of 4 MB. The private copy
lives in the extension state directory, which is always writable.

## Build and test

```sh
cargo test              # statement splitting, schema, browse, row edit, undo, redo
cargo build --release
tests/run.sh [RHUN]     # headless, against the rhun on your PATH
tests/check.sh          # the protocol's conformance checker (needs Go)
```

`tests/run.sh` builds the extension and runs `tests/real.py`: open a database, see its table, run
a query, update a row, undo, redo, save, and read the file back. A tag `vX.Y.Z` equal to
`version` in `extension.ini` builds Linux, macOS, and Windows.

The extension is not published to the rhun extension index from this repository. It needs rhun 0.18.

Install it with **Link a Folder** (the development install) and choose this repository. rhun
then runs `bin/rhun-sqlite-viewer-macos-arm64` from here. `tests/run.sh` and `stage.sh` build
that program and ad-hoc sign it. A plain `cargo build` binary still has only the linker's
signature, and macOS kills it on launch (exit 137). After copying a new build into `bin/`, run
`sh tests/sign.sh bin/rhun-sqlite-viewer-macos-arm64` and restart the extension.

Do not use **Install from Folder** on this repository. That copies the whole tree, and rhun
refuses a folder that contains a link. `cargo build` fills `target/` with hard links, so the
install stops with "the folder holds links or devices". **Install from Folder** works on the
clean folder from `sh stage.sh`, which holds only the manifest, the icon, and the program.

There is no WebAssembly build. The SQLite library is compiled into the program, and that build
does not target `wasm32-wasip1`.
