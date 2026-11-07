# mdglass

A fast, tiny Markdown viewer for Linux, for the terminal and the GNOME desktop.

![MD Glass showing a README](packaging/screenshots/main.png)

- One small binary (~0.6 MB), no WebKit, no runtime to install
- Terminal pager with search, or a native GNOME window (`--gui`)
- Tables, task lists, quotes and highlighted code blocks
- Find, zoom, and live reload when the file changes

## Install

Download from the [latest release](https://github.com/hpn777/mdglass/releases/latest):

```bash
sudo apt install ./mdglass_0.1.0-3_amd64.deb            # Ubuntu 22.04+ / Debian
flatpak install --user ./mdglass-0.1.0-x86_64.flatpak   # any distro (GNOME window)
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
```` ```mermaid ```` blocks and untyped box-art blocks are drawn as diagrams: vector
graphics in the window (in a palette that follows your light/dark setting), and honest
ASCII box art in the terminal, where node names stay searchable text. The terminal
draws flowcharts, state machines, ER models, sequence diagrams and pie charts; syntax
it cannot draw truthfully stays code. Press `D` to flip
between art and source, or start with `--no-diagrams`. Any diagram that cannot be drawn
truthfully at your width stays a code block; piped output stays markdown source
(unless `--color` forces art), so `mdglass file.md | grep` works.
`examples/diagrams.md` demos every kind, with `examples/screenshots/` as proof.

See `man mdglass` for all options.

## Development

`make check` runs the unit, terminal and GUI tests.

## License

MIT
