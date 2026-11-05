# mdv

A super fast terminal markdown viewer in Rust.

- ~0.5 MB binary, dynamically linked only against glibc. **One** external crate
  (`termion`, which pulls in `libc` and `numtoa`); the markdown parser, ANSI renderer,
  code highlighter, table layout and pager are all hand-rolled.
- Renders and word-wraps 2.8 MB of markdown in ~0.2 s; ordinary docs in 2–3 ms,
  process start included.
- Built-in less-style pager on the alternate screen: scrolling, search with match
  highlighting, multi-file browsing, re-wraps on resize.
- And a native GNOME preview: `mdv --gui` shows the same document in a GTK4 +
  libadwaita window with find, zoom and live reload. The system's GNOME
  libraries are resolved at runtime with `dlopen` — no WebKit, nothing extra
  to install, still one small binary (~0.6 MB).

## Install (Ubuntu)

Run as your normal user; the script asks for sudo only for the copy step.

```bash
./install.sh                        # build and install to /usr/local/bin
./install.sh --with-desktop         # also add mdv to the file manager's "Open With"
PREFIX=~/.local/bin ./install.sh    # per-user install, no sudo
./install.sh --uninstall
```

If `cargo` is missing, the installer sets up rustup (minimal profile), plus
`build-essential` for the linker. Alternatively: `make install`.

## Usage

```bash
mdv README.md                  # interactive pager
mdv a.md b.md c.md             # several files; ] and [ switch between them
cat notes.md | mdv -           # from stdin
mdv big.md | grep TODO         # piped: plain text, wrapped to 80 columns
mdv -n --color big.md | less -R  # keep colors when piping
mdv -c file.md                 # no colors (also honors NO_COLOR and TERM=dumb)
mdv --no-urls file.md          # hide the " (url)" after links
```

## Keys

| key                          | action                      |
|------------------------------|-----------------------------|
| `j` `k`, `↓` `↑`, `Enter`    | line down / up              |
| `d` `u`, `Ctrl-D` `Ctrl-U`   | half page down / up         |
| `f` `b`, `Space`, `PgDn` `PgUp` | page down / up           |
| `gg` `G`, `Home` `End`       | top / bottom                |
| `/` `?`                      | search forward / backward (case-insensitive) |
| `n` `N`                      | next / previous match       |
| `]` `[`                      | next / previous file        |
| `q`, `Esc`                   | quit (moves on to the next file, if any) |

## GNOME preview

```bash
mdv --gui README.md          # preview window; file managers get an "Open With" entry
```

`mdv --gui` shares the exact parser with the terminal back end and draws into
native widgets (no WebKit, no terminal emulation): a header bar with document
title, a hamburger menu, and a styled document view.

| shortcut | action |
|--------|--------|
| `Ctrl+O` | open a file |
| `Ctrl+F` | find bar — live highlighting, `Enter`/arrows cycle matches, `Esc` closes |
| `Ctrl+R`, `F5` | reload; files also reload automatically when they change on disk |
| `Ctrl++` / `Ctrl+-` / `Ctrl+0` | zoom in / out / reset |
| `Alt+←` `Alt+→` | previous / next of the files given on the command line |
| `Ctrl+W` | close |

Requires Ubuntu 22.04+ (GNOME's `libgtk-4` and `libadwaita-1`, loaded at
runtime). Images are shown as `◈ alt` text like in the terminal, and links are
styled but not clickable yet.

## What it renders

ATX and setext headings; **bold**, *italic*, ~~strike~~ and `inline code`; fenced and
indented code blocks with generic syntax highlighting (strings, comments, numbers,
keywords, function calls, types); nested blockquotes; nested, ordered and task lists;
inline and reference links (clickable OSC 8 hyperlinks, plus the URL unless
`--no-urls`); images as `◈ alt`; GitHub tables with `:---:` alignment; horizontal
rules; entities and backslash escapes; soft and hard line breaks. YAML/TOML front matter
and HTML comments are hidden, other HTML tags are stripped. Text is word-wrapped to the
terminal width with hanging indents, and CJK wide characters are measured correctly.

Not supported: footnotes, definition lists, math, and HTML rendering.

## Development

```bash
cargo test     # renderer, ANSI wrapping and pager key handling
make check     # the above plus end-to-end pty tests of the release binary
```

## License

MIT
