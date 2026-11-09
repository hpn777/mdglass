# mdglass

A fast, tiny Markdown viewer for Linux, for the terminal and the GNOME desktop.

![MD Glass showing a README](packaging/screenshots/main.png)

- One small binary (~6 MB), no WebKit, no runtime to install
- Terminal pager with search, or a native GNOME window (`--gui`)
- Tables, task lists, quotes, highlighted code blocks and diagrams
- Find, zoom, and live reload when the file changes

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

## Development

`make check` runs the unit, terminal and GUI tests.

## License

MIT
