// mdglass - super fast terminal markdown viewer.

mod ansi;
mod boxart;
mod doc;
mod gui;
mod pager;
mod vector;
mod render;
mod output;

use std::io::{self, Read, Write};

use render::{render, Cfg};

const HELP: &str = "\
mdglass - super fast terminal markdown viewer

USAGE:
    mdglass [OPTIONS] FILE...
    cat README.md | mdglass -

OPTIONS:
    -n, --no-pager    print rendered output to stdout (no interactive pager)
    -g, --gui         open a native GNOME preview window (GTK4 + libadwaita)
        --export-pdf PATH  export one document as paginated PDF (no display needed)
        --paper SIZE   a4 (default), a3, letter, legal
        --landscape    landscape output pages
        --margin PT    page margin in points (default 36)
        --font-size PT output font size in points (default 10)
        --table-mode MODE    auto (default), fit, tile
        --diagram-mode MODE  auto (default), fit, tile
        --no-repeat-first-column  don't repeat identifier column in table bands
        --overlap PT   diagram tile overlap in points (default 12)
    -c, --plain       no colors (default when stdout is not a terminal)
        --color       force colors, e.g. `mdglass -n --color FILE | less -R`
        --no-urls     don't show link URLs next to link text
        --no-diagrams render mermaid fences as source (box art is on by default)
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
    D                 toggle mermaid box art / source
    q Esc               quit (moves on to the next file, if any)
";

fn main() {
    let mut paths: Vec<String> = Vec::new();
    let mut no_pager = false;
    let mut gui = false;
    let mut forced_color: Option<bool> = None;
    let mut urls = true;
    let mut diagrams = true;
    let mut export_path: Option<String> = None;
    let mut output_settings = output::Settings::default();
    let mut output_options = false;

    let stdin_piped = !termion::is_tty(&io::stdin());
    let stdout_tty = termion::is_tty(&io::stdout());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--" { paths.extend(args); break; }
        let (option, inline_value) = a.split_once('=').map_or((a.as_str(), None), |(k, v)| (k, Some(v)));
        if matches!(option, "--export-pdf" | "--paper" | "--margin" | "--font-size" | "--table-mode" | "--diagram-mode" | "--overlap") {
            let value = inline_value.map(str::to_owned).or_else(|| args.next())
                .unwrap_or_else(|| usage_error(&format!("{option} requires a value")));
            match option {
                "--export-pdf" => {
                    if export_path.replace(value).is_some() { usage_error("--export-pdf may only be specified once"); }
                }
                "--paper" => {
                    output_options = true;
                    output_settings.paper = match value.to_ascii_lowercase().as_str() {
                        "a4" => output::Paper::A4, "a3" => output::Paper::A3,
                        "letter" => output::Paper::Letter, "legal" => output::Paper::Legal,
                        _ => usage_error("paper must be a4, a3, letter, or legal"),
                    };
                }
                "--margin" | "--font-size" | "--overlap" => {
                    output_options = true;
                    let number = value.parse::<f64>().unwrap_or_else(|_| usage_error(&format!("{option} requires a number in points")));
                    match option { "--margin" => output_settings.margin = number,
                        "--font-size" => output_settings.font_size = number, _ => output_settings.overlap = number }
                }
                _ => {
                    output_options = true;
                    let mode = match value.as_str() {
                        "auto" => output::FitMode::Auto, "fit" => output::FitMode::Fit, "tile" => output::FitMode::Tile,
                        _ => usage_error("layout mode must be auto, fit, or tile"),
                    };
                    if option == "--table-mode" { output_settings.table_mode = mode; }
                    else { output_settings.diagram_mode = mode; }
                }
            }
            continue;
        }
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("mdglass {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-n" | "--no-pager" => no_pager = true,
            "-g" | "--gui" => gui = true,
            "-c" | "--plain" | "--no-color" => forced_color = Some(false),
            "--color" => forced_color = Some(true),
            "--no-urls" => urls = false,
            "--no-diagrams" => diagrams = false,
            "--landscape" => { output_options = true; output_settings.landscape = true; }
            "--no-repeat-first-column" => { output_options = true; output_settings.repeat_first_column = false; }
            "-" => paths.push("-".into()),
            _ if a.starts_with('-') && a.len() > 1 => {
                eprintln!("mdglass: unknown option {a} (try --help)");
                std::process::exit(2);
            }
            _ => paths.push(a),
        }
    }
    if export_path.is_some() && (gui || no_pager || forced_color.is_some()) {
        usage_error("--export-pdf cannot be combined with --gui, --no-pager, or color options");
    }
    if export_path.is_none() && output_options { usage_error("page layout options require --export-pdf"); }
    if export_path.is_some() {
        output_settings.diagrams = diagrams;
        output_settings.urls = urls;
        output_settings.validate().unwrap_or_else(|e| usage_error(&e));
        if paths.len() > 1 { usage_error("--export-pdf accepts exactly one input document"); }
        if export_path.as_deref() == Some("") || export_path.as_deref() == Some("-") {
            usage_error("PDF destination must be a filename, not stdout");
        }
        if let (Some(input), Some(dest)) = (paths.first(), export_path.as_ref()) {
            if input != "-" && same_file(std::path::Path::new(input), std::path::Path::new(dest)) {
                usage_error("PDF destination must not replace the input document");
            }
        }
    }
    let color = forced_color.unwrap_or_else(|| {
        stdout_tty && std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").as_deref() != Ok("dumb")
    });
    // diagrams only where someone can read them: piped output without
    // --color must reproduce the markdown source (grep for '-->' still
    // works), the pager on any TTY gets them (ASCII when colorless)
    let diagrams = diagrams && (color || stdout_tty);
    if paths.is_empty() && gui && !stdin_piped {
        // launched from the app grid or with no argument: empty window with Open…
        if let Err(e) = gui::run(Vec::new(), None) {
            eprintln!("mdglass: {e}");
            std::process::exit(1);
        }
        return;
    }
    if paths.is_empty() {
        if stdin_piped {
            paths.push("-".into());
        } else {
            eprint!("{HELP}");
            std::process::exit(2);
        }
    }

    if let Some(destination) = export_path.as_ref() {
        if paths.iter().any(|p| p == "-") && stdin_is_file(std::path::Path::new(destination)) {
            usage_error("PDF destination must not replace the redirected input document");
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
                eprintln!("mdglass: {}: {e}", if path == "-" { "stdin" } else { path });
                failed = true;
            }
        }
    }
    if files.is_empty() {
        std::process::exit(if failed { 1 } else { 2 });
    }

    if let Some(path) = export_path {
        if failed { std::process::exit(1); }
        match output::export_pdf(&files[0].1, std::path::Path::new(&path), &output_settings) {
            Ok(warnings) => for warning in warnings { eprintln!("mdglass: warning: {warning}"); },
            Err(e) => { eprintln!("mdglass: {e}"); std::process::exit(1); }
        }
        return;
    }

    if gui {
        // empty stdin (e.g. /dev/null when started from the app grid) is no document
        let stdin_content = files
            .iter()
            .find(|(n, c)| n == "-" && !c.is_empty())
            .map(|(_, c)| c.clone());
        let paths: Vec<String> =
            files.iter().filter(|(n, _)| n != "-").map(|(n, _)| n.clone()).collect();
        if let Err(e) = gui::run(paths, stdin_content) {
            eprintln!("mdglass: {e}");
            std::process::exit(1);
        }
        return;
    }
    if no_pager || !stdout_tty {
        print_plain(&files, color, urls, diagrams);
    } else if let Err(e) = pager::run(&files, color, urls, diagrams) {
        // No usable terminal for raw mode: fall back to a plain dump.
        eprintln!("mdglass: pager unavailable ({e}), printing instead");
        print_plain(&files, color, urls, diagrams);
    }
    if failed {
        std::process::exit(1);
    }
}

