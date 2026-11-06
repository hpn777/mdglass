// Native GNOME back end: GTK4 + libadwaita, no WebKit, no build-time C
// dependency. The system's GNOME libraries are loaded at startup with dlopen
// (only runtime `.so` files are required, never the -dev packages), so the
// binary stays a single small file. Documents render into a styled GtkTextView
// with buffer tags: an AdwToolbarView + header bar, an open dialog, in-buffer
// find, zoom, next/prev file, and live reload when the file changes.

use std::sync::OnceLock;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::path::Path;
use std::time::SystemTime;

use crate::ansi::char_width;
use crate::doc::{parse, spans_plain, spans_slice, spans_width, table_pieces, Cfg, Line, Role, Span, Style, TablePiece};

// ------------------------------------------------------------------ FFI ---

extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_LAZY: c_int = 1;
const RTLD_GLOBAL: c_int = 256;

type Ptr = *mut c_void;

/// Opaque stand-in for GtkTextIter. The real struct is smaller; we
/// over-allocate and force 16-byte alignment so any SIMD copies GTK makes
/// internally stay aligned.
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct TextIter {
    dummy: [u64; 16],
}

// enum values from the installed GTK
const ORIENT_VERTICAL: c_int = 1;
const WRAP_NONE: c_int = 0; // GtkWrapMode NONE: the view never soft-wraps (Phase 2)
#[allow(dead_code)] // documents the enum; only AUTOMATIC is in use since Phase 2
const POLICY_NEVER: c_int = 2; // kept: documents the other policy values
const POLICY_AUTOMATIC: c_int = 1;
/// PangoWrapMode WORD_CHAR. NOT the same number as GtkWrapMode's (3): Pango
/// 1.56 renumbered the enum to WORD=0, CHAR=1, WORD_CHAR=2, NONE=3 (verified
/// via introspection on the installed pango 1.57). Passing 3 here means
/// WRAP_NONE on modern pango and silently disables pixel wrapping.
const PANGO_WRAP_WORD_CHAR: c_int = 2;
const PHASE_CAPTURED: c_int = 1;
const NON_UNIQUE: c_uint = 16; // G_APPLICATION_NON_UNIQUE
const PROVIDER_PRIORITY: c_uint = 600; // GTK_STYLE_PROVIDER_PRIORITY_APPLICATION
const WEIGHT_BOLD: c_int = 700;
const STYLE_ITALIC: c_int = 2;
const UNDERLINE_SINGLE: c_int = 1;
const SEARCH_FLAGS: c_uint = 1 | 2; // TEXT_ONLY | CASE_INSENSITIVE
const KEY_ESCAPE: c_uint = 0xff1b;
const BASE_FONT_PX: f64 = 15.0;
/// Heading font scales by level (index 0 unused). make_tags sets the tag
/// scale, wrap_spans_px measures the pre-wrap with the same size.
const HEAD_SCALES: [f64; 7] = [1.0, 1.5, 1.3, 1.15, 1.05, 1.0, 1.0];
const MONO_ADVANCE: f64 = 0.6; // fallback em advance, only before the first measure_mono()
/// Family pinned wherever columns must line up: DejaVu (0.6 em advance,
/// box drawing), Noto as the CJK fallback, generic monospace as the last resort.
const MONO_FAMILY: &str = "DejaVu Sans Mono, Noto Sans Mono, Monospace";

fn expect(handle: Ptr, what: &str) -> Result<Ptr, String> {
    if handle.is_null() {
        Err(format!("cannot load {what}"))
    } else {
        Ok(handle)
    }
}

unsafe fn tf<T>(handle: Ptr, name: &str) -> Result<T, String> {
    let cname = CString::new(name).unwrap();
    let p = dlsym(handle, cname.as_ptr());
    if p.is_null() {
        Err(format!("missing symbol {name}"))
    } else {
        Ok(std::mem::transmute_copy::<*mut c_void, T>(&p))
    }
}

/// All C entry points we use. `g_signal_connect_data` takes plain handlers,
/// which are then called as `handler(instance, signal-params..., user_data)`.
struct Lib {
    gtk_init_check: unsafe extern "C" fn() -> c_int,
    gtk_window_set_title: unsafe extern "C" fn(Ptr, *const c_char),
    gtk_window_set_default_size: unsafe extern "C" fn(Ptr, c_int, c_int),
    gtk_window_get_default_size: unsafe extern "C" fn(Ptr, *mut c_int, *mut c_int),
    gtk_window_close: unsafe extern "C" fn(Ptr),
    adw_application_window_set_content: unsafe extern "C" fn(Ptr, Ptr),
    gtk_window_present: unsafe extern "C" fn(Ptr),
    adw_header_bar_set_title_widget: unsafe extern "C" fn(Ptr, Ptr),
    adw_header_bar_pack_end: unsafe extern "C" fn(Ptr, Ptr),
    gtk_scrolled_window_new: unsafe extern "C" fn() -> Ptr,
    gtk_scrolled_window_set_child: unsafe extern "C" fn(Ptr, Ptr),
    gtk_scrolled_window_set_policy: unsafe extern "C" fn(Ptr, c_int, c_int),
    gtk_text_view_new: unsafe extern "C" fn() -> Ptr,
    gtk_text_view_get_buffer: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_text_view_set_wrap_mode: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_set_editable: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_set_cursor_visible: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_set_left_margin: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_set_right_margin: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_set_top_margin: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_set_bottom_margin: unsafe extern "C" fn(Ptr, c_int),
    gtk_text_view_scroll_to_iter: unsafe extern "C" fn(Ptr, *mut TextIter, f64, c_int, f64, f64) -> c_int,
    gtk_text_buffer_create_tag: unsafe extern "C" fn(Ptr, *const c_char, *const c_char, ...) -> Ptr,
    gtk_text_buffer_set_text: unsafe extern "C" fn(Ptr, *const c_char, c_int),
    gtk_text_buffer_get_end_iter: unsafe extern "C" fn(Ptr, *mut TextIter),
    gtk_text_buffer_get_start_iter: unsafe extern "C" fn(Ptr, *mut TextIter),
    gtk_text_buffer_get_insert: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_text_buffer_get_iter_at_mark: unsafe extern "C" fn(Ptr, *mut TextIter, Ptr),
    gtk_text_buffer_get_iter_at_offset: unsafe extern "C" fn(Ptr, *mut TextIter, c_int),
    gtk_text_buffer_get_iter_at_line: unsafe extern "C" fn(Ptr, *mut TextIter, c_int),
    gtk_text_view_get_visible_rect: unsafe extern "C" fn(Ptr, *mut c_int),
    gtk_text_view_get_line_at_y: unsafe extern "C" fn(Ptr, *mut TextIter, c_int, *mut c_int) -> Ptr,
    gtk_text_buffer_insert: unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int),
    gtk_text_buffer_insert_with_tags_by_name:
        unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int, *const c_char, ...),
    gtk_text_buffer_place_cursor: unsafe extern "C" fn(Ptr, *const TextIter),
    gtk_text_buffer_apply_tag_by_name:
        unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter),
    gtk_text_buffer_remove_tag_by_name:
        unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter),
    gtk_text_buffer_get_bounds: unsafe extern "C" fn(Ptr, *mut TextIter, *mut TextIter),
    gtk_text_iter_forward_search:
        unsafe extern "C" fn(*const TextIter, *const c_char, c_uint, *mut TextIter, *mut TextIter, *const TextIter) -> c_int,
    gtk_text_iter_compare: unsafe extern "C" fn(*const TextIter, *const TextIter) -> c_int,
    gtk_text_iter_get_offset: unsafe extern "C" fn(*const TextIter) -> c_int,
    gtk_text_iter_get_line: unsafe extern "C" fn(*const TextIter) -> c_int,
    gtk_box_new: unsafe extern "C" fn(c_int, c_int) -> Ptr,
    gtk_box_append: unsafe extern "C" fn(Ptr, Ptr),
    gtk_button_new_from_icon_name: unsafe extern "C" fn(*const c_char) -> Ptr,
    gtk_button_new_with_label: unsafe extern "C" fn(*const c_char) -> Ptr,
    gtk_menu_button_new: unsafe extern "C" fn() -> Ptr,
    gtk_label_new: unsafe extern "C" fn(*const c_char) -> Ptr,
    gtk_label_set_text: unsafe extern "C" fn(Ptr, *const c_char),
    gtk_label_get_text: unsafe extern "C" fn(Ptr) -> *const c_char,
    gtk_widget_add_css_class: unsafe extern "C" fn(Ptr, *const c_char),
    gtk_widget_set_visible: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_set_size_request: unsafe extern "C" fn(Ptr, c_int, c_int),
    gtk_widget_set_margin_start: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_set_margin_end: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_set_margin_top: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_set_margin_bottom: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_set_hexpand: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_set_vexpand: unsafe extern "C" fn(Ptr, c_int),
    gtk_widget_grab_focus: unsafe extern "C" fn(Ptr),
    gtk_widget_add_controller: unsafe extern "C" fn(Ptr, Ptr) -> c_uint,
    gtk_widget_get_display: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_widget_set_tooltip_text: unsafe extern "C" fn(Ptr, *const c_char),
    gtk_widget_create_pango_layout: unsafe extern "C" fn(Ptr, *const c_char) -> Ptr,
    gtk_event_controller_key_new: unsafe extern "C" fn() -> Ptr,
    gtk_event_controller_set_propagation_phase: unsafe extern "C" fn(Ptr, c_int),
    gtk_css_provider_new: unsafe extern "C" fn() -> Ptr,
    gtk_css_provider_load_from_string: unsafe extern "C" fn(Ptr, *const c_char),
    gtk_style_context_add_provider_for_display: unsafe extern "C" fn(Ptr, Ptr, c_uint),
    gtk_entry_new: unsafe extern "C" fn() -> Ptr,
    gtk_editable_get_text: unsafe extern "C" fn(Ptr) -> *const c_char,
    gtk_file_dialog_new: unsafe extern "C" fn() -> Ptr,
    gtk_file_dialog_open:
        unsafe extern "C" fn(Ptr, Ptr, Ptr, Option<unsafe extern "C" fn(Ptr, Ptr, Ptr)>, Ptr),
    gtk_file_dialog_open_finish: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr) -> Ptr,
    gtk_application_set_accels_for_action: unsafe extern "C" fn(Ptr, *const c_char, *const *const c_char),
    adw_application_new: unsafe extern "C" fn(*const c_char, c_uint) -> Ptr,
    adw_application_window_new: unsafe extern "C" fn(Ptr) -> Ptr,
    adw_header_bar_new: unsafe extern "C" fn() -> Ptr,
    adw_toolbar_view_new: unsafe extern "C" fn() -> Ptr,
    adw_toolbar_view_add_top_bar: unsafe extern "C" fn(Ptr, Ptr),
    adw_toolbar_view_set_content: unsafe extern "C" fn(Ptr, Ptr),
    adw_window_title_new: unsafe extern "C" fn(*const c_char, *const c_char) -> Ptr,
    adw_status_page_new: unsafe extern "C" fn() -> Ptr,
    adw_status_page_set_title: unsafe extern "C" fn(Ptr, *const c_char),
    adw_status_page_set_description: unsafe extern "C" fn(Ptr, *const c_char),
    adw_status_page_set_child: unsafe extern "C" fn(Ptr, Ptr),
    adw_style_manager_get_default: unsafe extern "C" fn() -> Ptr,
    adw_style_manager_get_dark: unsafe extern "C" fn(Ptr) -> c_int,
    pango_layout_set_text: unsafe extern "C" fn(Ptr, *const c_char, c_int),
    pango_layout_get_size: unsafe extern "C" fn(Ptr, *mut c_int, *mut c_int),
    pango_layout_set_width: unsafe extern "C" fn(Ptr, c_int),
    pango_layout_set_wrap: unsafe extern "C" fn(Ptr, c_int),
    pango_layout_get_line_count: unsafe extern "C" fn(Ptr) -> c_int,
    pango_layout_get_line_readonly: unsafe extern "C" fn(Ptr, c_int) -> Ptr,
    pango_layout_index_to_line_x: unsafe extern "C" fn(Ptr, c_int, c_int, *mut c_int, *mut c_int),
    pango_layout_get_context: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_layout_get_font_description: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_context_get_font_description: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_font_description_copy: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_layout_set_font_description: unsafe extern "C" fn(Ptr, Ptr),
    pango_font_description_from_string: unsafe extern "C" fn(*const c_char) -> Ptr,
    pango_font_description_set_absolute_size: unsafe extern "C" fn(Ptr, f64),
    pango_font_description_free: unsafe extern "C" fn(Ptr),
    g_object_set: unsafe extern "C" fn(Ptr, *const c_char, ...),
    g_object_unref: unsafe extern "C" fn(Ptr),
    g_object_ref_sink: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_widget_paintable_new: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_snapshot_new: unsafe extern "C" fn() -> Ptr,
    gdk_paintable_snapshot: unsafe extern "C" fn(Ptr, Ptr, f64, f64),
    gtk_snapshot_free_to_node: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_native_get_renderer: unsafe extern "C" fn(Ptr) -> Ptr,
    gsk_renderer_render_texture: unsafe extern "C" fn(Ptr, Ptr, Ptr) -> Ptr,
    gdk_texture_save_to_png: unsafe extern "C" fn(Ptr, *const c_char) -> c_int,
    gtk_widget_get_width: unsafe extern "C" fn(Ptr) -> c_int,
    gtk_widget_get_height: unsafe extern "C" fn(Ptr) -> c_int,
    gtk_editable_set_text: unsafe extern "C" fn(Ptr, *const c_char),
    g_action_group_activate_action: unsafe extern "C" fn(Ptr, *const c_char, Ptr),
    g_timeout_add: unsafe extern "C" fn(c_uint, Option<unsafe extern "C" fn(Ptr) -> c_int>, Ptr) -> c_uint,
    g_signal_connect_data: unsafe extern "C" fn(Ptr, *const c_char, Ptr, Ptr, Ptr, c_ulong) -> c_ulong,
    g_menu_new: unsafe extern "C" fn() -> Ptr,
    g_menu_append: unsafe extern "C" fn(Ptr, *const c_char, *const c_char),
    g_menu_append_submenu: unsafe extern "C" fn(Ptr, *const c_char, Ptr) -> c_uint,
    g_simple_action_new: unsafe extern "C" fn(*const c_char, Ptr) -> Ptr,
    g_action_map_add_action: unsafe extern "C" fn(Ptr, Ptr),
    g_application_run: unsafe extern "C" fn(Ptr, c_int, *mut *const c_char) -> c_int,
    g_file_peek_path: unsafe extern "C" fn(Ptr) -> *const c_char,
    g_timeout_add_seconds:
        unsafe extern "C" fn(c_uint, Option<unsafe extern "C" fn(Ptr) -> c_int>, Ptr) -> c_uint,
}

