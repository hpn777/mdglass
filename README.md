# mdglass

A fast, tiny Markdown viewer for Linux, for the terminal and the GNOME desktop.

![MD Glass showing a README](packaging/screenshots/main.png)

- One small binary (~0.6 MB), no WebKit, no runtime to install
- Terminal pager with search, or a native GNOME window (`--gui`)
- Tables, task lists, quotes and highlighted code blocks
- Find, zoom, and live reload when the file changes

## Install

On Ubuntu 22.04 or newer:

```bash
git clone https://github.com/hpn777/mdglass && cd mdglass
./install.sh --with-desktop     # adds "MD Glass" to the app grid and "Open With"
```

Or build a `.deb` with `make deb` and install it with `sudo apt install ./dist/mdglass_*.deb`.

## Usage

```bash
mdglass README.md           # view in the terminal
mdglass --gui README.md     # open in a GNOME window
cat notes.md | mdglass      # read from stdin
```

In the terminal, press `/` to search, `n` for the next match, `]` `[` to switch files, and `q` to quit.
In the window, use `Ctrl+F` to find, `Ctrl+O` to open, and `Ctrl++` / `Ctrl+-` to zoom.

See `man mdglass` for all options.

## Development

`make check` runs the unit, terminal and GUI tests.

## License

MIT
