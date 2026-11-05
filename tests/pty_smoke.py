#!/usr/bin/env python3
"""End-to-end checks of the mdv binary in a real pseudo-terminal.

Usage: tests/pty_smoke.py PATH_TO_MDV     (run by `make check`)
"""
import os, pty, re, select, struct, subprocess, sys, tempfile, time, fcntl, termios

MDV = sys.argv[1]
failures = []


def check(name, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + name + ("" if ok else f"  ({detail})"))
    if not ok:
        failures.append(name)


def run_pty(args, keys, cols=60, rows=12, timeout=5.0):
    """Run mdv in a pty, type `keys` one chunk at a time; return (output, exited)."""
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(MDV, [MDV] + args)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    out = b""

    def pump(seconds):
        nonlocal out
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.02)
            if r:
                try:
                    out += os.read(fd, 65536)
                except OSError:
                    return

    pump(0.3)
    for k in keys:
        os.write(fd, k)
        pump(0.15)
    deadline = time.time() + timeout
    exited = False
    while time.time() < deadline:
        if os.waitpid(pid, os.WNOHANG)[0] == pid:
            exited = True
            break
        pump(0.05)
    if not exited:
        os.kill(pid, 9)
        os.waitpid(pid, 0)
    return out, exited


def visible(b):
    return re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x1b\x07]*(\x1b\\|\x07)", b"", b).decode(errors="replace")


tmp = tempfile.mkdtemp()


def doc(name, text):
    path = os.path.join(tmp, name)
    with open(path, "w") as f:
        f.write(text)
    return path


long_code = doc("code.md", "```rust\nlet s = \"" + "x" * 150 + "\";\n```\n")
words = doc("words.md", "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima\n")
targets = doc("targets.md", "\n\n".join(f"para {i}" + (" TARGET" if i in (20, 40) else "") for i in range(60)))

out, exited = run_pty([long_code], [b"q"])
check("q exits", exited)
check("enters and leaves the alternate screen", b"\x1b[?1049h" in out and out.rstrip().endswith(b"\x1b[?1049l"))
check("cursor shown again on exit", b"\x1b[?25h" in out)
check("no blink in wrapped colored code", b"\x1b[5m" not in out)

out, exited = run_pty([words], [b"q"], cols=30)
rows = [r.strip() for r in visible(out).split("\r\n")]
check("wraps at word boundaries", "foxtrot golf hotel india" in rows and "juliet kilo lima" in rows, rows[:4])

out, exited = run_pty([targets], [b"/target", b"\r", b"n", b"q"])
check("search: Enter submits and q still quits", exited)
check("search: second match reached with n", "para 40 TARGET" in visible(out))

out, exited = run_pty([targets], [b"\x1b"])
check("bare Esc quits", exited)

wide_table = "| a | b |\n|---|---|\n| " + "word " * 30 + " | " + "x" * 90 + " |\n"
piped = subprocess.run([MDV, doc("h.md", "# hi\n\n[x](https://x.io)\n\n" + wide_table)], capture_output=True).stdout
check("piped output has no escape codes", b"\x1b" not in piped, piped[:60])
forced = subprocess.run([MDV, "-n", "--color", os.path.join(tmp, "h.md")], capture_output=True).stdout
check("--color forces colors when piped", b"\x1b[" in forced)

latin1 = os.path.join(tmp, "latin1.md")
open(latin1, "wb").write(b"caf\xe9\n")
r = subprocess.run([MDV, latin1], capture_output=True)
check("non-UTF-8 input is shown, not rejected", r.returncode == 0 and "caf\ufffd" in r.stdout.decode(), r.stderr)

print(f"\n{len(failures)} failed" if failures else "\nall pty checks passed")
sys.exit(1 if failures else 0)
