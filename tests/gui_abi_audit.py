#!/usr/bin/env python3
"""Check every C binding in src/gui.rs against GObject introspection data:
the argument count of each `gs!("symbol", fn(...))` must match the real
function (instance and GError** included). dlsym only proves a name exists;
a missing trailing argument (e.g. forward_search's `limit`) crashes at runtime.
usage: gui_abi_audit.py src/gui.rs   (exit 1 on any mismatch)"""
import re, sys, warnings
warnings.filterwarnings("ignore")
import gi
try:
    gi.require_version("GIRepository", "2.0")
except ValueError:
    gi.require_version("GIRepository", "3.0")
from gi.repository import GIRepository as GIR
repo = GIR.Repository.get_default() if hasattr(GIR.Repository, "get_default") else GIR.Repository()
for ns, v in [("Gtk","4.0"),("Adw","1"),("Gio","2.0"),("GLib","2.0"),("GObject","2.0"),("Gdk","4.0"),("Gsk","4.0"),("Pango","1.0"),("GdkPixbuf","2.0")]:
    repo.require(ns, v, 0)

table = {}
def add_fn(fi):
    try:
        sym = fi.get_symbol()
    except Exception:
        return
    is_method = fi.is_method() if hasattr(fi, "is_method") else bool(fi.get_flags() & GIR.FunctionInfoFlags.IS_METHOD)
    throws = fi.can_throw_gerror()
    n = fi.get_n_args() + (1 if is_method else 0) + (1 if throws else 0)
    table[sym] = n

def walk(info):
    if isinstance(info, GIR.FunctionInfo):
        add_fn(info); return
    if hasattr(info, "get_n_methods"):
        for i in range(info.get_n_methods()):
            add_fn(info.get_method(i))
    # constructors (g_..._new) are indexed too, so they get arg-count checked
    # instead of tripping the unknown -> exit 1 path; older GIRepository lacks them
    if hasattr(info, "get_n_constructors"):
        for i in range(info.get_n_constructors()):
            add_fn(info.get_constructor(i))

for ns in ["Gtk","Adw","Gio","GLib","GObject","Gdk","Gsk","Pango","GdkPixbuf"]:
    for i in range(repo.get_n_infos(ns)):
        walk(repo.get_info(ns, i))

src = open(sys.argv[1]).read()
def count_args(s):
    depth = 0; n = 0; cur = ""
    for ch in s:
        if ch in "(<": depth += 1
        elif ch in ")>": depth -= 1
        if ch == "," and depth == 0:
            if cur.strip(): n += 1
            cur = ""
        else:
            cur += ch
    if cur.strip(): n += 1
    return n

bad = 0; checked = 0; unknown = []
bindings = []
for m in re.finditer(r'g(?:s|opt)!\((?:\w+[ ,]*)?"(\w+)", unsafe extern "C" fn\(', src):
    bindings.append((m.group(1), m.end()))
# Output capabilities are intentionally loaded separately from the main GUI
# table. Their field types are inferred by tf(), so audit those as well.
for st in re.finditer(r'struct (\w+)\s*\{(.*?)\n\}', src, re.S):
    fields = {m.group(1): st.start(2) + m.end() for m in re.finditer(
        r'(\w+): unsafe extern "C" fn\(', st.group(2))}
    if not fields:
        continue
    impl = re.search(r'impl ' + re.escape(st.group(1)) + r'\s*\{(.*?)\n\}', src, re.S)
    if impl:
        for m in re.finditer(r'(\w+): tf\(\w+, "(\w+)"\)\?', impl.group(1)):
            if m.group(1) in fields:
                bindings.append((m.group(2), fields[m.group(1)]))
for name, start in bindings:
    i = start; depth = 1
    while depth:
        c = src[i]
        if c == "(": depth += 1
        elif c == ")": depth -= 1
        i += 1
    args = src[start:i-1]
    variadic = "..." in args
    n = count_args(args.replace("...", ""))
    if name not in table:
        unknown.append(name + (" (varargs)" if variadic else "")); continue
    checked += 1
    if table[name] != n:
        bad += 1
        print(f"MISMATCH {name}: rust {n} args, C {table[name]}")
print(f"checked {checked} bindings, mismatches {bad}")
# varargs / non-introspectable ones, verified by hand against the C headers
REVIEWED = {"gtk_text_buffer_create_tag (varargs)", "gtk_text_buffer_insert_with_tags_by_name (varargs)",
            "g_object_set (varargs)", "gtk_snapshot_free_to_node", "g_timeout_add",
            "g_signal_connect_data", "g_timeout_add_seconds",
            "gdk_pixbuf_get_pixels", "g_bytes_new_with_free_func"}
new = [u for u in unknown if u not in REVIEWED]
if new:
    print("NOT INTROSPECTABLE, review by hand and add to REVIEWED:", ", ".join(new))
sys.exit(1 if bad or new else 0)
