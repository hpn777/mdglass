#!/usr/bin/env python3
"""End-to-end checks of `mdglass --gui` in a real GTK4/libadwaita window, without
a monitor: the window runs on a private Broadway display and is driven by the
MDGLASS_GUI_SCRIPT test hook (actions, find, Escape, screenshots via GTK's own
renderer). Fails on any GTK warning/critical, crash, or wrong find count.
Also decodes the screenshots (tests/pngread.py, stdlib-only) and asserts the
numeric grid/diagram/quote/prose invariants of docs/rendering-quality-plan.md.
usage: gui_smoke.py path/to/mdglass   (skips with exit 0 if gtk4-broadwayd is absent)"""
import os, shutil, subprocess, sys, tempfile, threading, time
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pngread

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

# ------------------------------------------- screenshot pixel measurements ---
# A pixel is "lit" when it differs from the text view's own background (corner
# pixel, which also decides the light/dark label reported) by more than 60 in
# summed |dR|+|dG|+|dB|. That background is global per shot: code-block
# paragraph backgrounds (alpha 0.18 grey) differ by only ~35 and stay "dark",
# while faint table frames (~215) and box-drawing (~650) are clearly "lit" in
# either Adwaita theme.

def lit_xs(img, y, x0, x1):
    """x positions of pixels lit against the shot's view background"""
    row = img.row(y)
    r0, g0, b0 = img.rgb(6, img.h - 8)
    return [x for x in range(x0, x1)
            if abs(row[x][0] - r0) + abs(row[x][1] - g0) + abs(row[x][2] - b0) > 60]

def longest_run(xs):
    """(length, end x) of the longest consecutive run, starting at xs[0] if any"""
    best_len, best_end, cur = 1, xs[0], 1
    for a, b in zip(xs, xs[1:]):
        cur = cur + 1 if b == a + 1 else 1
        if cur > best_len:
            best_len, best_end = cur, b
    return best_len, best_end

def run_from(xs, x0):
    """end x of the consecutive run that starts at x0 (0 if x0 not lit)"""
    if not xs or xs[0] != x0:
        return 0
    end = x0
    for a, b in zip(xs, xs[1:]):
        if b != a + 1:
            break
        end = b
    return end

def border_rows(img, min_run, y0=56):
    """rows holding a horizontal lit run longer than min_run, grouped into
    consecutive-y segments: [(first_y, last_y, run_start, run_end), ...]"""
    rows, out = [], []
    for y in range(y0, img.h):
        xs = lit_xs(img, y, 4, img.w - 4)
        n, end = (longest_run(xs) if xs else (0, 0))
        if n > min_run:
            rows.append((y, xs[0], end))
    for y, start, end in rows:
        if out and y == out[-1][1] + 1:
            out[-1][1] = y
            out[-1][2] = min(out[-1][2], start)
            out[-1][3] = max(out[-1][3], end)
        else:
            out.append([y, y, start, end])
    return out

def theme(img):
    return "dark" if img.gray(6, img.h - 8) < 128 else "light"

def cjk_check(path):
    """Table border rows frame the body; every text scanline between the header
    separator and the bottom border must end within ±1 px of the separator's
    right edge (before Phase 3 a CJK cell drifted ~20 px by font fallback)."""
    img = pngread.open(path)
    segs = border_rows(img, 380)
    if len(segs) != 3:
        return False, f"expected 3 table border row segments, found {len(segs)}: {segs}"
    (t0, t1, tl, tr), (s0, s1, _, sr), (b0, b1, _, br) = segs
    sep_right = max(max(lit_xs(img, y, tl - 6, max(sr, tr) + 12)) for y in (s0, s1))
    offs, rows = [], 0
    for y in range(s1 + 1, b0):
        xs = lit_xs(img, y, tl - 6, sep_right + 12)
        if xs:
            rows += 1
            offs.append(max(xs) - sep_right)
    if not offs:
        return False, "no text scanlines between header separator and bottom border"
    return all(abs(o) <= 1 for o in offs), (
        f"theme={theme(img)} separator right edge x={sep_right} (y{s0}-{s1}), {rows} lit text scanlines "
        f"y{s1 + 1}..{b0 - 1}, right-edge offsets min {min(offs)} max {max(offs)} px (need all within ±1)")

def boxes_check(wide_path, narrow_path):
    """The 150-col box's top border must run unbroken to the viewport edge
    (pre-wrap never wraps art; before Phase 2 GTK wrapped it at ~x=991 and it
    continued on extra lines). The 60-col box must keep its left border column
    lit on every scanline between top+4 and bottom-4."""
    img = pngread.open(wide_path)
    segs = border_rows(img, 380)
    if not segs:
        return False, "wide box: no border row (run > 380 px) found at all"
    t0, t1, tl, _ = segs[0]
    end = run_from(lit_xs(img, t1, 4, img.w), tl)
    wide_ok = end >= img.w - 3
    img2 = pngread.open(narrow_path)
    segs2 = border_rows(img2, 380)
    if len(segs2) != 2:
        return False, f"narrow box: expected 2 border row segments, found {len(segs2)}: {segs2}"
    (n0, n1, nl, nr), (b0, _, _, _) = segs2
    gaps = [y for y in range(n0 + 4, b0 - 3) if not lit_xs(img2, y, max(4, nl - 2), nl + 8)]
    return wide_ok and not gaps, (
        f"wide box: top border unbroken x={tl}..{end} (must reach >= {img.w - 3}, window {img.w} wide) => "
        f"{'no wrap' if wide_ok else 'WRAPPED'}; narrow box: left border x={nl}, rows {n0 + 4}..{b0 - 4}, "
        f"{len(gaps)} gap rows {gaps[:6]}")