fn usage_error(message: &str) -> ! {
    eprintln!("mdglass: {message} (try --help)");
    std::process::exit(2)
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // Also protect a cached GUI document whose file disappeared while the
    // chooser was open: metadata alone cannot recognize that same pathname.
    fn normalized(path: &std::path::Path) -> std::path::PathBuf {
        if let Ok(path) = std::fs::canonicalize(path) { return path; }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            let parent = if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { parent };
            if let Ok(parent) = std::fs::canonicalize(parent) { return parent.join(name); }
        }
        let path = if path.is_absolute() { path.to_path_buf() }
            else { std::env::current_dir().unwrap_or_default().join(path) };
        let mut result = std::path::PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {},
                std::path::Component::ParentDir => { result.pop(); },
                component => result.push(component.as_os_str()),
            }
        }
        result
    }
    if normalized(a) == normalized(b) { return true; }
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

fn stdin_is_file(path: &std::path::Path) -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let input = io::stdin().as_fd().try_clone_to_owned().ok()
        .and_then(|fd| std::fs::File::from(fd).metadata().ok());
    match (input, std::fs::metadata(path)) {
        (Some(input), Ok(dest)) if input.is_file() => input.dev() == dest.dev() && input.ino() == dest.ino(),
        _ => false,
    }
}

fn print_plain(files: &[(String, String)], color: bool, urls: bool, diagrams: bool) {
    let width = termion::terminal_size().map(|(w, _)| w as usize).unwrap_or(80);
    let cfg = Cfg { diagrams, ..Cfg::new(color, width, urls) };
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

#[cfg(test)]
mod cli_tests {
    #[test]
    fn same_file_protects_missing_cached_document_paths() {
        let cwd = std::env::current_dir().unwrap();
        assert!(super::same_file(std::path::Path::new("missing-output-source.md"),
            &cwd.join("missing-output-source.md")));
        assert!(super::same_file(std::path::Path::new("missing-output-dir/../missing-output-source.md"),
            &cwd.join("missing-output-source.md")));
        assert!(!super::same_file(std::path::Path::new("missing-first.md"),
            std::path::Path::new("missing-second.md")));
    }
}
