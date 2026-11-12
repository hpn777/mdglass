#!/usr/bin/env python3
"""Headless PDF export integration checks (stdlib only).

Usage: python3 tests/output_smoke.py path/to/mdglass
Native Cairo/Pango are needed by the exporter. Missing native libraries skip
rendering checks, not CLI checks. Poppler's pdfinfo, pdftotext, pdftoppm and
pdfimages are optional test inspectors, never application dependencies.
"""

import ctypes
import ctypes.util
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET


MDGLASS = None
TOOLS = {name: shutil.which(name) for name in
         ("pdfinfo", "pdftotext", "pdftoppm", "pdfimages", "fc-match")}


def headless_env():
    env = os.environ.copy()
    for name in ("DISPLAY", "WAYLAND_DISPLAY", "BROADWAY_DISPLAY",
                 "MDGLASS_GUI_SCRIPT"):
        env.pop(name, None)
    # Merely linking GUI libraries is fine; initializing GTK must not be needed.
    env.update(GDK_BACKEND="mdglass-output-invalid-backend",
               GTK_MODULES="mdglass-output-nonexistent-module",
               G_DEBUG="fatal-warnings", ADW_DISABLE_PORTAL="1",
               LC_ALL="C.UTF-8")
    return env


def missing_native_libraries():
    missing = []
    for name in ("cairo", "pango-1.0", "pangocairo-1.0"):
        library = ctypes.util.find_library(name)
        try:
            if library is None:
                raise OSError("not found")
            ctypes.CDLL(library)
        except OSError as exc:
            missing.append(f"{name}: {exc}")
    return missing


