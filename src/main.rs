// mdv - super fast terminal markdown viewer.

mod ansi;
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
    -c, --plain       no colors (plain text)
        --no-urls     don't show link URLs next to link text
    -h, --help        print this help
    -V, --version     print version

KEYS:
    j/k  ↑/↓   line scroll      g/G or gg   top / bottom
    d/u  half page              f/b or SP   full page
    /    search (n / N next / previous)     [ ]   next / previous file
    Home/End  jump              q           quit (advances to next file)
";

fn main() {
    let mut paths: Vec<String> = Vec::new();
    let mut no_pager = false;
    let mut color = std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").as_deref() != Ok("dumb");
    let mut urls = true;

    let mut args = std::env::args().skip(1).peekable();
    let stdin_piped = !termion::is_tty(&io::stdin());
    let stdout_tty = termion::is_tty(&io::stdout());
    while let Some(a) = args.next() {
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
            "-c" | "--plain" | "--no-color" => color = false,
            "--no-urls" => urls = false,
            "-" => paths.push("-".into()),
            _ if a.starts_with('-') && a.len() > 1 => {
                eprintln!("mdv: unknown option {a} (try --help)");
                std::process::exit(2);
            }
            _ => paths.push(a),
        }
    }
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
        let content = if path == "-" {
            let mut buf = String::new();
            match io::stdin().read_to_string(&mut buf) {
                Ok(_) => buf,
                Err(e) => {
                    eprintln!("mdv: stdin: {e}");
                    failed = true;
                    continue;
                }
            }
        } else {
            match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("mdv: {path}: {e}");
                    failed = true;
                    continue;
                }
            }
        };
        files.push((path.clone(), content));
    }
    if files.is_empty() {
        std::process::exit(if failed { 1 } else { 2 });
    }

    if !color {
        no_pager = true;
    }
    if no_pager || !stdout_tty {
        print_plain(&files, color, urls);
    } else if let Err(e) = pager::run(&files) {
        // No usable terminal for raw mode: fall back to a plain dump.
        eprintln!("mdv: pager unavailable ({e}), printing instead");
        print_plain(&files, false, urls);
    }
    if failed {
        std::process::exit(1);
    }
}

fn print_plain(files: &[(String, String)], color: bool, urls: bool) {
    let width = if color { termion::terminal_size().map(|(w, _)| w as usize).unwrap_or(80) } else { 80 };
    let cfg = Cfg { color, width, urls };
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
