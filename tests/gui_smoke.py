#!/usr/bin/env python3
"""End-to-end checks of `mdglass --gui` in a real GTK4/libadwaita window, without
a monitor: the window runs on a private Broadway display and is driven by the
MDGLASS_GUI_SCRIPT test hook (actions, find, Escape, screenshots via GTK's own
renderer). Fails on any GTK warning/critical, crash, or wrong find count.
usage: gui_smoke.py path/to/mdglass   (skips with exit 0 if gtk4-broadwayd is absent)"""
import os, shutil, subprocess, sys, tempfile, threading, time

MDGLASS = os.path.abspath(sys.argv[1])
if not shutil.which("gtk4-broadwayd"):
    print("skip gui checks: gtk4-broadwayd not installed")
    sys.exit(0)

DISPLAY = ":31"
work = tempfile.mkdtemp(prefix="mdglass-gui-")
broadway = subprocess.Popen(["gtk4-broadwayd", DISPLAY], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(0.8)
failures = 0

def run(args, script, stdin=None, timeout=40):
    env = dict(os.environ, GDK_BACKEND="broadway", BROADWAY_DISPLAY=DISPLAY,
               MDGLASS_GUI_SCRIPT=script,
               # portals of the host session are not needed and may be slow
               ADW_DISABLE_PORTAL="1", GDK_DEBUG="no-portals")
    p = subprocess.run([MDGLASS, "--gui", *args], env=env, input=stdin, capture_output=True,
                       timeout=timeout, cwd=work)
    return p.returncode, p.stderr.decode(errors="replace")

def check(name, ok, detail=""):
    global failures
    print(("ok   " if ok else "FAIL ") + name + ("" if ok else f"\n     {detail}"))
    failures += 0 if ok else 1

def gtk_noise(err):
    return [l for l in err.splitlines() if "-WARNING" in l or "-CRITICAL" in l or "-ERROR" in l]

def shot_ok(path):
    return os.path.exists(path) and os.path.getsize(path) > 2000

try:
    doc = os.path.join(work, "doc.md")
    other = os.path.join(work, "other.md")
    open(doc, "w").write("""# Title

Some *text* with `code`, a [link](https://example.com) and an item.

> quoted item that is long enough to wrap around inside the window at the default size,
> so the continuation has to line up after the bar.

- item one
  - nested item
1. [x] done

| a | b |
|---|--:|
| 1 | 22 |

```rust
fn main() {}
```
""")
    open(other, "w").write("# Other file\n\nsecond document\n")

    p = lambda n: os.path.join(work, n)
    rc, err = run([doc, other], ";".join([
        f"shot:{p('start.png')}", "action:find", "find:item", "next", "next",
        f"shot:{p('find.png')}", "esc", "action:zoom-in", "action:zoom-in",
        f"shot:{p('zoom.png')}", "action:zoom-reset", "action:next-file",
        f"shot:{p('other.png')}", "action:prev-file", "action:reload", "quit"]))
    check("scripted session exits cleanly", rc == 0, f"exit {rc}\n{err[-600:]}")
    check("no GTK warnings or criticals", not gtk_noise(err), "\n     ".join(gtk_noise(err)[:5]))
    check("find counts matches", 'hits label = "1 of 4"' in err, err[-400:])
    for n in ["start", "find", "zoom", "other"]:
        check(f"screenshot {n}.png rendered", shot_ok(p(n + ".png")))

    rc, err = run([], f"shot:{p('empty.png')};action:open;quit", stdin=b"")
    check("no file: empty state + open dialog", rc == 0 and shot_ok(p("empty.png")) and not gtk_noise(
        "\n".join(l for l in err.splitlines() if "Failed to measure available space" not in l)), err[-400:])

    rc, err = run([], f"shot:{p('stdin.png')};quit", stdin=b"# piped\n\ntext\n")
    check("document from stdin", rc == 0 and shot_ok(p("stdin.png")), err[-400:])

    live = p("live.md")
    open(live, "w").write("# one\n")
    def edit():
        time.sleep(2.5)
        open(live, "w").write("# two\n\nchanged\n")
    threading.Thread(target=edit).start()
    noop = ";".join(["action:zoom-reset"] * 7)
    rc, err = run([live], f"{noop};quit")
    check("live reload survives an edit on disk", rc == 0 and not gtk_noise(err), err[-400:])
finally:
    broadway.terminate()
    shutil.rmtree(work, ignore_errors=True)

print("\nall gui checks passed" if not failures else f"\n{failures} gui check(s) failed")
sys.exit(1 if failures else 0)
