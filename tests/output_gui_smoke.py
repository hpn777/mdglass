#!/usr/bin/env python3
"""Compare GUI PDF export with GtkPrintOperation's real EXPORT callbacks.

Usage: python3 tests/output_gui_smoke.py path/to/mdglass
Needs gtk4-broadwayd; Poppler inspectors are optional. No printer or browser
is needed. A private XDG_RUNTIME_DIR and an ephemeral HTTP port isolate this
suite from tests/gui_smoke.py's Broadway display :31 and the user's session.
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import xml.etree.ElementTree as ET


MDGLASS = None
BROADWAY = shutil.which("gtk4-broadwayd")
PDFINFO = shutil.which("pdfinfo")
PDFTOTEXT = shutil.which("pdftotext")


def fixture():
    """Exercise vertical pages, horizontal bands, and an oversized row."""
    rows = [f"GUIROW{i:03d}" for i in range(90)]
    fragments = [f"PART{i:04d}" for i in range(1000)]
    cells = [f"CELL{row}{col:02d}" for row in range(2) for col in range(1, 10)]
    text = "# GUISNAPSHOT\n\nShared output defaults: A4, margin 36 points.\n\n"
    text += "| TALLHEADER | VALUEHEADER |\n|---|---|\n"
    text += "".join(f"| {marker} | value {i} |\n" for i, marker in enumerate(rows))
    text += "\n## Column bands\n\n| BANDHEADER | "
    text += " | ".join(f"COLUMN{i:02d}" for i in range(1, 10)) + " |\n"
    text += "|" + "---|" * 10 + "\n"
    for row in range(2):
        text += f"| KEY{row:03d} | " + " | ".join(
            f"CELL{row}{col:02d} abcdefghijklmnop" for col in range(1, 10)) + " |\n"
    text += "\n## Oversized row\n\n| FRAGMENTHEADER |\n|---|\n| "
    text += " ".join(fragments) + " |\n\nENDSNAPSHOT\n"
    return text, rows, fragments, cells


class GuiOutputChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BROADWAY:
            raise unittest.SkipTest("gtk4-broadwayd not installed")
        cls.temp = tempfile.TemporaryDirectory(prefix="mdglass-output-gui-")
        cls.addClassCleanup(cls.temp.cleanup)
        cls.work = Path(cls.temp.name)
        runtime = cls.work / "runtime"
        runtime.mkdir(mode=0o700)
        cls.env = os.environ.copy()
        for name in ("DISPLAY", "WAYLAND_DISPLAY", "GTK_MODULES", "G_DEBUG",
                     "MDGLASS_GUI_SCRIPT", "GDK_DEBUG"):
            cls.env.pop(name, None)
        # This display is private to runtime, not the host's Broadway namespace.
        # Port zero lets the kernel allocate an unused HTTP port too.
        cls.env.update(XDG_RUNTIME_DIR=str(runtime), GDK_BACKEND="broadway",
                       BROADWAY_DISPLAY=":73", ADW_DISABLE_PORTAL="1",
                       GDK_DEBUG="no-portals", GTK_A11Y="none", LC_ALL="C.UTF-8")
        log_path = cls.work / "broadway.log"
        log = log_path.open("wb")
        cls.addClassCleanup(log.close)
        cls.broadway = subprocess.Popen(
            [BROADWAY, "--address=127.0.0.1", "--port=0", ":73"],
            env=cls.env, stdout=log, stderr=subprocess.STDOUT)
        cls.addClassCleanup(cls.stop_broadway)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            diagnostic = log_path.read_text(errors="replace")
            if cls.broadway.poll() is not None:
                raise RuntimeError("private Broadway failed to start: " + diagnostic)
            if "Listening on " in diagnostic:
                break
            time.sleep(0.05)
        else:
            raise RuntimeError("private Broadway did not become ready: " + diagnostic)
        cls.markdown, cls.rows, cls.fragments, cls.cells = fixture()
        cls.source = cls.work / "fixture.md"
        cls.source.write_text(cls.markdown, encoding="utf-8")

    @classmethod
    def stop_broadway(cls):
        if cls.broadway.poll() is None:
            cls.broadway.terminate()
            try:
                cls.broadway.wait(timeout=5)
            except subprocess.TimeoutExpired:
                cls.broadway.kill()
                cls.broadway.wait(timeout=5)

    def inspector(self, executable, args):
        result = subprocess.run([executable, *map(str, args)], capture_output=True,
                                timeout=30, env=self.env)
        self.assertEqual(result.returncode, 0,
                         result.stderr.decode(errors="replace"))
        return result.stdout.decode("utf-8", errors="replace")

    def export_pair(self, label, stdin=False):
        direct = self.work / f"{label}-export.pdf"
        printed = self.work / f"{label}-print.pdf"
        env = dict(self.env, MDGLASS_GUI_SCRIPT=f"pdf:{direct};print-pdf:{printed};quit")
        result = subprocess.run(
            [MDGLASS, "--gui", "-" if stdin else str(self.source)],
            input=self.markdown.encode("utf-8") if stdin else b"",
            env=env, cwd=self.work, capture_output=True, timeout=90)
        stderr = result.stderr.decode(errors="replace")
        self.assertEqual(result.returncode, 0, stderr)
        noise = [line for line in stderr.splitlines()
                 if re.search(r"-(?:WARNING|CRITICAL|ERROR)\b|"
                              r"cannot open display|failed to initialize GTK", line, re.I)]
        self.assertFalse(noise, "GTK/runtime diagnostics:\n" + "\n".join(noise))
        self.assertIsNone(self.broadway.poll(), "Broadway exited during the GUI test")
        for path in (direct, printed):
            self.assertTrue(path.is_file(), f"GUI hook did not create {path.name}\n{stderr}")
            data = path.read_bytes()
            self.assertTrue(data.startswith(b"%PDF-"), f"not a PDF: {path.name}")
            self.assertIn(b"%%EOF", data[-1024:], f"truncated PDF: {path.name}")
        return direct, printed

    def geometry(self, path):
        info = self.inspector(PDFINFO, [path])
        count = re.search(r"^Pages:\s+(\d+)", info, re.M)
        self.assertIsNotNone(count, info)
        count = int(count.group(1))
        self.assertGreater(count, 1, "fixture must exercise pagination")
        info = self.inspector(PDFINFO, ["-f", "1", "-l", count, path])
        sizes = re.findall(r"^Page(?:\s+\d+)?\s+size:\s+([\d.]+) x ([\d.]+) pts",
                           info, re.M)
        self.assertEqual(len(sizes), count, "missing per-page dimensions:\n" + info)
        dimensions = [(float(width), float(height)) for width, height in sizes]
        for width, height in dimensions:
            self.assertAlmostEqual(width, 595.28, delta=1.0, msg="expected portrait A4")
            self.assertAlmostEqual(height, 841.89, delta=1.0, msg="expected portrait A4")
        return count, dimensions

    def text_pages(self, path):
        text = self.inspector(PDFTOTEXT, ["-layout", path, "-"])
        pages = text.split("\f")
        if pages and not pages[-1].strip():
            pages.pop()
        return pages

    def check_content(self, pages):
        text = "\n".join(pages)
        for marker in ["GUISNAPSHOT", "ENDSNAPSHOT", *self.rows, *self.fragments, *self.cells]:
            self.assertIn(marker, text, f"lost source content: {marker}")
        tall_pages = 0
        fragment_pages = 0
        for index, page in enumerate(pages, 1):
            if re.search(r"GUIROW\d{3}", page):
                tall_pages += 1
                self.assertIn("TALLHEADER", page, f"missing tall-table header on page {index}")
                self.assertIn("VALUEHEADER", page, f"missing tall-table header on page {index}")
            if re.search(r"PART\d{4}", page):
                fragment_pages += 1
                self.assertIn("FRAGMENTHEADER", page,
                              f"missing fragmented-row header on page {index}")
        self.assertGreater(tall_pages, 1, "fixture did not continue the tall table")
        self.assertGreater(fragment_pages, 1, "fixture did not fragment the oversized row")

    def check_margins(self, path):
        xml = self.inspector(PDFTOTEXT, ["-bbox", path, "-"])
        root = ET.fromstring(xml)
        left_edges = []
        for page in root.iter():
            if page.tag.rsplit("}", 1)[-1] != "page":
                continue
            width, height = float(page.attrib["width"]), float(page.attrib["height"])
            for word in page.iter():
                if word.tag.rsplit("}", 1)[-1] != "word":
                    continue
                x0, y0, x1, y1 = [float(word.attrib[key]) for key in
                                  ("xMin", "yMin", "xMax", "yMax")]
                left_edges.append(x0)
                # One point tolerates glyph overhang and Poppler rounding.
                self.assertGreaterEqual(x0, 35, f"text outside 36pt left margin: {word.text}")
                self.assertGreaterEqual(y0, 35, f"text outside 36pt top margin: {word.text}")
                self.assertLessEqual(x1, width - 35,
                                     f"text outside 36pt right margin: {word.text}")
                self.assertLessEqual(y1, height - 35,
                                     f"text outside 36pt bottom margin: {word.text}")
        self.assertTrue(left_edges, "no text bounding boxes")
        self.assertAlmostEqual(min(left_edges), 36, delta=1.0,
                               msg="shared default left margin is not 36 points")

    def compare_pair(self, pair):
        direct, printed = pair
        # Missing inspectors do not skip actually exercising both GUI hooks.
        if not PDFINFO or not PDFTOTEXT:
            missing = [name for name, tool in (("pdfinfo", PDFINFO),
                                               ("pdftotext", PDFTOTEXT)) if not tool]
            self.skipTest("PDFs produced; optional comparison tools absent: " + ", ".join(missing))
        direct_geometry = self.geometry(direct)
        print_geometry = self.geometry(printed)
        self.assertEqual(direct_geometry[0], print_geometry[0],
                         "GtkPrintOperation and direct export paginate differently")
        for a, b in zip(direct_geometry[1], print_geometry[1]):
            for first, second in zip(a, b):
                self.assertAlmostEqual(first, second, delta=0.1)
        direct_pages, print_pages = self.text_pages(direct), self.text_pages(printed)
        self.assertEqual(len(direct_pages), direct_geometry[0])
        self.assertEqual(len(print_pages), print_geometry[0])
        self.check_content(direct_pages)
        self.check_content(print_pages)
        self.assertEqual([" ".join(page.split()) for page in direct_pages],
                         [" ".join(page.split()) for page in print_pages],
                         "GUI export and print callbacks disagree on per-page content")
        self.check_margins(direct)
        self.check_margins(printed)

    def test_file_snapshot_export_matches_print_callbacks(self):
        self.compare_pair(self.export_pair("file"))

    def test_stdin_snapshot_export_matches_print_callbacks(self):
        self.compare_pair(self.export_pair("stdin", stdin=True))

    def test_output_settings_forms_cancel_cleanly(self):
        env = dict(self.env, MDGLASS_GUI_SCRIPT=
                   "action:print;output-response:cancel;action:export-pdf;output-response:cancel;quit")
        result = subprocess.run([MDGLASS, "--gui", self.source], input=b"", env=env,
                                capture_output=True, timeout=30)
        stderr = result.stderr.decode(errors="replace")
        self.assertEqual(result.returncode, 0, stderr)
        self.assertNotRegex(stderr, r"-(?:WARNING|CRITICAL|ERROR)\b", stderr)

    def test_independent_gui_instances_do_not_forward_jobs(self):
        processes = []
        destinations = [self.work / f"independent-{i}.pdf" for i in range(2)]
        try:
            for dest in destinations:
                env = dict(self.env, MDGLASS_GUI_SCRIPT=f"pdf:{dest};quit")
                processes.append(subprocess.Popen([MDGLASS, "--gui", self.source],
                                 stdin=subprocess.DEVNULL, env=env, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE))
            for process in processes:
                _, stderr = process.communicate(timeout=30)
                self.assertEqual(process.returncode, 0, stderr.decode(errors="replace"))
            for dest in destinations:
                self.assertTrue(dest.is_file(), f"job forwarded to another instance: {dest}")
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                    process.communicate()

    def test_background_export_survives_document_closure(self):
        source = self.work / "background.md"
        source.write_text("| Identifier | Value | Notes |\n|---|---|---|\n" + "".join(
            f"| ASYNC{i:05d} | {i} | several words for background pagination |\n"
            for i in range(5000)), encoding="utf-8")
        dest = self.work / "background.pdf"
        env = dict(self.env, MDGLASS_GUI_SCRIPT=f"pdf-async:{dest};close;find:ignored;quit")
        result = subprocess.run([MDGLASS, "--gui", source], input=b"", env=env,
                                capture_output=True, timeout=90)
        stderr = result.stderr.decode(errors="replace")
        self.assertEqual(result.returncode, 0, stderr)
        self.assertIn("pdf-async done ok", stderr)
        self.assertIn("mdglass-script: close", stderr)
        self.assertLess(stderr.index("mdglass-script: close"), stderr.index("pdf-async done ok"),
                        "fixture must exercise completion after document closure")
        self.assertNotRegex(stderr, r"-(?:WARNING|CRITICAL|ERROR)\b|FAILED", stderr)
        self.assertTrue(dest.is_file())
        if PDFTOTEXT:
            text = self.inspector(PDFTOTEXT, [dest, "-"])
            self.assertIn("ASYNC00000", text)
            self.assertIn("ASYNC04999", text)

    def test_background_cancel_preserves_existing_destination(self):
        dest = self.work / "cancelled.pdf"
        sentinel = b"existing destination must survive cancellation"
        dest.write_bytes(sentinel)
        env = dict(self.env, MDGLASS_GUI_SCRIPT=f"pdf-cancel:{dest};quit")
        result = subprocess.run([MDGLASS, "--gui", self.source], input=b"", env=env,
                                capture_output=True, timeout=30)
        stderr = result.stderr.decode(errors="replace")
        self.assertEqual(result.returncode, 0, stderr)
        self.assertIn("pdf-cancel done cancelled", stderr)
        self.assertNotRegex(stderr, r"-(?:WARNING|CRITICAL|ERROR)\b|FAILED", stderr)
        self.assertEqual(dest.read_bytes(), sentinel)
        self.assertEqual(list(self.work.glob(".mdglass-*.tmp")), [])


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: output_gui_smoke.py PATH_TO_MDGLASS")
    MDGLASS = os.path.abspath(sys.argv[1])
    if not os.path.isfile(MDGLASS) or not os.access(MDGLASS, os.X_OK):
        sys.exit(f"not an executable: {MDGLASS}")
    unittest.main(argv=[sys.argv[0]], verbosity=2)