#[rustfmt::skip]
fn load_lib() -> Result<Lib, String> {
    unsafe {
        let gtk = expect(dlopen(c"libgtk-4.so.1".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libgtk-4 (GNOME 42 or newer)")?;
        let adw = expect(dlopen(c"libadwaita-1.so.0".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libadwaita-1 (GNOME 42 or newer)")?;
        let gobject = expect(dlopen(c"libgobject-2.0.so.0".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libgobject-2.0")?;
        let gio = expect(dlopen(c"libgio-2.0.so.0".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libgio-2.0")?;
        let glib = expect(dlopen(c"libglib-2.0.so.0".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libglib-2.0")?;
        let pango = expect(dlopen(c"libpango-1.0.so.0".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libpango-1.0")?;
        let mut missing: Vec<String> = Vec::new();
        macro_rules! g {
            ($h:expr, $n:literal, $t:ty) => {
                match tf::<$t>($h, $n) {
                    Ok(f) => f,
                    Err(e) => { missing.push(e); std::mem::transmute::<*mut c_void, $t>(std::ptr::null_mut()) }
                }
            };
        }
        macro_rules! gs {
            ($n:literal, $t:ty) => { g!(gtk, $n, $t) };
            (adw $n:literal, $t:ty) => { g!(adw, $n, $t) };
            (go $n:literal, $t:ty) => { g!(gobject, $n, $t) };
            (gio $n:literal, $t:ty) => { g!(gio, $n, $t) };
            (glib $n:literal, $t:ty) => { g!(glib, $n, $t) };
            (pango $n:literal, $t:ty) => { g!(pango, $n, $t) };
        }
        let lib = Lib {
            gtk_init_check: gs!("gtk_init_check", unsafe extern "C" fn() -> c_int),
            gtk_window_set_title: gs!("gtk_window_set_title", unsafe extern "C" fn(Ptr, *const c_char)),
            gtk_window_set_default_size: gs!("gtk_window_set_default_size", unsafe extern "C" fn(Ptr, c_int, c_int)),
            gtk_window_get_default_size: gs!("gtk_window_get_default_size", unsafe extern "C" fn(Ptr, *mut c_int, *mut c_int)),
            gtk_window_close: gs!("gtk_window_close", unsafe extern "C" fn(Ptr)),
            adw_application_window_set_content: gs!(adw "adw_application_window_set_content", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_window_present: gs!("gtk_window_present", unsafe extern "C" fn(Ptr)),
            adw_header_bar_set_title_widget: gs!(adw "adw_header_bar_set_title_widget", unsafe extern "C" fn(Ptr, Ptr)),
            adw_header_bar_pack_end: gs!(adw "adw_header_bar_pack_end", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_scrolled_window_new: gs!("gtk_scrolled_window_new", unsafe extern "C" fn() -> Ptr),
            gtk_scrolled_window_set_child: gs!("gtk_scrolled_window_set_child", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_scrolled_window_set_policy: gs!("gtk_scrolled_window_set_policy", unsafe extern "C" fn(Ptr, c_int, c_int)),
            gtk_text_view_new: gs!("gtk_text_view_new", unsafe extern "C" fn() -> Ptr),
            gtk_text_view_get_buffer: gs!("gtk_text_view_get_buffer", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_text_view_set_wrap_mode: gs!("gtk_text_view_set_wrap_mode", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_set_editable: gs!("gtk_text_view_set_editable", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_set_cursor_visible: gs!("gtk_text_view_set_cursor_visible", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_set_left_margin: gs!("gtk_text_view_set_left_margin", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_set_right_margin: gs!("gtk_text_view_set_right_margin", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_set_top_margin: gs!("gtk_text_view_set_top_margin", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_set_bottom_margin: gs!("gtk_text_view_set_bottom_margin", unsafe extern "C" fn(Ptr, c_int)),
            gtk_text_view_scroll_to_iter: gs!("gtk_text_view_scroll_to_iter", unsafe extern "C" fn(Ptr, *mut TextIter, f64, c_int, f64, f64) -> c_int),
            gtk_text_buffer_create_tag: gs!("gtk_text_buffer_create_tag", unsafe extern "C" fn(Ptr, *const c_char, *const c_char, ...) -> Ptr),
            gtk_text_buffer_set_text: gs!("gtk_text_buffer_set_text", unsafe extern "C" fn(Ptr, *const c_char, c_int)),
            gtk_text_buffer_get_end_iter: gs!("gtk_text_buffer_get_end_iter", unsafe extern "C" fn(Ptr, *mut TextIter)),
            gtk_text_buffer_get_start_iter: gs!("gtk_text_buffer_get_start_iter", unsafe extern "C" fn(Ptr, *mut TextIter)),
            gtk_text_buffer_get_insert: gs!("gtk_text_buffer_get_insert", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_text_buffer_get_iter_at_mark: gs!("gtk_text_buffer_get_iter_at_mark", unsafe extern "C" fn(Ptr, *mut TextIter, Ptr)),
            gtk_text_buffer_get_iter_at_offset: gs!("gtk_text_buffer_get_iter_at_offset", unsafe extern "C" fn(Ptr, *mut TextIter, c_int)),
            gtk_text_buffer_get_iter_at_line: gs!("gtk_text_buffer_get_iter_at_line", unsafe extern "C" fn(Ptr, *mut TextIter, c_int)),
            gtk_text_view_get_visible_rect: gs!("gtk_text_view_get_visible_rect", unsafe extern "C" fn(Ptr, *mut c_int)),
            gtk_text_view_get_line_at_y: gs!("gtk_text_view_get_line_at_y", unsafe extern "C" fn(Ptr, *mut TextIter, c_int, *mut c_int) -> Ptr),
            gtk_text_buffer_insert: gs!("gtk_text_buffer_insert", unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int)),
            gtk_text_buffer_insert_with_tags_by_name: gs!("gtk_text_buffer_insert_with_tags_by_name", unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int, *const c_char, ...)),
            gtk_text_buffer_place_cursor: gs!("gtk_text_buffer_place_cursor", unsafe extern "C" fn(Ptr, *const TextIter)),
            gtk_text_buffer_apply_tag_by_name: gs!("gtk_text_buffer_apply_tag_by_name", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter)),
            gtk_text_buffer_remove_tag_by_name: gs!("gtk_text_buffer_remove_tag_by_name", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter)),
            gtk_text_buffer_get_bounds: gs!("gtk_text_buffer_get_bounds", unsafe extern "C" fn(Ptr, *mut TextIter, *mut TextIter)),
            gtk_text_iter_forward_search: gs!("gtk_text_iter_forward_search", unsafe extern "C" fn(*const TextIter, *const c_char, c_uint, *mut TextIter, *mut TextIter, *const TextIter) -> c_int),
            gtk_text_iter_compare: gs!("gtk_text_iter_compare", unsafe extern "C" fn(*const TextIter, *const TextIter) -> c_int),
            gtk_text_iter_get_offset: gs!("gtk_text_iter_get_offset", unsafe extern "C" fn(*const TextIter) -> c_int),
            gtk_text_iter_get_line: gs!("gtk_text_iter_get_line", unsafe extern "C" fn(*const TextIter) -> c_int),
            gtk_box_new: gs!("gtk_box_new", unsafe extern "C" fn(c_int, c_int) -> Ptr),
            gtk_box_append: gs!("gtk_box_append", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_button_new_from_icon_name: gs!("gtk_button_new_from_icon_name", unsafe extern "C" fn(*const c_char) -> Ptr),
            gtk_button_new_with_label: gs!("gtk_button_new_with_label", unsafe extern "C" fn(*const c_char) -> Ptr),
            gtk_menu_button_new: gs!("gtk_menu_button_new", unsafe extern "C" fn() -> Ptr),
            gtk_label_new: gs!("gtk_label_new", unsafe extern "C" fn(*const c_char) -> Ptr),
            gtk_label_set_text: gs!("gtk_label_set_text", unsafe extern "C" fn(Ptr, *const c_char)),
            gtk_label_get_text: gs!("gtk_label_get_text", unsafe extern "C" fn(Ptr) -> *const c_char),
            gtk_widget_add_css_class: gs!("gtk_widget_add_css_class", unsafe extern "C" fn(Ptr, *const c_char)),
            gtk_widget_set_visible: gs!("gtk_widget_set_visible", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_set_size_request: gs!("gtk_widget_set_size_request", unsafe extern "C" fn(Ptr, c_int, c_int)),
            gtk_widget_set_margin_start: gs!("gtk_widget_set_margin_start", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_set_margin_end: gs!("gtk_widget_set_margin_end", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_set_margin_top: gs!("gtk_widget_set_margin_top", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_set_margin_bottom: gs!("gtk_widget_set_margin_bottom", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_set_hexpand: gs!("gtk_widget_set_hexpand", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_set_vexpand: gs!("gtk_widget_set_vexpand", unsafe extern "C" fn(Ptr, c_int)),
            gtk_widget_grab_focus: gs!("gtk_widget_grab_focus", unsafe extern "C" fn(Ptr)),
            gtk_widget_add_controller: gs!("gtk_widget_add_controller", unsafe extern "C" fn(Ptr, Ptr) -> c_uint),
            gtk_widget_get_display: gs!("gtk_widget_get_display", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_widget_set_tooltip_text: gs!("gtk_widget_set_tooltip_text", unsafe extern "C" fn(Ptr, *const c_char)),
            gtk_widget_create_pango_layout: gs!("gtk_widget_create_pango_layout", unsafe extern "C" fn(Ptr, *const c_char) -> Ptr),
            gtk_event_controller_key_new: gs!("gtk_event_controller_key_new", unsafe extern "C" fn() -> Ptr),
            gtk_event_controller_set_propagation_phase: gs!("gtk_event_controller_set_propagation_phase", unsafe extern "C" fn(Ptr, c_int)),
            gtk_css_provider_new: gs!("gtk_css_provider_new", unsafe extern "C" fn() -> Ptr),
            gtk_css_provider_load_from_string: gs!("gtk_css_provider_load_from_string", unsafe extern "C" fn(Ptr, *const c_char)),
            gtk_style_context_add_provider_for_display: gs!("gtk_style_context_add_provider_for_display", unsafe extern "C" fn(Ptr, Ptr, c_uint)),
            gtk_entry_new: gs!("gtk_entry_new", unsafe extern "C" fn() -> Ptr),
            gtk_editable_get_text: gs!("gtk_editable_get_text", unsafe extern "C" fn(Ptr) -> *const c_char),
            gtk_file_dialog_new: gs!("gtk_file_dialog_new", unsafe extern "C" fn() -> Ptr),
            gtk_file_dialog_open: gs!("gtk_file_dialog_open", unsafe extern "C" fn(Ptr, Ptr, Ptr, Option<unsafe extern "C" fn(Ptr, Ptr, Ptr)>, Ptr)),
            gtk_file_dialog_open_finish: gs!("gtk_file_dialog_open_finish", unsafe extern "C" fn(Ptr, Ptr, *mut Ptr) -> Ptr),
            gtk_application_set_accels_for_action: gs!("gtk_application_set_accels_for_action", unsafe extern "C" fn(Ptr, *const c_char, *const *const c_char)),
            adw_application_new: gs!(adw "adw_application_new", unsafe extern "C" fn(*const c_char, c_uint) -> Ptr),
            adw_application_window_new: gs!(adw "adw_application_window_new", unsafe extern "C" fn(Ptr) -> Ptr),
            adw_header_bar_new: gs!(adw "adw_header_bar_new", unsafe extern "C" fn() -> Ptr),
            adw_toolbar_view_new: gs!(adw "adw_toolbar_view_new", unsafe extern "C" fn() -> Ptr),
            adw_toolbar_view_add_top_bar: gs!(adw "adw_toolbar_view_add_top_bar", unsafe extern "C" fn(Ptr, Ptr)),
            adw_toolbar_view_set_content: gs!(adw "adw_toolbar_view_set_content", unsafe extern "C" fn(Ptr, Ptr)),
            adw_window_title_new: gs!(adw "adw_window_title_new", unsafe extern "C" fn(*const c_char, *const c_char) -> Ptr),
            adw_status_page_new: gs!(adw "adw_status_page_new", unsafe extern "C" fn() -> Ptr),
            adw_status_page_set_title: gs!(adw "adw_status_page_set_title", unsafe extern "C" fn(Ptr, *const c_char)),
            adw_status_page_set_description: gs!(adw "adw_status_page_set_description", unsafe extern "C" fn(Ptr, *const c_char)),
            adw_status_page_set_child: gs!(adw "adw_status_page_set_child", unsafe extern "C" fn(Ptr, Ptr)),
            adw_style_manager_get_default: gs!(adw "adw_style_manager_get_default", unsafe extern "C" fn() -> Ptr),
            adw_style_manager_get_dark: gs!(adw "adw_style_manager_get_dark", unsafe extern "C" fn(Ptr) -> c_int),
            pango_layout_set_text: gs!(pango "pango_layout_set_text", unsafe extern "C" fn(Ptr, *const c_char, c_int)),
            pango_layout_get_size: gs!(pango "pango_layout_get_size", unsafe extern "C" fn(Ptr, *mut c_int, *mut c_int)),
            pango_layout_set_width: gs!(pango "pango_layout_set_width", unsafe extern "C" fn(Ptr, c_int)),
            pango_layout_set_wrap: gs!(pango "pango_layout_set_wrap", unsafe extern "C" fn(Ptr, c_int)),
            pango_layout_get_line_count: gs!(pango "pango_layout_get_line_count", unsafe extern "C" fn(Ptr) -> c_int),
            pango_layout_get_line_readonly: gs!(pango "pango_layout_get_line_readonly", unsafe extern "C" fn(Ptr, c_int) -> Ptr),
            pango_layout_index_to_line_x: gs!(pango "pango_layout_index_to_line_x", unsafe extern "C" fn(Ptr, c_int, c_int, *mut c_int, *mut c_int)),
            pango_layout_get_context: gs!(pango "pango_layout_get_context", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_layout_get_font_description: gs!(pango "pango_layout_get_font_description", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_context_get_font_description: gs!(pango "pango_context_get_font_description", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_font_description_copy: gs!(pango "pango_font_description_copy", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_layout_set_font_description: gs!(pango "pango_layout_set_font_description", unsafe extern "C" fn(Ptr, Ptr)),
            pango_font_description_from_string: gs!(pango "pango_font_description_from_string", unsafe extern "C" fn(*const c_char) -> Ptr),
            pango_font_description_set_absolute_size: gs!(pango "pango_font_description_set_absolute_size", unsafe extern "C" fn(Ptr, f64)),
            pango_font_description_free: gs!(pango "pango_font_description_free", unsafe extern "C" fn(Ptr)),
            g_object_set: gs!(go "g_object_set", unsafe extern "C" fn(Ptr, *const c_char, ...)),
            g_object_unref: gs!(go "g_object_unref", unsafe extern "C" fn(Ptr)),
            g_object_ref_sink: gs!(go "g_object_ref_sink", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_widget_paintable_new: gs!("gtk_widget_paintable_new", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_snapshot_new: gs!("gtk_snapshot_new", unsafe extern "C" fn() -> Ptr),
            gdk_paintable_snapshot: gs!("gdk_paintable_snapshot", unsafe extern "C" fn(Ptr, Ptr, f64, f64)),
            gtk_snapshot_free_to_node: gs!("gtk_snapshot_free_to_node", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_native_get_renderer: gs!("gtk_native_get_renderer", unsafe extern "C" fn(Ptr) -> Ptr),
            gsk_renderer_render_texture: gs!("gsk_renderer_render_texture", unsafe extern "C" fn(Ptr, Ptr, Ptr) -> Ptr),
            gdk_texture_save_to_png: gs!("gdk_texture_save_to_png", unsafe extern "C" fn(Ptr, *const c_char) -> c_int),
            gtk_widget_get_width: gs!("gtk_widget_get_width", unsafe extern "C" fn(Ptr) -> c_int),
            gtk_widget_get_height: gs!("gtk_widget_get_height", unsafe extern "C" fn(Ptr) -> c_int),
            gtk_editable_set_text: gs!("gtk_editable_set_text", unsafe extern "C" fn(Ptr, *const c_char)),
            g_action_group_activate_action: gs!(gio "g_action_group_activate_action", unsafe extern "C" fn(Ptr, *const c_char, Ptr)),
            g_timeout_add: gs!(glib "g_timeout_add", unsafe extern "C" fn(c_uint, Option<unsafe extern "C" fn(Ptr) -> c_int>, Ptr) -> c_uint),
            g_signal_connect_data: gs!(go "g_signal_connect_data", unsafe extern "C" fn(Ptr, *const c_char, Ptr, Ptr, Ptr, c_ulong) -> c_ulong),
            g_menu_new: gs!(gio "g_menu_new", unsafe extern "C" fn() -> Ptr),
            g_menu_append: gs!(gio "g_menu_append", unsafe extern "C" fn(Ptr, *const c_char, *const c_char)),
            g_menu_append_submenu: gs!(gio "g_menu_append_submenu", unsafe extern "C" fn(Ptr, *const c_char, Ptr) -> c_uint),
            g_simple_action_new: gs!(gio "g_simple_action_new", unsafe extern "C" fn(*const c_char, Ptr) -> Ptr),
            g_action_map_add_action: gs!(gio "g_action_map_add_action", unsafe extern "C" fn(Ptr, Ptr)),
            g_application_run: gs!(gio "g_application_run", unsafe extern "C" fn(Ptr, c_int, *mut *const c_char) -> c_int),
            g_file_peek_path: gs!(gio "g_file_peek_path", unsafe extern "C" fn(Ptr) -> *const c_char),
            g_timeout_add_seconds: gs!(glib "g_timeout_add_seconds", unsafe extern "C" fn(c_uint, Option<unsafe extern "C" fn(Ptr) -> c_int>, Ptr) -> c_uint),
        };
        if !missing.is_empty() {
            return Err(format!("system libraries are missing: {}", missing.join(", ")));
        }
        Ok(lib)
    }
}

static LIB: OnceLock<Lib> = OnceLock::new();

#[inline]
fn l() -> &'static Lib {
    LIB.get().unwrap()
}

// ----------------------------------------------------------------- app ---

struct App {
    app: Ptr,
    win: Ptr,
    view: Ptr,
    buffer: Ptr,
    wtitle: Ptr,
    status: Ptr,
    scrolled: Ptr,
    dialog: Ptr,
    findbar: Ptr,
    entry: Ptr,
    hits: Ptr,
    css: Ptr,
    zoom: f64,
    /// real pixel width of one monospace column at the current zoom, 0 until measured
    mono_px: f64,
    files: Vec<String>,
    idx: usize,
    content: Option<String>, // set when the document came from stdin
    mtime: Option<SystemTime>,
    /// last rendered source and the column budget it was rendered with, so
    /// resizes and zoom changes re-lay out without re-reading the file
    src_cache: Option<String>,
    budget: usize,
    /// margin tags by name: (tag, quote depth, indent cols, hanging cols, is code block)
    ptags: HashMap<String, (Ptr, usize, usize, usize, bool)>,
    /// colored tags with their light and dark foregrounds (apply_theme_colors)
    ctags: Vec<(Ptr, &'static str, &'static str)>,
    /// per-run letter-spacing tags by spacing value in Pango units (fit_tag)
    fit_tags: HashMap<i64, Ptr>,
    status_shown: bool,
    find_shown: bool,
}

impl App {
    fn findbar_visible(&self) -> bool {
        self.find_shown
    }
}

static mut APP: *mut App = std::ptr::null_mut();

#[inline]
unsafe fn app() -> *mut App {
    APP
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("").unwrap())
}

unsafe fn set_s(o: Ptr, prop: &str, v: &str) {
    let (p, v) = (cstr(prop), cstr(v));
    (l().g_object_set)(o, p.as_ptr(), v.as_ptr(), std::ptr::null::<c_char>());
}

unsafe fn set_i(o: Ptr, prop: &str, v: c_int) {
    let p = cstr(prop);
    (l().g_object_set)(o, p.as_ptr(), v, std::ptr::null::<c_char>());
}

unsafe fn set_b(o: Ptr, prop: &str, v: bool) {
    let p = cstr(prop);
    (l().g_object_set)(o, p.as_ptr(), c_int::from(v), std::ptr::null::<c_char>());
}

unsafe fn set_f(o: Ptr, prop: &str, v: f64) {
    let p = cstr(prop);
    (l().g_object_set)(o, p.as_ptr(), v, std::ptr::null::<c_char>());
}

unsafe fn connect(o: Ptr, signal: &str, cb: Ptr) {
    let s = cstr(signal);
    (l().g_signal_connect_data)(
        o,
        s.as_ptr(),
        cb,
        APP as Ptr,
        std::ptr::null_mut(),
        0,
    );
}

/// Reinterpret a concrete `extern "C"` handler as a GObject callback pointer.
fn cbptr<T>(f: T) -> Ptr {
    unsafe { std::mem::transmute_copy(&f) }
}

// ------------------------------------------------------------ text tags ---

unsafe fn tag(name: &str) -> Ptr {
    let n = cstr(name);
    (l().gtk_text_buffer_create_tag)((*app()).buffer, n.as_ptr(), std::ptr::null())
}

/// Tag name `fit_tag` registers for the letter-spacing value `sp` (the same
/// string is needed by callers inserting text with tags by name).
fn fit_name(sp: i64) -> String {
    format!("fit{sp}")
}

/// Cached tag applying absolute letter-spacing of `sp` Pango units (1/1024 px)
/// to every char it covers: each glyph is pushed so the whole run occupies
/// exactly its nominal width, and everything after it lands back on the
/// column grid. Spacing is absolute px, so one tag per value stays correct
/// across zoom — a relayout recomputes `sp` and fetches or creates that tag.
unsafe fn fit_tag(sp: i64) -> Ptr {
    let a = &mut *app();
    if let Some(&t) = a.fit_tags.get(&sp) {
        return t;
    }
    let t = tag(&fit_name(sp));
    if !t.is_null() {
        set_i(t, "letter-spacing", sp as c_int);
        set_i(t, "letter-spacing-set", 1);
        a.fit_tags.insert(sp, t);
    }
    t
}

unsafe fn make_tags() {
    let base = tag("base"); // sentinel so varargs always have one name
    let _ = base;
    let above: [c_int; 7] = [0, 14, 12, 10, 8, 8, 8];
    for lvl in 1..=6 {
        let t = tag(&format!("h{lvl}"));
        set_i(t, "weight", WEIGHT_BOLD);
        set_f(t, "scale", HEAD_SCALES[lvl]);
        set_i(t, "pixels-above-lines", above[lvl]);
        set_i(t, "pixels-below-lines", 2);
    }
    set_i(tag("bold"), "weight", WEIGHT_BOLD);
    set_i(tag("italic"), "style", STYLE_ITALIC);
    set_i(tag("strike"), "strikethrough", 1);
    set_i(tag("under"), "underline", UNDERLINE_SINGLE);

    let t = tag("role-code");
    set_s(t, "family", MONO_FAMILY);
    set_s(t, "background", "rgba(127,127,127,0.18)");
    set_i(t, "background-set", 1);

    let t = tag("role-link");
    set_i(t, "foreground-set", 1);
    set_i(t, "underline", UNDERLINE_SINGLE);
    (*app()).ctags.push((t, "#1c71d8", "#78aeed"));

    // colored tags carry light and dark Adwaita palette colors, applied by
    // apply_theme_colors and updated live on the style manager's notify::dark
    for (name, light, dark) in
        [("role-frame", "#a0aaaf", "#6f6f6f"), ("role-faint", "#5e5c64", "#9a999d"),
         ("role-bullet", "#e66100", "#ffa348"), ("role-image", "#613583", "#d56199"),
         ("role-done", "#26a269", "#2ec27e"), ("role-bar", "#1c71d8", "#3584e4")]
    {
        let t = tag(name);
        set_i(t, "foreground-set", 1);
        (*app()).ctags.push((t, light, dark));
    }

    // pinned family wherever columns must line up (see col_px and measure_mono)
    set_s(tag("lead"), "family", MONO_FAMILY);

    let t = tag("mono");
    set_s(t, "family", MONO_FAMILY);

    let t = tag("rule");
    set_s(t, "paragraph-background", "rgba(127,127,127,0.28)");
    set_i(t, "paragraph-background-set", 1);
    set_i(t, "pixels-above-lines", 8);
    set_i(t, "pixels-below-lines", 8);

    let t = tag("hit");
    set_s(t, "background", "rgba(246,201,83,0.45)");
    set_i(t, "background-set", 1);

    apply_theme_colors();
}

/// Paint the current Adwaita light/dark palette onto the registered colored
/// tags. Tag properties are live: the notify::dark handler repaints the whole
/// buffer by just updating the tags, with no re-render.
unsafe fn apply_theme_colors() {
    let is_dark = (l().adw_style_manager_get_dark)((l().adw_style_manager_get_default)()) != 0;
    for (t, light, dark) in (*app()).ctags.iter().copied() {
        set_s(t, "foreground", if is_dark { dark } else { light });
    }
}

// style manager notify::dark handler: (instance, param-spec, user_data).
unsafe extern "C" fn on_dark_changed(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { apply_theme_colors() }
}

/// Measure the real pixel width of one monospace column at the current zoom:
/// Pango lays out 200 `0` glyphs under the same explicit font description the
/// CSS zoom size implies (Pango units are 1/1024 px, unrounded). Called from
/// apply_css so margins and budgets follow every zoom change.
unsafe fn measure_mono() {
    let text = cstr(&"0".repeat(200));
    let layout = (l().gtk_widget_create_pango_layout)((*app()).view, std::ptr::null());
    let desc = (l().pango_font_description_from_string)(c"DejaVu Sans Mono".as_ptr());
    (l().pango_font_description_set_absolute_size)(desc, BASE_FONT_PX * (*app()).zoom * 1024.0);
    (l().pango_layout_set_font_description)(layout, desc);
    (l().pango_layout_set_text)(layout, text.as_ptr(), text.as_bytes().len() as c_int);
    let (mut w, mut h) = (0 as c_int, 0 as c_int);
    (l().pango_layout_get_size)(layout, &mut w, &mut h);
    (*app()).mono_px = w as f64 / 200.0 / 1024.0;
    (l().pango_font_description_free)(desc);
    (l().g_object_unref)(layout);
}

/// Measure `text` as it renders: a PangoLayout from the view (same context
/// and font options), the pinned mono family list, and the absolute size the
/// CSS zoom size implies. Width in pixels, unrounded (`get_size` returns Pango
/// units, 1/1024 px; `get_pixel_size` would round). NaN on any failure —
/// callers treat NaN as "skip fitting", never panic.
unsafe fn mono_run_px(text: &str) -> f64 {
    let view = (*app()).view;
    if view.is_null() {
        return f64::NAN;
    }
    let layout = (l().gtk_widget_create_pango_layout)(view, std::ptr::null());
    if layout.is_null() {
        return f64::NAN;
    }
    let family = cstr(MONO_FAMILY);
    let desc = (l().pango_font_description_from_string)(family.as_ptr());
    if desc.is_null() {
        (l().g_object_unref)(layout);
        return f64::NAN;
    }
    (l().pango_font_description_set_absolute_size)(desc, BASE_FONT_PX * (*app()).zoom * 1024.0);
    (l().pango_layout_set_font_description)(layout, desc);
    (l().pango_layout_set_text)(layout, text.as_ptr() as *const c_char, text.len() as c_int);
    let (mut w, mut h) = (0 as c_int, 0 as c_int);
    (l().pango_layout_get_size)(layout, &mut w, &mut h);
    (l().pango_font_description_free)(desc);
    (l().g_object_unref)(layout);
    w as f64 / 1024.0
}

/// Width of one monospace column at the current zoom, in pixels.
unsafe fn col_px() -> f64 {
    let m = (*app()).mono_px;
    if m > 1.0 {
        m
    } else {
        BASE_FONT_PX * (*app()).zoom * MONO_ADVANCE // before the first measure_mono()
    }
}

/// (Re)compute the pixel margins of one margin tag. Margins accumulate onto
/// the view's own margin; bullets and quote bars are monospace, so column
/// counts convert to pixels exactly. A negative Pango indent keeps the first
/// line at the margin and indents the wrapped lines, so quote bars and
/// bullets hang to the left of the text they introduce.
unsafe fn set_margins(t: Ptr, depth: usize, indent: usize, hanging: usize, code: bool) {
    let px = col_px();
    let cols = |n: usize| (n as f64 * px).round() as c_int;
    set_i(t, "accumulative-margin", 1);
    if code {
        // quote bars and gutters are buffer text now (see bar_text), so block
        // content starts flush at the margin, exactly like the terminal
        set_i(t, "left-margin", 0);
        set_i(t, "indent", 0);
    } else {
        // top-level list markers sit in a fixed two-column gutter
        let gutter = if depth == 0 && hanging > 0 { 2 } else { 0 };
        set_i(t, "left-margin", cols(indent + gutter));
        set_i(t, "indent", -cols(depth * 2 + hanging));
    }
}

unsafe fn ensure_margin(name: String, depth: usize, indent: usize, hanging: usize, code: bool) -> String {
    let a = &mut *app();
    if !a.ptags.contains_key(&name) {
        let t = tag(&name);
        if code {
            set_s(t, "family", MONO_FAMILY);
            set_s(t, "paragraph-background", "rgba(127,127,127,0.11)");
            set_i(t, "paragraph-background-set", 1);
        }
        set_margins(t, depth, indent, hanging, code);
        a.ptags.insert(name.clone(), (t, depth, indent, hanging, code));
    }
    name
}

/// Code-block tag per blockquote depth (one shaded block; bars sit in the
/// text at the margin, so the block itself needs no left inset).
unsafe fn ensure_cb(depth: usize) -> String {
    ensure_margin(format!("cb{depth}"), depth, 0, 0, true)
}

/// Paragraph margins per (quote depth, indent, hanging).
unsafe fn ensure_para_tag(depth: usize, indent: usize, hanging: usize) -> Option<String> {
    if depth == 0 && indent == 0 && hanging == 0 {
        return None;
    }
    Some(ensure_margin(format!("p{depth}-{indent}-{hanging}"), depth, indent, hanging, false))
}

// ------------------------------------------------------------- rendering ---

unsafe fn insert_text(text: &str, tags: &[String]) {
    if text.is_empty() {
        return;
    }
    let a = &mut *app();
    let mut it = TextIter::default();
    (l().gtk_text_buffer_get_end_iter)(a.buffer, &mut it);
    let t = cstr(text);
    if tags.is_empty() {
        (l().gtk_text_buffer_insert)(a.buffer, &mut it, t.as_ptr(), text.len() as c_int);
        return;
    }
    // pad to a fixed number of names: "base" repeats are harmless
    debug_assert!(tags.len() <= 10);
    let mut names: Vec<CString> = tags.iter().map(|s| cstr(s)).collect();
    while names.len() < 10 {
        names.push(cstr("base"));
    }
    (l().gtk_text_buffer_insert_with_tags_by_name)(
        a.buffer,
        &mut it,
        t.as_ptr(),
        t.as_bytes().len() as c_int,
        names[0].as_ptr(),
        names[1].as_ptr(),
        names[2].as_ptr(),
        names[3].as_ptr(),
        names[4].as_ptr(),
        names[5].as_ptr(),
        names[6].as_ptr(),
        names[7].as_ptr(),
        names[8].as_ptr(),
        names[9].as_ptr(),
        std::ptr::null::<c_char>(),
    );
}

fn style_tag_names(s: &Span) -> Vec<String> {
    let mut t: Vec<String> = Vec::new();
    if s.style.contains(Style::BOLD) {
        t.push("bold".into());
    }
    if s.style.contains(Style::ITALIC) {
        t.push("italic".into());
    }
    if s.style.contains(Style::STRIKE) {
        t.push("strike".into());
    }
    if s.style.contains(Style::UNDER) {
        t.push("under".into());
    }
    let role = match s.role {
        Role::None => None,
        Role::Code => Some("role-code"),
        Role::Link => Some("role-link"),
        Role::Image => Some("role-image"),
        Role::Faint | Role::Todo => Some("role-faint"),
        Role::Bullet => Some("role-bullet"),
        Role::Done => Some("role-done"),
    };
    if let Some(r) = role {
        t.push(r.into());
    }
    t
}

/// Quote bars as buffer text: one `┃ ` per quote depth, occupying the
/// columns where a paragraph would hang its bar (the terminal does the same).
fn bar_text(depth: usize) -> String {
    "┃ ".repeat(depth)
}

/// True when `text` can render wider or narrower than its nominal columns:
/// a double-width char (CJK, emoji: DejaVu lacks them, and the fallback's
/// advance is not 2x mono) or any char >= U+2000 outside the box-drawing
/// block U+2500..U+257F (DejaVu draws those exactly). Pure ASCII, Cyrillic or
/// box runs never drift: they are never measured and never fitted, which
/// keeps the cost at zero and the risk of touching exact text at zero.
fn suspect(text: &str) -> bool {
    text.chars()
        .any(|c| char_width(c) == 2 || (c >= '\u{2000}' && !('\u{2500}'..='\u{257F}').contains(&c)))
}

/// The cached letter-spacing tag name that forces `text` to occupy exactly
/// `target` px, or None when no fitting happens: empty or non-suspect text,
/// a failed measurement, |delta| < 1 px (layout rounding, not drift), or a
/// tag that could not be created. Callers then behave exactly as before.
unsafe fn fit_run(text: &str, target: f64) -> Option<String> {
    if text.is_empty() || !suspect(text) {
        return None;
    }
    let measured = mono_run_px(text);
    if !measured.is_finite() {
        return None;
    }
    let delta = target - measured;
    if delta.abs() < 1.0 {
        return None;
    }
    let sp = (delta * 1024.0 / text.chars().count() as f64).round() as i64;
    let name = fit_name(sp);
    (!fit_tag(sp).is_null()).then_some(name)
}

/// Insert one code line, letter-spacing each wide run (CJK, emoji) to exactly
/// 2 columns per char so diagrams keep their column grid where the mono font
/// falls back. Plain segments keep the current tags and the runs stay in
/// insertion order; with nothing to fit the line is inserted in one call,
/// byte-identical to the pre-Phase-3 fast path.
unsafe fn emit_code_line(line: &str, tags: &[String]) {
    let mut fixes: Vec<(usize, usize, String)> = Vec::new(); // byte range -> fit tag
    for (a, b) in crate::ansi::wide_runs(line) {
        let run = &line[a..b];
        // a wide run is suspect by construction; fit_run re-checks and skips
        // anything it cannot measure
        if let Some(name) = fit_run(run, 2.0 * run.chars().count() as f64 * col_px()) {
            fixes.push((a, b, name));
        }
    }
    if fixes.is_empty() {
        insert_text(line, tags);
        return;
    }
    let mut pos = 0usize;
    for (a, b, name) in fixes {
        insert_text(&line[pos..a], tags);
        let mut rt = tags.to_vec();
        rt.push(name);
        insert_text(&line[a..b], &rt);
        pos = b;
    }
    insert_text(&line[pos..], tags);
}

/// Insert spans; the first `lead` columns (list bullet, task box) and the
/// quote bars are set in monospace so the hanging indent lines up exactly.
unsafe fn emit_spans(spans: &[Span], base: &[String], depth: usize, lead: usize) {
    if depth > 0 {
        let mut bt = base.to_vec();
        bt.push("role-bar".into());
        bt.push("lead".into());
        insert_text(&bar_text(depth), &bt);
    }
    let mut col = 0usize;
    for s in spans {
        let mut t = base.to_vec();
        t.extend(style_tag_names(s));
        if col < lead {
            t.push("lead".into());
        }
        col += s.text.chars().count();
        insert_text(&s.text, &t);
    }
}

/// Pre-wrap a paragraph/heading into visual lines that fit `width_px` pixels.
/// Body text renders in the proportional UI font, so column counts are
/// meaningless: a PangoLayout created from the view inherits its CSS font
/// (family and zoom-scaled size) and word-wraps the plain text; each visual
/// line becomes one fragment. Headings render at `font_scale` x the CSS size,
/// so the layout font gets that absolute size. On any failure (null layout or
/// line, offsets off a char boundary, no lines) the paragraph comes back as a
/// single unwrapped fragment — the view no longer wraps, so it would overflow
/// under the hscrollbar; never panic.
unsafe fn wrap_spans_px(spans: &[Span], width_px: i32, font_scale: f64) -> Vec<Vec<Span>> {
    let unwrapped = || vec![spans.to_vec()];
    let view = (*app()).view;
    if view.is_null() || width_px <= 0 {
        return unwrapped();
    }
    let layout = (l().gtk_widget_create_pango_layout)(view, std::ptr::null());
    if layout.is_null() {
        return unwrapped();
    }
    if font_scale != 1.0 {
        // NULL from get_font_description means "use the context font"
        let mut desc = (l().pango_layout_get_font_description)(layout);
        if desc.is_null() {
            let ctx = (l().pango_layout_get_context)(layout);
            if !ctx.is_null() {
                desc = (l().pango_context_get_font_description)(ctx);
            }
        }
        if !desc.is_null() {
            let copy = (l().pango_font_description_copy)(desc);
            if !copy.is_null() {
                // Pango units are 1/1024 px, like the CSS size the view uses
                (l().pango_font_description_set_absolute_size)(
                    copy,
                    BASE_FONT_PX * (*app()).zoom * font_scale * 1024.0,
                );
                (l().pango_layout_set_font_description)(layout, copy);
                (l().pango_font_description_free)(copy);
            }
        }
    }
    let text = spans_plain(spans);
    (l().pango_layout_set_text)(layout, text.as_ptr() as *const c_char, text.len() as c_int);
    (l().pango_layout_set_width)(layout, width_px * 1024); // PANGO_SCALE
    (l().pango_layout_set_wrap)(layout, PANGO_WRAP_WORD_CHAR);
    let mut frags: Vec<Vec<Span>> = Vec::new();
    let count = (l().pango_layout_get_line_count)(layout);
    // Visual-line byte ranges: the original design read PangoLayoutLine's
    // offset/length at struct bytes 16/20 (layout* 0, runs* 8). That ABI died
    // with Pango >= 1.44: the struct now holds a `s` access pointer at byte 16
    // and offset/length are 30-bit packed fields, so 16/20 reads pointer
    // garbage — exactly what tests/gui_smoke.py + the Broadway fixtures
    // detected on pango 1.57 (the fallback below fired, no crash). Derive the
    // same ranges from the public API instead: line k starts at the lowest
    // byte index whose line number is k (monotonic, binary-searchable).
    let line_at = |b: usize| -> c_int {
        let (mut line, mut x) = (0 as c_int, 0 as c_int);
        unsafe { (l().pango_layout_index_to_line_x)(layout, b as c_int, 0, &mut line, &mut x) };
        line
    };
    let mut ok = count >= 1;
    let mut starts: Vec<usize> = Vec::new();
    if ok {
        starts.push(0usize);
        for k in 1..count {
            if (l().pango_layout_get_line_readonly)(layout, k).is_null() {
                ok = false;
                break;
            }
            let (mut lo, mut hi) = (starts[k as usize - 1], text.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if line_at(mid) >= k {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            starts.push(lo);
        }
    }
    if ok {
        for k in 0..starts.len() {
            let a = starts[k];
            let b = if k + 1 < starts.len() { starts[k + 1] } else { text.len() };
            if !text.is_char_boundary(a) || !text.is_char_boundary(b) {
                ok = false; // never slice invalid UTF-8
                break;
            }
            frags.push(spans_slice(spans, a, b));
        }
    }
    (l().g_object_unref)(layout);
    if !ok || frags.is_empty() {
        return unwrapped();
    }
    frags
}

/// Pixel width available to pre-wrapped prose at this quote depth: the
/// column budget minus the quote bars (2 columns per depth) and one column
/// of slack against pixel rounding, so GTK never overflows the viewport and
/// no spurious horizontal scrollbar appears for prose-only documents.
unsafe fn wrap_px(depth: usize) -> i32 {
    let cols = (*app()).budget.saturating_sub(1 + depth * 2);
    ((cols as f64) * col_px()).max(0.0) as i32
}

/// One visual line of a pre-wrapped paragraph: skip Pango's empty trailing
/// fragment when other lines exist (the last text line already carries it).
fn is_droppable(frag: &[Span], total: usize) -> bool {
    total > 1 && spans_plain(frag).trim().is_empty()
}

unsafe fn emit(lines: &[Line], depth: usize) {
    for line in lines {
        match line {
            Line::Blank => {
                if depth > 0 {
                    insert_text(&bar_text(depth), &["lead".into(), "role-bar".into()]);
                }
                insert_text("\n", &[]);
            }
            Line::Rule => {
                if depth > 0 {
                    insert_text(&bar_text(depth), &["lead".into(), "role-bar".into()]);
                }
                insert_text("\n", &["rule".to_string()]);
            }
            Line::Heading { lvl, spans } => {
                let lvl = (*lvl).clamp(1, 6);
                let mut t = vec![format!("h{lvl}")];
                if let Some(p) = ensure_para_tag(depth, 0, 0) {
                    t.push(p);
                }
                // Pre-wrapped here: the view is WRAP_NONE, so each Pango line
                // becomes its own buffer line; every fragment carries the same
                // tags, and emit_spans bars quote depths on each of them.
                let frags = wrap_spans_px(spans, wrap_px(depth), HEAD_SCALES[lvl]);
                for frag in frags.iter() {
                    if is_droppable(frag, frags.len()) {
                        continue;
                    }
                    emit_spans(frag, &t, depth, 0);
                    insert_text("\n", &t);
                }
            }
            Line::Para { spans, indent, hanging } => {
                let mut t = Vec::new();
                if let Some(p) = ensure_para_tag(depth, *indent, *hanging) {
                    t.push(p);
                }
                let frags = wrap_spans_px(spans, wrap_px(depth), 1.0);
                for (k, frag) in frags.iter().enumerate() {
                    if is_droppable(frag, frags.len()) {
                        continue;
                    }
                    // the bullet hanging belongs to the first visual line
                    // only; the rest reuse the same margin tag
                    emit_spans(frag, &t, depth, if k == 0 { *hanging } else { 0 });
                    insert_text("\n", &t);
                }
            }
            Line::Code { lines, .. } => {
                let t = vec![ensure_cb(depth)];
                for cl in lines {
                    if depth > 0 {
                        insert_text(&bar_text(depth), &["lead".into(), "role-bar".into()]);
                    }
                    emit_code_line(cl, &t);
                    insert_text("\n", &t);
                }
            }
            Line::Table(t) => {
                // styled pieces: faint frame, bold header, and cell roles
                // (code, links) survive; padding stays plain mono
                for row in table_pieces(t) {
                    if depth > 0 {
                        insert_text(&bar_text(depth), &["lead".into(), "role-bar".into()]);
                    }
                    for p in row {
                        match p {
                            TablePiece::Frame(f) => {
                                insert_text(&f, &["mono".into(), "role-frame".into()])
                            }
                            TablePiece::Cell { spans, pad_l, pad_r, header } => {
                                insert_text(&" ".repeat(pad_l), &["mono".into()]);
                                // fit the whole cell (not the padding or frame)
                                // to its nominal width so the `│` that follows
                                // lands back on the column grid
                                let plain = spans_plain(&spans);
                                let fit = fit_run(&plain, spans_width(&spans) as f64 * col_px());
                                for s in spans {
                                    let mut tags = vec!["mono".to_string()];
                                    if header {
                                        tags.push("bold".into());
                                    }
                                    tags.extend(style_tag_names(&s));
                                    if let Some(f) = &fit {
                                        tags.push(f.clone());
                                    }
                                    insert_text(&s.text, &tags);
                                }
                                insert_text(&" ".repeat(pad_r), &["mono".into()]);
                            }
                        }
                    }
                    insert_text("\n", &["mono".into()]);
                }
            }
            Line::Quote { inner } => emit(inner, depth + 1),
        }
    }
}

// -------------------------------------------------------------- document ---

unsafe fn current_path() -> Option<String> {
    let a = &*app();
    a.files.get(a.idx).cloned()
}

/// Parse `src` with a `cols`-column budget and render it into the buffer.
/// Every re-render (resize, zoom, disk reload) rebuilds the whole buffer, so
/// the cursor offset and the top visible line are saved before the clear and
/// restored (clamped to the new buffer) afterwards.
unsafe fn render_source(src: &str, cols: usize) {
    let cfg = Cfg::new(false, cols, true);
    let parsed = parse(src, &cfg);
    let a = &mut *app();

    // --- view state before the buffer is cleared (silently skipped when the
    // view is null or not allocated yet, e.g. the initial load) ---
    let mut cur_off = -1 as c_int;
    let mut top_line = -1 as c_int;
    if !a.buffer.is_null() && !a.view.is_null() {
        let mark = (l().gtk_text_buffer_get_insert)(a.buffer);
        if !mark.is_null() {
            let mut it = TextIter::default();
            (l().gtk_text_buffer_get_iter_at_mark)(a.buffer, &mut it, mark);
            cur_off = (l().gtk_text_iter_get_offset)(&it);
        }
        // GdkRectangle: four c_ints (x, y, width, height)
        let mut rect = [0 as c_int; 4];
        (l().gtk_text_view_get_visible_rect)(a.view, rect.as_mut_ptr());
        if rect[3] > 0 {
            let mut it = TextIter::default();
            // the PangoLayoutLine* return and the line-top out-param are not
            // needed; a null return still fills the iter
            (l().gtk_text_view_get_line_at_y)(a.view, &mut it, rect[1], std::ptr::null_mut());
            top_line = (l().gtk_text_iter_get_line)(&it);
        }
    }

    (l().gtk_text_buffer_set_text)(a.buffer, c"".as_ptr(), 0);
    emit(&parsed, 0);

    // --- put the cursor and the top line back, clamped to the new buffer ---
    if cur_off > 0 || top_line > 0 {
        let mut end = TextIter::default();
        (l().gtk_text_buffer_get_end_iter)(a.buffer, &mut end);
        if cur_off > 0 {
            let end_off = (l().gtk_text_iter_get_offset)(&end);
            let mut it = TextIter::default();
            (l().gtk_text_buffer_get_iter_at_offset)(a.buffer, &mut it, cur_off.min(end_off));
            (l().gtk_text_buffer_place_cursor)(a.buffer, &it);
        }
        if top_line > 0 {
            let last = (l().gtk_text_iter_get_line)(&end);
            let mut it = TextIter::default();
            (l().gtk_text_buffer_get_iter_at_line)(a.buffer, &mut it, top_line.min(last));
            (l().gtk_text_view_scroll_to_iter)(a.view, &mut it, 0.0, 0, 0.0, 0.0);
        }
    }
}

/// Width of the text area in monospace columns: what one table line may use.
unsafe fn view_cols() -> usize {
    let a = &*app();
    let mut w = (l().gtk_widget_get_width)(a.view);
    if w < 300 {
        // not allocated yet (first load runs before the window is realized):
        // fall back to the window's configured default width
        let (mut dw, mut dh) = (0 as c_int, 0 as c_int);
        (l().gtk_window_get_default_size)(a.win, &mut dw, &mut dh);
        w = dw;
    }
    // 56 = the two 28 px view margins, 16 = scrollbar allowance
    (((w - 56 - 16) as f64 / col_px()).floor() as usize).clamp(24, 400)
}

/// Re-parse and re-emit the cached source for the current zoom and window
/// width, without touching disk or the title. render_source preserves the
/// cursor and the top visible line across the re-render.
unsafe fn relayout_cached() {
    let cols = view_cols();
    let Some(src) = (*app()).src_cache.clone() else { return };
    (*app()).budget = cols;
    render_source(&src, cols);
}

unsafe fn load_current() {
    let src = match &(*app()).content {
        Some(c) => c.clone(),
        None => match current_path() {
            Some(p) => match std::fs::read(&p) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => format!("mdglass: cannot read {p}: {e}"),
            },
            None => String::new(),
        },
    };
    let cols = view_cols();
    (*app()).src_cache = Some(src.clone());
    (*app()).budget = cols;
    render_source(&src, cols);

    let shown = match &(*app()).content {
        Some(_) => "(stdin)".to_string(),
        None => current_path().unwrap_or_default(),
    };
    let (name, dir) = if shown == "(stdin)" {
        ("(stdin)".to_string(), String::new())
    } else {
        let p = Path::new(&shown);
        (
            p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(shown.clone()),
            p.parent().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        )
    };
    set_s((*app()).wtitle, "title", &name);
    set_s((*app()).wtitle, "subtitle", &dir);
    let t = cstr(&format!("{name} - MD Glass"));
    (l().gtk_window_set_title)((*app()).win, t.as_ptr());
    if (*app()).status_shown {
        (*app()).status_shown = false;
        (l().gtk_scrolled_window_set_child)((*app()).scrolled, (*app()).view);
    }
    (*app()).mtime = current_path().and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
}

// ----------------------------------------------------------------- find ---

const MAX_HITS: usize = 5000;

unsafe fn clear_hits() {
    let a = &*app();
    let (mut s, mut e) = (TextIter::default(), TextIter::default());
    (l().gtk_text_buffer_get_bounds)(a.buffer, &mut s, &mut e);
    let n = cstr("hit");
    (l().gtk_text_buffer_remove_tag_by_name)(a.buffer, n.as_ptr(), &s, &e);
}

unsafe fn do_search(from_top: bool, backward: bool) {
    let a = &*app();
    let needle = CStr::from_ptr((l().gtk_editable_get_text)(a.entry)).to_string_lossy().into_owned();
    clear_hits();
    if needle.trim().is_empty() {
        (l().gtk_label_set_text)(a.hits, c"".as_ptr());
        return;
    }
    let needle_c = cstr(needle.trim());
    let mut hits: Vec<(TextIter, TextIter)> = Vec::new();
    let mut it = TextIter::default();
    (l().gtk_text_buffer_get_start_iter)(a.buffer, &mut it);
    loop {
        let (mut s, mut e) = (TextIter::default(), TextIter::default());
        let found =
            (l().gtk_text_iter_forward_search)(&it, needle_c.as_ptr(), SEARCH_FLAGS, &mut s, &mut e, std::ptr::null()); // no limit
        if found == 0 {
            break;
        }
        let n = cstr("hit");
        (l().gtk_text_buffer_apply_tag_by_name)(a.buffer, n.as_ptr(), &s, &e);
        if hits.len() < MAX_HITS {
            hits.push((s, e));
        }
        it = e;
    }
    let label = if hits.is_empty() {
        "no results".to_string()
    } else {
        let mut cursor = TextIter::default();
        (l().gtk_text_buffer_get_iter_at_mark)(a.buffer, &mut cursor, (l().gtk_text_buffer_get_insert)(a.buffer));
        let mut pick = if from_top { Some(0) } else { None };
        if pick.is_none() {
            if backward {
                pick = hits.iter().rposition(|h| (l().gtk_text_iter_compare)(&h.0, &cursor) < 0);
                if pick.is_none() {
                    pick = Some(hits.len() - 1);
                }
            } else {
                pick = hits.iter().position(|h| (l().gtk_text_iter_compare)(&h.0, &cursor) >= 0);
                if pick.is_none() {
                    pick = Some(0);
                }
            }
        }
        let k = pick.unwrap_or(0);
        let mut target = hits[k].1;
        (l().gtk_text_buffer_place_cursor)(a.buffer, &hits[k].1);
        (l().gtk_text_view_scroll_to_iter)(a.view, &mut target, 0.3, 0, 0.0, 0.5);
        format!("{} of {}", k + 1, hits.len())
    };
    let lc = cstr(&label);
    (l().gtk_label_set_text)(a.hits, lc.as_ptr());
}

unsafe fn show_findbar() {
    let a = &mut *app();
    (l().gtk_widget_set_visible)(a.findbar, 1);
    a.find_shown = true;
    (l().gtk_widget_grab_focus)(a.entry);
}

unsafe fn hide_findbar() {
    let a = &mut *app();
    (l().gtk_widget_set_visible)(a.findbar, 0);
    a.find_shown = false;
    clear_hits();
    (l().gtk_widget_grab_focus)(a.view);
}

// --------------------------------------------------------------- zoom ---

unsafe fn apply_css() {
    measure_mono(); // margins below convert columns with the measured advance
    let zoom = (*app()).zoom;
    let css = format!(
        ".mdglassview {{ font-size: {:.1}px; }}\n.mdglassfind {{ border-top: 1px solid alpha(currentColor, 0.12); }}\n",
        BASE_FONT_PX * zoom
    );
    let c = cstr(&css);
    (l().gtk_css_provider_load_from_string)((*app()).css, c.as_ptr());
    let tags: Vec<_> = (*app()).ptags.values().copied().collect();
    for (t, depth, indent, hanging, code) in tags {
        set_margins(t, depth, indent, hanging, code);
    }
}

// --------------------------------------------------------------- actions ---

unsafe fn select_file(delta: isize) {
    let a = &mut *app();
    if a.files.len() < 2 {
        return;
    }
    let n = a.files.len() as isize;
    a.idx = (((a.idx as isize + delta) % n + n) % n) as usize;
    load_current();
}

unsafe extern "C" fn act_open(_action: Ptr, _param: Ptr, _data: Ptr) {
    let a = &*app();
    if a.dialog.is_null() {
        (*app()).dialog = (l().gtk_file_dialog_new)();
    }
    (l().gtk_file_dialog_open)(
        (*app()).dialog,
        a.win,
        std::ptr::null_mut(),
        Some(on_dialog_done),
        std::ptr::null_mut(),
    );
}

unsafe extern "C" fn on_dialog_done(source: Ptr, result: Ptr, _data: Ptr) {
    let mut err: Ptr = std::ptr::null_mut();
    let gfile = (l().gtk_file_dialog_open_finish)(source, result, &mut err);
    if gfile.is_null() {
        return; // cancelled or error (one leaked GError per failed open at worst)
    }
    let path = CStr::from_ptr((l().g_file_peek_path)(gfile)).to_string_lossy().into_owned();
    let a = &mut *app();
    if let Some(pos) = a.files.iter().position(|f| *f == path) {
        a.idx = pos;
    } else {
        a.files.push(path);
        a.idx = a.files.len() - 1;
    }
    a.content = None;
    load_current();
}

unsafe extern "C" fn act_find(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { show_findbar() }
}

unsafe extern "C" fn act_reload(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { load_current() }
}

unsafe extern "C" fn act_zoom_in(_: Ptr, _: Ptr, _: Ptr) {
    unsafe {
        (*app()).zoom = ((*app()).zoom * 1.15).min(3.0);
        apply_css(); // re-measures mono_px
        relayout_cached(); // prose re-wraps and tables re-fit; view state preserved
    }
}

unsafe extern "C" fn act_zoom_out(_: Ptr, _: Ptr, _: Ptr) {
    unsafe {
        (*app()).zoom = ((*app()).zoom / 1.15).max(0.5);
        apply_css(); // re-measures mono_px
        relayout_cached(); // prose re-wraps and tables re-fit; view state preserved
    }
}

unsafe extern "C" fn act_zoom_reset(_: Ptr, _: Ptr, _: Ptr) {
    unsafe {
        (*app()).zoom = 1.0;
        apply_css(); // re-measures mono_px
        relayout_cached(); // prose re-wraps and tables re-fit; view state preserved
    }
}

unsafe extern "C" fn act_next(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { select_file(1) }
}

unsafe extern "C" fn act_prev(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { select_file(-1) }
}

unsafe extern "C" fn act_quit(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { (l().gtk_window_close)((*app()).win) }
}

// window destroy handler: (widget, user_data). Once the window is gone the
// polling ticks must not touch the view or the disposed window any more;
// nulling win is what their is_null guards check.
unsafe extern "C" fn on_win_destroy(_: Ptr, _: Ptr) {
    unsafe { (*app()).win = std::ptr::null_mut() }
}

// small-button handlers: (widget, user_data)

unsafe extern "C" fn click_find_next(_: Ptr, _: Ptr) {
    unsafe { do_search(false, false) }
}

unsafe extern "C" fn click_find_prev(_: Ptr, _: Ptr) {
    unsafe { do_search(false, true) }
}

unsafe extern "C" fn click_find_close(_: Ptr, _: Ptr) {
    unsafe { hide_findbar() }
}

unsafe extern "C" fn entry_changed(_: Ptr, _: Ptr) {
    unsafe { do_search(true, false) }
}

unsafe extern "C" fn entry_activate(_: Ptr, _: Ptr) {
    unsafe { do_search(false, false) }
}

// key-pressed handler: (controller, keyval, keycode, state, user_data).
// Returning FALSE lets the event continue to the entry's own handling.
unsafe extern "C" fn key_press(_ctrl: Ptr, keyval: c_uint, _code: c_uint, _state: c_uint, _data: Ptr) -> c_int {
    if keyval == KEY_ESCAPE {
        unsafe {
            if (*app()).findbar_visible() {
                hide_findbar();
            }
        }
    }
    0
}

// A resize is picked up by polling in on_relayout_tick: GtkWidget's
// notify::width proved unreliable here (never fired for the initial 0 -> w
// allocation), and one FFI width read per tick is as cheap as the mtime poll.
unsafe extern "C" fn on_relayout_tick(_data: Ptr) -> c_int {
    unsafe {
        let a = &*app();
        if !a.status_shown && !a.win.is_null() && view_cols() != a.budget {
            relayout_cached();
        }
    }
    1
}

unsafe extern "C" fn on_tick(_data: Ptr) -> c_int {
    unsafe {
        let a = &*app();
        if !a.win.is_null() && a.content.is_none() {
            if let Some(p) = current_path() {
                if let Ok(t) = std::fs::metadata(&p).map(|m| m.modified()) {
                    if let Ok(t) = t {
                        if a.mtime.is_some() && a.mtime != Some(t) {
                            load_current();
                        }
                    }
                }
            }
        }
    }
    1
}

unsafe extern "C" fn on_activate(_app: Ptr, _data: Ptr) {
    let a = &mut *app();
    if !a.win.is_null() {
        return; // secondary activations are ignored (NON_UNIQUE apps have none anyway)
    }
    a.win = (l().adw_application_window_new)(a.app);
    (l().gtk_window_set_default_size)(a.win, 1100, 780);
    connect(a.win, "destroy", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(on_win_destroy));

    let toolbar = (l().adw_toolbar_view_new)();
    let header = (l().adw_header_bar_new)();
    a.wtitle = (l().adw_window_title_new)(c"MD Glass".as_ptr(), c"".as_ptr());
    (l().adw_header_bar_set_title_widget)(header, a.wtitle);
    let menu_btn = (l().gtk_menu_button_new)();
    set_s(menu_btn, "icon-name", "open-menu-symbolic");
    set_b(menu_btn, "primary", true);
    build_menu(menu_btn);
    (l().adw_header_bar_pack_end)(header, menu_btn);
    (l().adw_toolbar_view_add_top_bar)(toolbar, header);

    let vbox = (l().gtk_box_new)(ORIENT_VERTICAL, 0);
    a.scrolled = (l().gtk_scrolled_window_new)();
    (l().gtk_widget_set_hexpand)(a.scrolled, 1);
    (l().gtk_widget_set_vexpand)(a.scrolled, 1);
    // wide code/tables keep their shape and scroll horizontally; prose is
    // pre-wrapped in Rust (wrap_spans_px) so it never reaches the scrollbar
    (l().gtk_scrolled_window_set_policy)(a.scrolled, POLICY_AUTOMATIC, POLICY_AUTOMATIC);
    a.view = (l().gtk_text_view_new)();
    (l().g_object_ref_sink)(a.view); // survives being swapped out for the status page
    (l().gtk_text_view_set_wrap_mode)(a.view, WRAP_NONE);
    (l().gtk_text_view_set_editable)(a.view, 0);
    (l().gtk_text_view_set_cursor_visible)(a.view, 1);
    (l().gtk_text_view_set_left_margin)(a.view, 28);
    (l().gtk_text_view_set_right_margin)(a.view, 28);
    (l().gtk_text_view_set_top_margin)(a.view, 12);
    (l().gtk_text_view_set_bottom_margin)(a.view, 36);
    (l().gtk_widget_add_css_class)(a.view, c"mdglassview".as_ptr());
    a.buffer = (l().gtk_text_view_get_buffer)(a.view);
    make_tags();
    // live light/dark switch: apply_theme_colors updates the tags in place
    connect(
        (l().adw_style_manager_get_default)(),
        "notify::dark",
        cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(on_dark_changed),
    );
    (l().gtk_scrolled_window_set_child)(a.scrolled, a.view);

    a.status = build_status_page();
    (l().g_object_ref_sink)(a.status);
    a.status_shown = true;

    a.findbar = build_findbar();
    (l().gtk_box_append)(vbox, a.scrolled);
    (l().gtk_box_append)(vbox, a.findbar);
    (l().adw_toolbar_view_set_content)(toolbar, vbox);
    (l().adw_application_window_set_content)(a.win, toolbar);

    a.css = (l().gtk_css_provider_new)();
    let display = (l().gtk_widget_get_display)(a.view);
    (l().gtk_style_context_add_provider_for_display)(display, a.css, PROVIDER_PRIORITY);
    apply_css();

    add_actions();
    add_accels();

    if a.files.is_empty() && a.content.is_none() {
        (l().gtk_scrolled_window_set_child)(a.scrolled, a.status);
        a.status_shown = true;
    } else {
        a.status_shown = false;
        load_current();
    }
    (l().gtk_window_present)(a.win);
    (l().g_timeout_add_seconds)(2, Some(on_tick), std::ptr::null_mut());
    (l().g_timeout_add)(200, Some(on_relayout_tick), std::ptr::null_mut());
    if std::env::var_os("MDGLASS_GUI_SCRIPT").is_some() {
        (l().g_timeout_add)(700, Some(on_script_step), std::ptr::null_mut());
    }
}

// ------------------------------------------------------------- widgets ---

unsafe fn build_status_page() -> Ptr {
    let page = (l().adw_status_page_new)();
    (l().adw_status_page_set_title)(page, c"No document".as_ptr());
    (l().adw_status_page_set_description)(page, c"Open a Markdown file (Ctrl+O), or run mdglass --gui FILE".as_ptr());
    (l().gtk_widget_set_hexpand)(page, 1);
    (l().gtk_widget_set_vexpand)(page, 1);
    let btn = (l().gtk_button_new_with_label)(c"Open…".as_ptr());
    set_s(btn, "action-name", "win.open");
    set_i(btn, "halign", 3); // GTK_ALIGN_CENTER
    (l().gtk_widget_add_css_class)(btn, c"pill".as_ptr());
    (l().gtk_widget_add_css_class)(btn, c"suggested-action".as_ptr());
    (l().adw_status_page_set_child)(page, btn);
    page
}

unsafe fn build_findbar() -> Ptr {
    let bar = (l().gtk_box_new)(0 /* horizontal */, 6);
    (l().gtk_widget_add_css_class)(bar, c"mdglassfind".as_ptr());
    (l().gtk_widget_set_margin_start)(bar, 20);
    (l().gtk_widget_set_margin_end)(bar, 20);
    (l().gtk_widget_set_margin_top)(bar, 8);
    (l().gtk_widget_set_margin_bottom)(bar, 8);
    (l().gtk_widget_set_visible)(bar, 0);

    build_findbar_entry(&bar);

    let label = (l().gtk_label_new)(c" ".as_ptr());
    (l().gtk_widget_set_margin_end)(label, 8);
    set_hits_label(label);
    (l().gtk_box_append)(bar, label);
    build_findbar_close(&bar);
    bar
}

unsafe fn build_findbar_entry(bar: &Ptr) {
    let entry = (l().gtk_entry_new)();
    (l().gtk_widget_set_size_request)(entry, 320, -1);
    (l().gtk_widget_set_hexpand)(entry, 1);
    set_s(entry, "placeholder-text", "Find…");
    connect(entry, "changed", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(entry_changed));
    connect(entry, "activate", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(entry_activate));
    let ctrl = (l().gtk_event_controller_key_new)();
    (l().gtk_event_controller_set_propagation_phase)(ctrl, PHASE_CAPTURED);
    (l().gtk_widget_add_controller)(entry, ctrl);
    connect(ctrl, "key-pressed", cbptr::<unsafe extern "C" fn(Ptr, c_uint, c_uint, c_uint, Ptr) -> c_int>(key_press));
    (*app()).entry = entry;
    (*app()).findbar = *bar;
    (l().gtk_box_append)(*bar, entry);

    let prev = (l().gtk_button_new_from_icon_name)(c"go-up-symbolic".as_ptr());
    (l().gtk_widget_set_tooltip_text)(prev, c"Previous match (Shift+Enter unused; use the button)".as_ptr());
    connect(prev, "clicked", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(click_find_prev));
    (l().gtk_box_append)(*bar, prev);
    let next = (l().gtk_button_new_from_icon_name)(c"go-down-symbolic".as_ptr());
    connect(next, "clicked", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(click_find_next));
    (l().gtk_box_append)(*bar, next);
}

unsafe fn build_findbar_close(bar: &Ptr) {
    let close = (l().gtk_button_new_from_icon_name)(c"window-close-symbolic".as_ptr());
    (l().gtk_widget_set_tooltip_text)(close, c"Close find bar (Esc)".as_ptr());
    connect(close, "clicked", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(click_find_close));
    (l().gtk_box_append)(*bar, close);
}

unsafe fn set_hits_label(label: Ptr) {
    (*app()).hits = label;
}

unsafe fn build_menu(btn: Ptr) {
    let menu = (l().g_menu_new)();
    let add = | action: &str, label: &str | {
        let (a, lb) = (cstr(action), cstr(label));
        (l().g_menu_append)(menu, lb.as_ptr(), a.as_ptr());
    };
    add("win.open", "_Open…");
    add("win.find", "_Find…");
    add("win.reload", "_Reload");
    let section = (l().g_menu_new)();
    let sec_add = | menu: Ptr, action: &str, label: &str | {
        let (a, lb) = (cstr(action), cstr(label));
        (l().g_menu_append)(menu, lb.as_ptr(), a.as_ptr());
    };
    sec_add(section, "win.zoom-in", "Zoom _In");
    sec_add(section, "win.zoom-out", "Zoom _Out");
    sec_add(section, "win.zoom-reset", "_Reset Zoom");
    let s = cstr("View");
    (l().g_menu_append_submenu)(menu, s.as_ptr(), section);
    add("win.next-file", "Next _File");
    add("win.prev-file", "_Previous File");
    let section2 = (l().g_menu_new)();
    sec_add(section2, "win.quit", "_Close Window");
    let s2 = cstr("Window");
    (l().g_menu_append_submenu)(menu, s2.as_ptr(), section2);
    set_o(btn, "menu-model", menu);
    (l().g_object_unref)(menu);
}

unsafe fn set_o(o: Ptr, prop: &str, v: Ptr) {
    let p = cstr(prop);
    (l().g_object_set)(o, p.as_ptr(), v, std::ptr::null::<c_char>());
}

unsafe fn add_actions() {
    let win = (*app()).win;
    let add = | name: &str, cb: Ptr | {
        let n = cstr(name);
        let act = (l().g_simple_action_new)(n.as_ptr(), std::ptr::null_mut());
        connect(act, "activate", cb);
        (l().g_action_map_add_action)(win, act);
    };
    add("open", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_open));
    add("find", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_find));
    add("reload", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_reload));
    add("zoom-in", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_zoom_in));
    add("zoom-out", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_zoom_out));
    add("zoom-reset", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_zoom_reset));
    add("next-file", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_next));
    add("prev-file", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_prev));
    add("quit", cbptr::<unsafe extern "C" fn(Ptr, Ptr, Ptr)>(act_quit));
}

unsafe fn add_accels() {
    let app = (*app()).app;
    let accel = | action: &str, keys: &[&str] | {
        let a = cstr(action);
        let ks: Vec<CString> = keys.iter().map(|k| cstr(k)).collect();
        let mut ptrs: Vec<*const c_char> = ks.iter().map(|k| k.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        (l().gtk_application_set_accels_for_action)(app, a.as_ptr(), ptrs.as_ptr());
    };
    accel("win.open", &["<Control>o"]);
    accel("win.find", &["<Control>f"]);
    accel("win.reload", &["F5", "<Control>r"]);
    accel("win.zoom-in", &["<Control>plus", "<Control>equal"]);
    accel("win.zoom-out", &["<Control>minus"]);
    accel("win.zoom-reset", &["<Control>0"]);
    accel("win.next-file", &["<Alt>Right"]);
    accel("win.prev-file", &["<Alt>Left"]);
    accel("win.quit", &["<Control>w"]);
}


// ----------------------------------------------------------- test hook ---
// MDGLASS_GUI_SCRIPT="action:find;find:text;esc;shot:/tmp/a.png;quit" drives the
// real window for end-to-end tests (e.g. under GDK_BACKEND=broadway), one
// step every 700 ms. Not part of the user interface.

static mut SCRIPT_POS: usize = 0;
static mut SHOT_RETRIES: u32 = 0;

unsafe fn snapshot_png(path: &str) -> bool {
    let a = &*app();
    let (w, h) = ((l().gtk_widget_get_width)(a.win), (l().gtk_widget_get_height)(a.win));
    let paintable = (l().gtk_widget_paintable_new)(a.win);
    let snap = (l().gtk_snapshot_new)();
    (l().gdk_paintable_snapshot)(paintable, snap, w as f64, h as f64);
    let node = (l().gtk_snapshot_free_to_node)(snap);
    let renderer = (l().gtk_native_get_renderer)(a.win);
    let tex = if node.is_null() || renderer.is_null() {
        std::ptr::null_mut()
    } else {
        (l().gsk_renderer_render_texture)(renderer, node, std::ptr::null_mut())
    };
    let p = cstr(path);
    let ok = !tex.is_null() && (l().gdk_texture_save_to_png)(tex, p.as_ptr()) != 0;
    eprintln!(
        "mdglass-script: shot {path} {w}x{h} node={} renderer={} texture={} {}",
        !node.is_null(), !renderer.is_null(), !tex.is_null(), if ok { "ok" } else { "FAILED" }
    );
    (l().g_object_unref)(paintable);
    ok
}

unsafe extern "C" fn on_script_step(_data: Ptr) -> c_int {
    let script = std::env::var("MDGLASS_GUI_SCRIPT").unwrap_or_default();
    let steps: Vec<&str> = script.split(';').filter(|s| !s.is_empty()).collect();
    let pos = SCRIPT_POS;
    SCRIPT_POS += 1;
    let Some(step) = steps.get(pos) else { return 0 };
    let (kind, arg) = step.split_once(':').unwrap_or((step, ""));
    eprintln!("mdglass-script: {step}");
    let a = &*app();
    match kind {
        "action" => {
            let n = cstr(arg);
            (l().g_action_group_activate_action)(a.win, n.as_ptr(), std::ptr::null_mut());
        }
        "find" => {
            let t = cstr(arg);
            (l().gtk_editable_set_text)(a.entry, t.as_ptr());
            let lbl = CStr::from_ptr((l().gtk_label_get_text)(a.hits)).to_string_lossy().into_owned();
            eprintln!("mdglass-script: hits label = {lbl:?}");
        }
        "next" => click_find_next(std::ptr::null_mut(), std::ptr::null_mut()),
        "esc" => {
            key_press(std::ptr::null_mut(), KEY_ESCAPE, 0, 0, std::ptr::null_mut());
        }
        "shot" => {
            if !snapshot_png(arg) && SHOT_RETRIES < 10 {
                SHOT_RETRIES += 1;
                SCRIPT_POS -= 1; // no frame drawn yet: try again next tick
            } else {
                SHOT_RETRIES = 0;
            }
        }
        "quit" => {
            (l().gtk_window_close)(a.win);
            return 0;
        }
        _ => eprintln!("mdglass-script: unknown step {step}"),
    }
    1
}

// ----------------------------------------------------------------- run ---

/// Open the GNOME preview window. Blocks until the window is closed.
pub fn run(files: Vec<String>, content: Option<String>) -> Result<(), String> {
    let lib = load_lib()?;
    if LIB.set(lib).is_err() {
        return Err("gui::run called twice".into());
    }
    if unsafe { (l().gtk_init_check)() } == 0
    {
        return Err("no GNOME display available (need a Wayland or X11 session)".into());
    }
    let app = unsafe { (l().adw_application_new)(c"io.github.hpn777.MdGlass".as_ptr(), NON_UNIQUE) };
    if app.is_null() {
        return Err("cannot create Adwaita application".into());
    }
    let boxed = Box::new(App {
        app,
        win: std::ptr::null_mut(),
        view: std::ptr::null_mut(),
        buffer: std::ptr::null_mut(),
        wtitle: std::ptr::null_mut(),
        status: std::ptr::null_mut(),
        scrolled: std::ptr::null_mut(),
        dialog: std::ptr::null_mut(),
        findbar: std::ptr::null_mut(),
        entry: std::ptr::null_mut(),
        hits: std::ptr::null_mut(),
        css: std::ptr::null_mut(),
        zoom: 1.0,
        mono_px: 0.0,
        files,
        idx: 0,
        content,
        mtime: None,
        src_cache: None,
        budget: 120,
        ptags: HashMap::new(),
        ctags: Vec::new(),
        fit_tags: HashMap::new(),
        status_shown: false,
        find_shown: false,
    });
    unsafe {
        APP = Box::into_raw(boxed);
        connect(app, "activate", cbptr::<unsafe extern "C" fn(Ptr, Ptr)>(on_activate));
        let rc = (l().g_application_run)(app, 0, std::ptr::null_mut());
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("GUI exited with status {rc}"))
        }
    }
}
