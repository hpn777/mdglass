# mdv

**md**ark**d**o**v**iewer — a super fast terminal markdown viewer in Rust.

- ~0.5 MB static-ish binary, **one** external crate (`termion`); everything else — markdown
  parser, ANSI renderer, code highlighter, table layout, pager — is hand-rolled.
- Renders 2.8 MB of markdown in ~0.12 s; ordinary docs in low single-digit milliseconds.
- Built-in less-style pager: scroll, jump, incremental search with match highlighting,
  multi-file browsing, resize-aware redraw.

## Install (Ubuntu)

```bash
./install.sh                 # builds release and installs to /usr/local/bin
./install.sh --with-desktop  # ...and adds "Open with mdv" to the file manager
./install.sh --uninstall
```

Or manually:

```bash
cargo build --release
sudo install -Dm755 "$(cargo metadata --no-deps --format-version 1 \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/mdv" /usr/local/bin/mdv
```

The installer sets up rustup (minimal profile) automatically if `cargo` is missing.

## Usage

```bash
mdv README.md              # interactive pager
mdv a.md b.md c.md         # multiple files, cycle with [ and ]
cat notes.md | mdv -       # from stdin
mdv -n big.md | grep ...   # rendered output to stdout, no pager
mdv -c file.md             # plain text, no colors (also honors NO_COLOR / TERM=dumb)
mdv --no-urls file.md      # hide " (url)" suffixes after links
```

## Keys

| key                | action                    | key              | action                 |
|--------------------|---------------------------|------------------|------------------------|
| `j`/`k`, `↑`/`↓`   | line up/down              | `/`              | search                 |
| `Ctrl-D`/`Ctrl-U`  | half page                 | `n` / `N`        | next / previous match  |
| `Ctrl-F`/`Ctrl-B`, `Space` | full page         | `[` / `]`        | previous / next file   |
| `gg` / `G`, `Home`/`End` | top / bottom      | `q`, `Esc`       | quit (advances file)   |

## What it renders

Headings (ATX + setext), **bold**/*italic*/~~strike~~, `inline code`, fenced code blocks
with generic syntax highlighting (strings, comments, numbers, keywords, functions, types),
nested blockquotes, nested/ordered/task lists, links (clickable OSC 8 + optional visible
URL), images as `◈ alt`, GitHub tables with `:---:` alignment, horizontal rules, entities,
backslash escapes, HTML tags stripped, YAML front matter hidden, CJK-safe wrapping.

## Development

```bash
cargo test          # 13 unit tests for the renderer + ANSI layer
make install        # build + install
```

## License

MIT