def quote_check(path):
    """The blue quote bar must cover every line from the quote's first to its
    last row with no gap > 4 px. Chosen formulation (simpler and reliable vs.
    per-band edge tests): every inner block type carries the bar and the bar
    glyphs of adjacent lines nearly touch, so a bar missing on any inner block
    type (code lines, blank '> ' lines, table rows: the pre-Phase-1 defects)
    leaves a gap of a line's glyph height (>= 14 px), far above the tolerance.
    The bar column is derived from the shot, which also excludes the blinking
    text caret at x=28..29 (lit on far fewer rows than the bar)."""
    img = pngread.open(path)
    counts = Counter()
    for y in range(56, img.h):
        for x in lit_xs(img, y, 26, 45):
            counts[x] += 1
    if not counts:
        return False, "no quote bar pixels found in x 26..44"
    peak = max(counts.values())
    bar_xs = sorted(x for x, c in counts.items() if c >= max(20, peak // 4))
    bx0, bx1 = bar_xs[0], bar_xs[-1]
    bar_rows = {y for y in range(56, img.h) if lit_xs(img, y, bx0, bx1 + 1)}
    q0, q1 = min(bar_rows), max(bar_rows)
    span, gap, maxgap = q1 - q0 + 1, 0, 0
    for y in range(q0, q1 + 1):
        gap = gap + 1 if y not in bar_rows else 0
        maxgap = max(maxgap, gap)
    bands, cur = 0, False
    for y in range(q0, q1 + 1):
        lit = bool(lit_xs(img, y, 44, img.w - 40))
        bands, cur = bands + (1 if lit and not cur else 0), lit
    ok = maxgap <= 4 and span >= 60 and bands >= 5
    return ok, (
        f"bar x {bx0}..{bx1} lit on {len(bar_rows)}/{span} rows y{q0}..{q1}, max gap {maxgap} px (need <= 4), "
        f"{bands} text bands beside the bar (need >= 5)")

def prose_check(path):
    """Pre-wrapped prose stops at the column budget; with WRAP_NONE the old
    GTK-wrap overflow zone (between the wrap edge and the scrollbar) must stay
    empty and no line may reach near the window edge."""
    img = pngread.open(path)
    z0, z1 = 955, 1008  # overflow zone x 955..1007; the v-scrollbar sits at x >= 1012
    hits, right = [], 0
    for y in range(56, img.h):
        xs = lit_xs(img, y, 4, img.w - 16)
        if xs:
            right = max(right, max(xs))
            zz = [x for x in xs if z0 <= x < z1]
            if zz:
                hits.append((y, zz[0]))
    ok = not hits and right <= img.w - 64
    return ok, (
        f"rightmost text pixel x={right} (limit {img.w - 64}), lit pixels in overflow zone x {z0}..{z1 - 1}: "
        f"{len(hits)} rows {hits[:5]}")

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

    # ---- rendering-quality fixtures (docs/rendering-quality-plan.md) --------
    box = lambda w: ("```\n" + "╭" + "─" * (w - 2) + "╮\n│" + " " * (w - 2) + "│\n"
                     "│" + " " * (w - 2) + "│\n╰" + "─" * (w - 2) + "╯\n```\n")
    open(p("q_cjk.md"), "w").write("""# Grid

| column a | column b | column c | column d |
|---|---|---|---|
| alpha | beta | 表格与图表 | delta |
| echo | foxtrot | golf | hotel |
| india | juliett | kilo | lima |
""")
    open(p("q_wide.md"), "w").write(box(150))
    open(p("q_narrow.md"), "w").write(box(60))
    open(p("q_quote.md"), "w").write("""> Quoted paragraph that is long enough to wrap into a second visual line at
> the default window size, to check the bar on continuations.
> 
> ```
> one
> two
> three
> ```
> 
> | x | y |
> |---|---|
> | 1 | 22 |
""")
    open(p("q_prose.md"), "w").write("word " * 300 + """

| a | b |
|---|---|
| 1 | 2 |
""")
    qnames = ["q_cjk", "q_wide", "q_narrow", "q_quote", "q_prose"]
    steps = []
    for i, n in enumerate(qnames):
        if i:
            steps.append("action:next-file")
        steps.append(f"shot:{p(n + '.png')}")
    steps.append("quit")
    rc, err = run([p(n + ".md") for n in qnames], ";".join(steps))
    check("quality session exits cleanly", rc == 0, f"exit {rc}\n{err[-600:]}")
    check("quality session has no GTK warnings", not gtk_noise(err), "\n     ".join(gtk_noise(err)[:5]))

    def quality(name, fn, *paths):
        try:
            ok, detail = fn(*paths)
        except Exception as e:  # undecodable shot must read as a normal failure
            ok, detail = False, f"{type(e).__name__}: {e}"
        check(name, ok, detail)
        if ok:  # these checks exist for their numbers: print them when green too
            print("     " + detail)

    quality("CJK table rows end at the header separator edge", cjk_check, p("q_cjk.png"))
    quality("wide box unwrapped, narrow box border unbroken", boxes_check, p("q_wide.png"), p("q_narrow.png"))
    quality("quote bar covers every inner block line", quote_check, p("q_quote.png"))
    quality("prose never enters the overflow zone", prose_check, p("q_prose.png"))

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
