#!/usr/bin/env python3
"""The SQLite viewer in rhun, headless.

Open a database, see its table, run a SELECT from a .sql file, update a row, undo, redo, save,
and read the file back.

    RHUN=/path/to/rhun python3 tests/real.py
"""
import os
from pathlib import Path
import platform
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
RHUN = os.environ.get("RHUN")
SYSTEM = platform.system().lower()
MACHINE = platform.machine().lower()
if SYSTEM == "darwin" and MACHINE in ("arm64", "aarch64"):
    TARGET = "macos-arm64"
elif SYSTEM == "linux" and MACHINE == "x86_64":
    TARGET = "linux-x86_64"
elif SYSTEM == "windows" and MACHINE in ("amd64", "x86_64"):
    TARGET = "windows-x64"
else:
    TARGET = ""
EXE = ".exe" if SYSTEM == "windows" else ""
CONFIG = (
    "[files]\nrestore_session = false\nrestore_project = false\n"
    "[updates]\ncheck = false\n[git]\nenabled = false\n"
    "[ui]\nsidebar = true\nagents_panel = false\n"
    "[editor]\ncursor_blink = false\n"
)


def phase(lines, name):
    start = f"PHASE {name}"
    chunk = []
    seen = False
    for line in lines:
        if line.strip() == start:
            if seen:
                break
            seen = True
            chunk = []
            continue
        if seen:
            if line.startswith("PHASE "):
                break
            chunk.append(line)
    return chunk


def customs(lines):
    return [line for line in lines if line.startswith("cdoc=")]


def field(line, name):
    key = name + "="
    for part in line.split():
        if part.startswith(key):
            return part[len(key):]
    return ""


@unittest.skipUnless(RHUN, "set RHUN")
@unittest.skipUnless(TARGET, "no rhun target for this machine")
class SqliteViewer(unittest.TestCase):
    def test_query_edit_undo_redo_save(self):
        with tempfile.TemporaryDirectory(prefix="rhun-sqlite-") as tmp:
            work = Path(tmp)
            proj = work / "proj"
            proj.mkdir()
            db_path = proj / "notes.db"
            con = sqlite3.connect(db_path)
            con.execute("create table notes(id integer primary key, body text)")
            con.execute("insert into notes(id, body) values (1, 'hello')")
            con.commit()
            con.close()
            select = proj / "select.sql"
            update = proj / "update.sql"
            select.write_text("SELECT body FROM notes;\n", encoding="utf-8")
            update.write_text(
                "UPDATE notes SET body = 'edited' WHERE id = 1;\n", encoding="utf-8"
            )
            exts = work / "exts" / "BRAVO68WEB" / "rhun-sqlite-viewer"
            (exts / "bin").mkdir(parents=True)
            shutil.copy(ROOT / "extension.ini", exts)
            icon = ROOT / "icon.png"
            if icon.exists():
                shutil.copy(icon, exts)
            binary = ROOT / "bin" / f"rhun-sqlite-viewer-{TARGET}{EXE}"
            shutil.copy(binary, exts / "bin" / binary.name)
            (work / "config" / "rhun").mkdir(parents=True)
            (work / "config" / "rhun" / "config").write_text(CONFIG, encoding="utf-8")
            (work / "state").mkdir()
            (work / "tmp").mkdir()
            actions = [
                "echo PHASE open",
                "wait 100",
                "open " + db_path.as_posix(),
                "wait-ext rhun-sqlite-viewer running",
                "wait 500",
                "print-custom",
                "print-view schema",
                "echo PHASE select",
                "open " + select.as_posix(),
                "wait 200",
                "cmd rhun-sqlite-viewer.run",
                "wait 500",
                "view-show results",
                "wait 200",
                "print-view results",
                "echo PHASE update",
                "open " + update.as_posix(),
                "wait 200",
                "cmd rhun-sqlite-viewer.run",
                "wait 500",
                "print-custom",
                "echo PHASE undo",
                "cmd rhun-sqlite-viewer.undo",
                "wait 400",
                "open " + select.as_posix(),
                "wait 200",
                "cmd rhun-sqlite-viewer.run",
                "wait 500",
                "print-view results",
                "print-custom",
                "echo PHASE redo",
                "cmd rhun-sqlite-viewer.redo",
                "wait 400",
                "print-custom",
                "echo PHASE save",
                "open " + select.as_posix(),
                "wait 100",
                "cmd prev_tab",
                "wait 100",
                "print-tabs",
                "key ctrl+s",
                "wait 800",
                "print-custom",
                "echo PHASE done",
                "quit",
            ]
            script = work / "s.rsc"
            script.write_text("\n".join(actions) + "\n", encoding="utf-8")
            env = dict(os.environ)
            env["HOME"] = str(work)
            env["XDG_CONFIG_HOME"] = str(work / "config")
            env["XDG_STATE_HOME"] = str(work / "state")
            env["TMPDIR"] = str(work / "tmp")
            result = subprocess.run(
                [
                    RHUN,
                    str(proj),
                    "--headless",
                    "1000x600",
                    "--scale",
                    "1",
                    "--extensions",
                    str(work / "exts"),
                    "--script",
                    str(script),
                ],
                env=env,
                capture_output=True,
                timeout=180,
            )
            out = result.stdout.decode(errors="replace").splitlines()
            err = result.stderr.decode(errors="replace")
            self.assertEqual(result.returncode, 0, err + "\n" + "\n".join(out))
            opened = phase(out, "open")
            custom = customs(opened)
            self.assertTrue(custom, opened)
            self.assertEqual(field(custom[-1], "model"), "owned", opened)
            self.assertEqual(field(custom[-1], "dirty"), "0", opened)
            self.assertTrue(any("label=notes" in line for line in opened), opened)
            selected = phase(out, "select")
            self.assertTrue(any("label=hello" in line for line in selected), selected)
            updated = customs(phase(out, "update"))
            self.assertTrue(updated, out)
            self.assertEqual(field(updated[-1], "dirty"), "1", updated)
            undone = phase(out, "undo")
            self.assertTrue(any("label=hello" in line for line in undone), undone)
            self.assertEqual(field(customs(undone)[-1], "dirty"), "0", undone)
            redone = customs(phase(out, "redo"))
            self.assertTrue(redone, out)
            self.assertEqual(field(redone[-1], "dirty"), "1", redone)
            saved = phase(out, "save")
            tabs = [line for line in saved if line.startswith("tabs=")]
            self.assertTrue(tabs, saved)
            self.assertIn("*notes.db", tabs[-1], saved)
            saved_custom = customs(saved)
            self.assertTrue(saved_custom, saved)
            self.assertEqual(field(saved_custom[-1], "dirty"), "0", saved)
            con = sqlite3.connect(db_path)
            got = con.execute("select body from notes where id = 1").fetchone()
            con.close()
            self.assertEqual(got, ("edited",))


if __name__ == "__main__":
    unittest.main()
