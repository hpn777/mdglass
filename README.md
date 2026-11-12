# mdglass

A fast, tiny Markdown viewer for Linux, for the terminal and the GNOME desktop.

![MD Glass showing a README](packaging/screenshots/main.png)

- One small binary (~6 MB), no WebKit, no runtime to install
- Terminal pager with search, or a native GNOME window (`--gui`)
- Tables, task lists, quotes, highlighted code blocks and diagrams
- Find, zoom, and live reload when the file changes
- Native printing and headless, paginated PDF export, with repeating table headers

## Install

Download from the [latest release](https://github.com/hpn777/mdglass/releases/latest):

```bash
sudo apt install ./mdglass_*_amd64.deb       # Ubuntu 22.04+ / Debian 12+; the
                                             # --gui window wants GTK 4.12+
                                             # (Ubuntu 23.10+, Debian 13+)
flatpak install --user ./mdglass-*.flatpak   # any distro, GNOME window included
```

Or from source:

```bash
git clone https://github.com/hpn777/mdglass && cd mdglass
./install.sh --with-desktop     # adds "MD Glass" to the app grid and "Open With"
```

## Usage

```bash
mdglass README.md           # view in the terminal
mdglass --gui README.md     # open in a GNOME window
cat notes.md | mdglass      # read from stdin
```

In the terminal, press `/` to search, `n` for the next match, `]` `[` to switch files, and `q` to quit.
In the window, use `Ctrl+F` to find, `Ctrl+O` to open, and `Ctrl++` / `Ctrl+-` to zoom.
Prose wraps to the window; wide tables and diagrams keep their shape and scroll sideways.
Those wraps are real line breaks, as in the terminal: find does not match a phrase split
across two lines, copied text keeps the breaks, and resizing or zooming clears a selection.
```` ```mermaid ```` blocks are drawn as diagrams: vector graphics in the window (in a
palette that follows your light/dark setting), and honest ASCII box art in the terminal,
where node names stay searchable text. Blocks that are untyped but look like box art are
drawn as vectors in the window too. The terminal draws flowcharts, state machines, ER
models, sequence diagrams and pie charts; anything it cannot draw truthfully at your
width, or with syntax it does not fully cover, stays a code block. Press `D` to flip
between art and source, or start with `--no-diagrams`. Piped output stays markdown source
(unless `--color` forces art), so `mdglass file.md | grep` works.
`examples/diagrams.md` demos every kind, with `examples/screenshots/` as proof.

See `man mdglass` for all options.

## Printing and PDF export

In the window, use **Print…** (`Ctrl+P`) or **Export as PDF…** from the menu.
Printing uses the selected printer's page geometry, not the window width or zoom.
Both actions offer paper, font, margin, table and diagram layout controls;
preferences persist for the current window session.
PDF export runs in the background with a Cancel button; it uses a document
snapshot, so reloading or closing the document cannot change the export.
For headless export:

```bash
mdglass --export-pdf notes.pdf notes.md
mdglass --export-pdf wide.pdf --paper a4 --landscape --table-mode tile notes.md
cat notes.md | mdglass --export-pdf notes.pdf -
```

Output settings use points (72 per inch): `--margin 36`, `--font-size 10`,
`--paper a4|a3|letter|legal`, and `--landscape`. Tables wrap without truncating
cells and repeat headers across pages. `--table-mode auto|fit|tile` controls
fitting versus column bands; bands repeat the first identifier column unless
`--no-repeat-first-column` is set. Box-drawing tables inside code fences remain
code, not semantic tables.

Diagrams use the built-in engines, never an external renderer. Use
`--diagram-mode auto|fit|tile` and `--overlap 12` for oversized diagrams.
Tiling preserves geometry with overlap and continuation labels; it cannot
guarantee semantic breaks between diagram nodes. `--no-diagrams` exports
source instead. Unsupported diagrams stay source with a diagnostic.
`--no-urls` omits printed link targets; link labels remain readable text.

The executable remains one file, with no new Cargo dependencies or browser.
PDF output lazily loads system **Cairo, Pango and PangoCairo**; diagram output
also uses **librsvg**. It needs installed fonts, but no GTK initialization,
display server, Python, Node, or conversion tools. On Debian/Ubuntu these
libraries are supplied by `libcairo2`, `libpango-1.0-0`, `libpangocairo-1.0-0`
and `librsvg2-2`. Native printing additionally needs the existing GTK GUI
libraries and a working system print service/desktop portal. This is a single
distributed binary, **not a statically self-contained graphics stack**.

PDFs are written to a private temporary file beside the destination and moved
into place only after successful completion. The CLI refuses to overwrite its
Markdown input. PDF inspection tools used by the test suite are not runtime
dependencies.
Cancellation is cooperative between layout operations and pages; an in-flight
diagram-renderer call must return before cancellation can finish.

## Development

`make check` runs the unit, terminal and GUI tests.

## License

MIT