class OutputCase(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mdglass-output-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.source = self.document("input.md", "# Export smoke\n\nHello PDF.\n")

    def document(self, name, content):
        path = self.work / name
        path.write_text(content, encoding="utf-8")
        return path

    def run_cli(self, args, stdin=b"", expected=0):
        result = subprocess.run(
            [MDGLASS, *map(str, args)], input=stdin, capture_output=True,
            env=headless_env(), cwd=self.work, timeout=45)
        detail = (f"args={args!r}\nexit={result.returncode}\n"
                  f"stderr={result.stderr.decode(errors='replace')[:3000]}")
        if isinstance(expected, tuple):
            self.assertIn(result.returncode, expected, detail)
        else:
            self.assertEqual(result.returncode, expected, detail)
        self.assertNotRegex(result.stderr.decode(errors="replace"),
                            r"(?i)(Gtk-(?:WARNING|CRITICAL|ERROR)|"
                            r"cannot open display|failed to initialize GTK)", detail)
        if expected:
            self.assertTrue(result.stderr.strip(), "failure needs a diagnostic: " + detail)
        return result

    def inspect(self, tool, args):
        if not TOOLS[tool]:
            self.skipTest(f"optional PDF inspector {tool} not installed")
        result = subprocess.run([TOOLS[tool], *map(str, args)],
                                capture_output=True, timeout=30,
                                env=headless_env())
        self.assertEqual(result.returncode, 0,
                         f"{tool}: {result.stderr.decode(errors='replace')}")
        return result.stdout

    def pdf_text(self, path):
        return self.inspect("pdftotext", ["-layout", path, "-"]).decode("utf-8")

    def pages(self, path):
        return [page for page in self.pdf_text(path).split("\f") if page.strip()]

    def info(self, path):
        return self.inspect("pdfinfo", [path]).decode("utf-8", errors="replace")

    def page_count(self, path):
        match = re.search(r"^Pages:\s+(\d+)", self.info(path), re.M)
        self.assertIsNotNone(match, "pdfinfo did not report a page count")
        return int(match.group(1))

    def export(self, source=None, options=(), name="output.pdf", stdin=b""):
        path = self.work / name
        self.run_cli(["--export-pdf", path, *options,
                      self.source if source is None else source], stdin=stdin)
        self.assertTrue(path.is_file(), "export did not create its destination")
        data = path.read_bytes()
        self.assertTrue(data.startswith(b"%PDF-"), data[:80])
        self.assertIn(b"%%EOF", data[-1024:], "truncated PDF")
        return path


class CliChecks(OutputCase):
    def test_invalid_numeric_settings(self):
        cases = {
            "--margin": ("nan", "NaN", "inf", "-inf", "-1", "10000", "1e309", "oops"),
            "--font-size": ("nan", "inf", "0", "-1", "10000", "oops"),
            "--overlap": ("nan", "inf", "-1", "10000", "oops"),
        }
        target = self.work / "unchanged.pdf"
        sentinel = b"existing output must survive invalid settings\n"
        for option, values in cases.items():
            for value in values:
                with self.subTest(option=option, value=value):
                    target.write_bytes(sentinel)
                    self.run_cli(["--export-pdf", target, f"{option}={value}", self.source],
                                 expected=2)
                    self.assertEqual(target.read_bytes(), sentinel)

    def test_invalid_choices_and_missing_values(self):
        for option, value in (("--paper", "poster"), ("--table-mode", "stretch"),
                              ("--diagram-mode", "raster")):
            with self.subTest(option=option):
                self.run_cli(["--export-pdf", self.work / "bad.pdf",
                              option, value, self.source], expected=2)
        for option in ("--export-pdf", "--paper", "--margin", "--font-size",
                       "--table-mode", "--diagram-mode", "--overlap"):
            with self.subTest(missing=option):
                args = [] if option == "--export-pdf" else ["--export-pdf", "out.pdf"]
                self.run_cli([*args, option], expected=2)
        self.run_cli(["--export-pdf=", self.source], expected=2)

    def test_incompatible_modes_and_multiple_inputs(self):
        for gui in ("--gui", "-g"):
            with self.subTest(gui=gui):
                self.run_cli(["--export-pdf", "bad.pdf", gui, self.source], expected=2)
        self.run_cli(["--export-pdf", "bad.pdf", self.source, self.source], expected=2)
        self.run_cli(["--export-pdf", "bad.pdf", "--not-an-option", self.source], expected=2)

    def test_same_input_and_output_is_not_truncated(self):
        original = self.source.read_bytes()
        # The contract requires rejection/preservation; this may be classified
        # as either an invalid destination (2) or an I/O error (1).
        self.run_cli(["--export-pdf", self.source, self.source], expected=(1, 2))
        self.assertEqual(self.source.read_bytes(), original)
        for kind in ("symlink", "hardlink"):
            alias = self.work / (kind + ".pdf")
            if kind == "symlink":
                alias.symlink_to(self.source)
            else:
                os.link(self.source, alias)
            self.run_cli(["--export-pdf", alias, self.source], expected=(1, 2))
            self.assertEqual(self.source.read_bytes(), original)

    def test_duplicate_export_and_layout_without_export(self):
        self.run_cli(["--export-pdf", "a.pdf", "--export-pdf", "b.pdf", self.source], expected=2)
        self.run_cli(["--paper", "a4", self.source], expected=2)

    def test_redirected_stdin_cannot_replace_its_source(self):
        original = self.source.read_bytes()
        for explicit in (False, True):
            for alias in (False, True):
                target = self.source
                if alias:
                    target = self.work / f"redirect-alias-{explicit}.pdf"
                    os.link(self.source, target)
                with self.source.open("rb") as stdin:
                    result = subprocess.run([MDGLASS, "--export-pdf", str(target),
                                             *(["-"] if explicit else [])], stdin=stdin,
                                            capture_output=True, env=headless_env(),
                                            cwd=self.work, timeout=30)
                self.assertEqual(result.returncode, 2, result.stderr.decode(errors="replace"))
                self.assertIn(b"redirected input", result.stderr)
                self.assertEqual(self.source.read_bytes(), original)

    def test_missing_input_preserves_existing_destination(self):
        target = self.work / "existing.pdf"
        sentinel = b"previous export remains intact\n"
        target.write_bytes(sentinel)
        self.run_cli(["--export-pdf", target, self.work / "missing.md"], expected=1)
        self.assertEqual(target.read_bytes(), sentinel)


class PdfChecks(OutputCase):
    @classmethod
    def setUpClass(cls):
        missing = missing_native_libraries()
        if missing:
            raise unittest.SkipTest("native PDF libraries unavailable: " + "; ".join(missing))

    def test_headless_export_and_unicode(self):
        source = self.document("unicode.md", "# Unicode\n\ncafé naïve Ελληνικά Привет\n")
        path = self.export(source)
        text = self.pdf_text(path)
        self.assertIn("café", text)
        self.assertIn("naïve", text)
        # These scripts are covered by DejaVu Sans, but minimal systems may
        # have no such font. Latin extraction above remains mandatory.
        if TOOLS["fc-match"]:
            font = subprocess.run([TOOLS["fc-match"], "DejaVu Sans", "-f", "%{family}"],
                                  capture_output=True, timeout=10)
            if b"DejaVu Sans" in font.stdout:
                self.assertIn("Ελληνικά", text)
                self.assertIn("Привет", text)

    def test_wrapped_text_and_table_geometry_stays_on_page(self):
        source = self.document("geometry.md", "# Page geometry\n\n" +
            ("Paragraph with **bold** and _italic_ words. " * 100) +
            "\n\n> Quoted paragraph " + ("quoted words " * 100) +
            "\n\n- List item " + ("list words " * 100) +
            "\n\n```text\n" + "long_identifier_" * 50 + "\n```\n\n" +
            "| First | Second |\n|---|---|\n| " + "cell words " * 100 +
            " | " + "other words " * 50 + " |\n")
        path = self.export(source, ("--table-mode=fit", "--margin=36"))
        root = ET.fromstring(self.inspect("pdftotext", ["-bbox", path, "-"]))
        words = 0
        for page in root.iter():
            if page.tag.rsplit("}", 1)[-1] != "page":
                continue
            width, height = float(page.attrib["width"]), float(page.attrib["height"])
            for word in page.iter():
                if word.tag.rsplit("}", 1)[-1] != "word":
                    continue
                words += 1
                self.assertGreaterEqual(float(word.attrib["xMin"]), 35.0, word.text)
                self.assertGreaterEqual(float(word.attrib["yMin"]), 35.0, word.text)
                self.assertLessEqual(float(word.attrib["xMax"]), width - 35.0, word.text)
                self.assertLessEqual(float(word.attrib["yMax"]), height - 35.0, word.text)
        self.assertGreater(words, 500)

    def test_empty_document_is_a_valid_single_page(self):
        path = self.export(self.document("empty.md", ""))
        self.assertEqual(self.page_count(path), 1)

    def test_paper_sizes_stdin_and_equals_options(self):
        sizes = {"a4": (595.28, 841.89), "a3": (841.89, 1190.55),
                 "letter": (612, 792), "legal": (612, 1008)}
        for paper, dimensions in sizes.items():
            for landscape in (False, True):
                with self.subTest(paper=paper, landscape=landscape):
                    path = self.work / f"{paper}-{landscape}.pdf"
                    self.run_cli([f"--export-pdf={path}", f"--paper={paper}",
                                  "--margin=24", "--font-size=11", "--overlap=0",
                                  *(["--landscape"] if landscape else []), "-"],
                                 stdin=b"# STDINMARK\n\nPiped PDF input.\n")
                    info = self.info(path)
                    match = re.search(r"^Page size:\s+([\d.]+) x ([\d.]+) pts", info, re.M)
                    self.assertIsNotNone(match, info)
                    expected = dimensions[::-1] if landscape else dimensions
                    for actual, wanted in zip(map(float, match.groups()), expected):
                        self.assertAlmostEqual(actual, wanted, delta=1.0)
                    self.assertIn("STDINMARK", self.pdf_text(path))

    def test_option_terminator_and_separate_numeric_options(self):
        self.document("--input.md", "TerminatorMARK\n")
        path = self.work / "terminator.pdf"
        self.run_cli(["--export-pdf", path, "--margin", "24", "--font-size", "10",
                      "--", "--input.md"])
        self.assertIn("TerminatorMARK", self.pdf_text(path))

    def test_zero_margin_and_implicit_stdin(self):
        target = self.work / "zero.pdf"
        self.run_cli(["--export-pdf", target, "--margin=0"], stdin=b"ZeroMarginMARK\n")
        self.assertIn("ZeroMarginMARK", self.pdf_text(target))

    def test_links_honor_no_urls(self):
        source = self.document("links.md", "[LinkMARK](https://example.org/URLMARK)\n")
        shown = self.export(source, name="shown.pdf")
        hidden = self.export(source, ("--no-urls",), name="hidden.pdf")
        self.assertIn("URLMARK", self.pdf_text(shown))
        self.assertIn("LinkMARK", self.pdf_text(hidden))
        self.assertNotIn("URLMARK", self.pdf_text(hidden))

    def test_tall_table_repeats_header_on_every_continuation(self):
        markers = [f"ROW{i:03d}" for i in range(110)]
        table = "| HEADERKEY | HEADERVALUE |\n|---|---|\n" + "".join(
            f"| {marker} | value {i} |\n" for i, marker in enumerate(markers))
        path = self.export(self.document("tall.md", table), ("--font-size", "12"))
        pages = self.pages(path)
        self.assertGreater(len(pages), 1, "fixture must exercise continuation pages")
        text = "\n".join(pages)
        for marker in markers:
            self.assertIn(marker, text)
        for index, page in enumerate(pages):
            if re.search(r"ROW\d{3}", page):
                self.assertIn("HEADERKEY", page, f"missing header on page {index + 1}")
                self.assertIn("HEADERVALUE", page, f"missing header on page {index + 1}")

    def test_wide_table_bands_preserve_all_cells_and_key_column(self):
        headers = ["BANDKEY"] + [f"COL{i:02d}" for i in range(1, 13)]
        markers = [f"C{col:02d}R{row:02d}" for row in range(3) for col in range(1, 13)]
        table = "| " + " | ".join(headers) + " |\n|" + "---|" * len(headers) + "\n"
        for row in range(3):
            cells = [f"KEY{row:03d}"] + [
                f"C{col:02d}R{row:02d} abcdefghijklmnop" for col in range(1, 13)]
            table += "| " + " | ".join(cells) + " |\n"
        source = self.document("wide.md", table)
        path = self.export(source, ("--table-mode", "tile", "--font-size", "11"))
        pages = self.pages(path)
        self.assertGreater(len(pages), 1, "wide table must create column bands")
        for marker in markers:
            self.assertIn(marker, "\n".join(pages))
        for page in pages:
            if re.search(r"C\d{2}R\d{2}", page):
                self.assertIn("BANDKEY", page)
                self.assertIn("KEY000", page)
        no_repeat = self.export(source, ("--table-mode=tile", "--no-repeat-first-column"),
                                name="no-repeat.pdf")
        text = self.pdf_text(no_repeat)
        for marker in markers:
            self.assertIn(marker, text)
        self.assertEqual(text.count("KEY000"), 1, "disabled key column was repeated")

    def test_oversized_row_fragments_preserve_content(self):
        markers = [f"FRAG{i:04d}" for i in range(900)]
        table = ("| FRAGMENTHEADER |\n|---|\n| " + " ".join(markers) + " |\n")
        path = self.export(self.document("fragment.md", table),
                           ("--table-mode", "fit", "--font-size", "12"))
        pages = self.pages(path)
        self.assertGreater(len(pages), 1, "oversized row must be fragmented")
        text = "\n".join(pages)
        for marker in markers:
            self.assertIn(marker, text)
        for page in pages:
            if "FRAG0" in page:
                self.assertIn("FRAGMENTHEADER", page)

    def test_header_only_table_and_fit_mode(self):
        source = self.document("header.md", "| ONLYHEADER | OTHERHEADER |\n|---|---|\n")
        for mode in ("auto", "fit", "tile"):
            with self.subTest(mode=mode):
                path = self.export(source, (f"--table-mode={mode}",), name=f"header-{mode}.pdf")
                text = self.pdf_text(path)
                self.assertIn("ONLYHEADER", text)
                self.assertIn("OTHERHEADER", text)

    def test_empty_boundary_cells_and_escaped_pipe_content(self):
        source = self.document("boundary.md", "||CENTERHEADER||\n|---|---|---|\n||CENTERVALUE||\n\n"
                               r"| LEFTHEADER | RIGHTHEADER |" + "\n|---|---|\n" +
                               r"| LEFTVALUE | escaped\|" + "\n")
        text = self.pdf_text(self.export(source))
        for marker in ("CENTERHEADER", "CENTERVALUE", "LEFTVALUE", "escaped|"):
            self.assertIn(marker, text)

    def test_diagram_modes_tiling_and_source_fallback(self):
        nodes = [f'N{i}["Node {i:02d} moderately wide label"]' for i in range(16)]
        source = self.document("diagram.md", "```mermaid\ngraph LR\n  " +
                               " --> ".join(nodes) + "\n```\n")
        fit = self.export(source, ("--diagram-mode", "fit"), name="fit.pdf")
        tiled = self.export(source, ("--diagram-mode=tile", "--overlap", "18"),
                            name="tile.pdf")
        self.assertGreater(self.page_count(tiled), self.page_count(fit),
                           "tile mode must not silently shrink an oversized diagram")
        self.export(source, ("--diagram-mode=auto",), name="auto.pdf")
        fallback = self.export(source, ("--no-diagrams",), name="source.pdf")
        self.assertIn("graph LR", self.pdf_text(fallback))

    def test_diagram_is_vector_not_embedded_bitmap(self):
        source = self.document("vector.md", "```mermaid\ngraph LR\nA[Alpha] --> B[Beta]\n```\n")
        path = self.export(source, ("--diagram-mode", "fit"))
        images = self.inspect("pdfimages", ["-list", path]).decode(errors="replace")
        self.assertFalse(re.search(r"^\s*\d+\s+\d+\s+", images, re.M),
                         "diagram unexpectedly contains a raster image:\n" + images)
        # Check that 'no images' did not merely mean a blank diagram page.
        prefix = self.work / "preview"
        self.inspect("pdftoppm", ["-f", "1", "-singlefile", "-scale-to", "500", path, prefix])
        ppm = prefix.with_suffix(".ppm").read_bytes()
        match = re.match(rb"P6\s+(?:#[^\n]*\n\s*)*(\d+)\s+(\d+)\s+255\s", ppm)
        self.assertIsNotNone(match, "unexpected PPM output")
        pixels = ppm[match.end():]
        self.assertEqual(len(pixels), int(match[1]) * int(match[2]) * 3)
        self.assertGreater(sum(value < 230 for value in pixels), 100,
                           "vector diagram rendered as a blank page")

    def test_io_errors_and_atomic_destination_replacement(self):
        self.run_cli(["--export-pdf", self.work / "missing-parent" / "out.pdf", self.source],
                     expected=1)
        directory = self.work / "destination-directory"
        directory.mkdir()
        marker = directory / "keep"
        marker.write_bytes(b"keep")
        self.run_cli(["--export-pdf", directory, self.source], expected=1)
        self.assertEqual(marker.read_bytes(), b"keep")
        self.assertEqual(list(self.work.glob(".mdglass-*.tmp")), [], "temporary export directory leaked")
        # An existing valid PDF should be replaced, not appended to.
        path = self.export(name="replace.pdf")
        path.write_bytes(path.read_bytes() + b"STALE_TRAILER_SENTINEL")
        self.export(self.document("replacement.md", "REPLACEMENTMARK\n"), name="replace.pdf")
        self.assertNotIn(b"STALE_TRAILER_SENTINEL", path.read_bytes())
        self.assertIn("REPLACEMENTMARK", self.pdf_text(path))
        self.assertEqual(path.stat().st_mode & 0o077, 0, "export lost private file permissions")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: output_smoke.py PATH_TO_MDGLASS")
    MDGLASS = os.path.abspath(sys.argv[1])
    if not os.path.isfile(MDGLASS) or not os.access(MDGLASS, os.X_OK):
        sys.exit(f"not an executable: {MDGLASS}")
    absent = [name for name, path in TOOLS.items() if path is None]
    if absent:
        print("optional inspectors unavailable: " + ", ".join(absent), flush=True)
    unittest.main(argv=[sys.argv[0]], verbosity=2)
