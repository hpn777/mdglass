// mdv - super fast terminal markdown viewer.

mod ansi;
mod doc;
mod pager;
mod render;

use std::io::{self, Read, Write};

use render::{render, Cfg};

const HELP: &str = "\
mdv - super fast terminal markdown viewer

USAGE:
    mdv [OPTIONS] FILE...
    cat README.md | mdv -

OPTIONS:
    -n, --no-pager    print rendered output to stdout (no interactive pager)
    -c, --plain       no colors (default when stdout is not a terminal)
        --color       force colors, e.g. `mdv -n --color FILE | less -R`
        --no-urls     don't show link URLs next to link text
    -h, --help        print this help
    -V, --version     print version

KEYS:
    j k  ↓ ↑  Enter     line down / up
    d u  Ctrl-D Ctrl-U  half page down / up
    f b  Space  PgDn PgUp  page down / up
    gg G  Home End      top / bottom
    / ?                 search forward / backward (case-insensitive)
    n N                 next / previous match
    ] [                 next / previous file
    q Esc               quit (moves on to the next file, if any)
";

fn main() {
    let mut paths: Vec<String> = Vec::new();
    let mut no_pager = false;
    let mut forced_color: Option<bool> = None;
    let mut urls = true;

    let stdin_piped = !termion::is_tty(&io::stdin());
    let stdout_tty = termion::is_tty(&io::stdout());
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("mdv {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-n" | "--no-pager" => no_pager = true,
            "-c" | "--plain" | "--no-color" => forced_color = Some(false),
            "--color" => forced_color = Some(true),
            "--no-urls" => urls = false,
            "-" => paths.push("-".into()),
            _ if a.starts_with('-') && a.len() > 1 => {
                eprintln!("mdv: unknown option {a} (try --help)");
                std::process::exit(2);
            }
            _ => paths.push(a),
        }
    }
    let color = forced_color.unwrap_or_else(|| {
        stdout_tty && std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").as_deref() != Ok("dumb")
    });
    if paths.is_empty() {
        if stdin_piped {
            paths.push("-".into());
        } else {
            eprint!("{HELP}");
            std::process::exit(2);
        }
    }

    let mut files: Vec<(String, String)> = Vec::new();
    let mut failed = false;
    for path in &paths {
        let bytes = if path == "-" {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf).map(|_| buf)
        } else {
            std::fs::read(path)
        };
        match bytes {
            // invalid UTF-8 (e.g. Latin-1 files) is shown with replacement chars
            Ok(b) => files.push((path.clone(), String::from_utf8_lossy(&b).into_owned())),
            Err(e) => {
                eprintln!("mdv: {}: {e}", if path == "-" { "stdin" } else { path });
                failed = true;
            }
        }
    }
    if files.is_empty() {
        std::process::exit(if failed { 1 } else { 2 });
    }

    if no_pager || !stdout_tty {
        print_plain(&files, color, urls);
    } else if let Err(e) = pager::run(&files, color, urls) {
        // No usable terminal for raw mode: fall back to a plain dump.
        eprintln!("mdv: pager unavailable ({e}), printing instead");
        print_plain(&files, color, urls);
    }
    if failed {
        std::process::exit(1);
    }
}

fn print_plain(files: &[(String, String)], color: bool, urls: bool) {
    let width = termion::terminal_size().map(|(w, _)| w as usize).unwrap_or(80);
    let cfg = Cfg::new(color, width, urls);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (name, content) in files.iter() {
        if files.len() > 1 {
            let _ = writeln!(out, "\n── {name} {}", "─".repeat(width.saturating_sub(name.len() + 4)));
        }
        for line in render(content, &cfg) {
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = out.flush();
}
