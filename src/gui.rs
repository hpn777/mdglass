// Native GNOME back end: GTK4 + libadwaita, no WebKit, no build-time C
// dependency. The system's GNOME libraries are loaded at startup with dlopen
// (only runtime `.so` files are required, never the -dev packages), so the
// binary stays a single small file. Documents render into a styled GtkTextView
// with buffer tags: an AdwToolbarView + header bar, an open dialog, in-buffer
// find, zoom, next/prev file, and live reload when the file changes.

use std::sync::OnceLock;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::path::Path;
use std::time::SystemTime;

use crate::ansi::char_width;
use crate::vector;
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
/// PangoWrapMode WORD_CHAR. Not GtkWrapMode's WORD_CHAR (3): the two enums
/// order their members differently, and 3 is PANGO_WRAP_NONE.
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
    gtk_text_buffer_create_tag: unsafe extern "C" fn(Ptr, *const c_char, *const c_char, ...) -> Ptr,
    gtk_text_buffer_set_text: unsafe extern "C" fn(Ptr, *const c_char, c_int),
    gtk_text_buffer_get_end_iter: unsafe extern "C" fn(Ptr, *mut TextIter),
    gtk_text_buffer_get_start_iter: unsafe extern "C" fn(Ptr, *mut TextIter),
    gtk_text_buffer_get_insert: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_text_buffer_get_iter_at_mark: unsafe extern "C" fn(Ptr, *mut TextIter, Ptr),
    gtk_text_buffer_get_iter_at_offset: unsafe extern "C" fn(Ptr, *mut TextIter, c_int),
    gtk_text_view_get_visible_rect: unsafe extern "C" fn(Ptr, *mut c_int),
    gtk_text_view_get_line_at_y: unsafe extern "C" fn(Ptr, *mut TextIter, c_int, *mut c_int),
    gtk_text_view_scroll_to_mark: unsafe extern "C" fn(Ptr, Ptr, f64, c_int, f64, f64),
    gtk_text_buffer_create_child_anchor: unsafe extern "C" fn(Ptr, *const TextIter) -> Ptr,
    gtk_text_view_add_child_at_anchor: unsafe extern "C" fn(Ptr, Ptr, Ptr),
    gtk_picture_new: unsafe extern "C" fn() -> Ptr,
    gtk_picture_set_paintable: unsafe extern "C" fn(Ptr, Ptr),
    gdk_memory_texture_new: unsafe extern "C" fn(c_int, c_int, c_int, Ptr, usize) -> Ptr,
    g_bytes_new_with_free_func: unsafe extern "C" fn(Ptr, usize, Option<unsafe extern "C" fn(Ptr)>, Ptr) -> Ptr,
    gdk_pixbuf_loader_new_with_type: unsafe extern "C" fn(*const c_char, *mut Ptr) -> Ptr,
    gdk_pixbuf_loader_write: unsafe extern "C" fn(Ptr, *const u8, usize, *mut Ptr) -> c_int,
    gdk_pixbuf_loader_close: unsafe extern "C" fn(Ptr, *mut Ptr) -> c_int,
    gdk_pixbuf_loader_get_pixbuf: unsafe extern "C" fn(Ptr) -> Ptr,
    gdk_pixbuf_get_width: unsafe extern "C" fn(Ptr) -> c_int,
    gdk_pixbuf_get_height: unsafe extern "C" fn(Ptr) -> c_int,
    gdk_pixbuf_scale_simple: unsafe extern "C" fn(Ptr, c_int, c_int, c_int) -> Ptr,
    gdk_pixbuf_loader_set_size: unsafe extern "C" fn(Ptr, c_int, c_int),
    gdk_pixbuf_get_n_channels: unsafe extern "C" fn(Ptr) -> c_int,
    gdk_pixbuf_get_has_alpha: unsafe extern "C" fn(Ptr) -> c_int,
    gdk_pixbuf_get_bits_per_sample: unsafe extern "C" fn(Ptr) -> c_int,
    g_error_free: unsafe extern "C" fn(Ptr),
    gdk_pixbuf_get_pixels: unsafe extern "C" fn(Ptr) -> Ptr,
    gdk_pixbuf_get_rowstride: unsafe extern "C" fn(Ptr) -> c_int,
    g_bytes_unref: unsafe extern "C" fn(Ptr),
    gtk_text_view_get_iter_location: unsafe extern "C" fn(Ptr, *const TextIter, *mut c_int),
    gtk_scrolled_window_get_hadjustment: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_scrolled_window_get_vadjustment: unsafe extern "C" fn(Ptr) -> Ptr,
    gtk_adjustment_get_value: unsafe extern "C" fn(Ptr) -> f64,
    gtk_adjustment_set_value: unsafe extern "C" fn(Ptr, f64),
    gtk_adjustment_get_upper: unsafe extern "C" fn(Ptr) -> f64,
    gtk_adjustment_get_page_size: unsafe extern "C" fn(Ptr) -> f64,
    gtk_text_buffer_create_mark: unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, c_int) -> Ptr,
    gtk_text_buffer_move_mark: unsafe extern "C" fn(Ptr, Ptr, *const TextIter),
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
    pango_layout_set_markup: unsafe extern "C" fn(Ptr, *const c_char, c_int),
    pango_layout_get_line_count: unsafe extern "C" fn(Ptr) -> c_int,
    pango_layout_get_line_readonly: unsafe extern "C" fn(Ptr, c_int) -> Ptr,
    pango_layout_line_get_start_index: unsafe extern "C" fn(Ptr) -> c_int,
    pango_layout_get_context: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_context_get_font_description: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_font_description_copy: unsafe extern "C" fn(Ptr) -> Ptr,
    pango_layout_set_font_description: unsafe extern "C" fn(Ptr, Ptr),
    pango_font_description_from_string: unsafe extern "C" fn(*const c_char) -> Ptr,
    pango_font_description_set_absolute_size: unsafe extern "C" fn(Ptr, f64),
    pango_font_description_set_weight: unsafe extern "C" fn(Ptr, c_int),
    pango_font_description_free: unsafe extern "C" fn(Ptr),
    g_object_set: unsafe extern "C" fn(Ptr, *const c_char, ...),
    g_object_unref: unsafe extern "C" fn(Ptr),
    g_object_ref: unsafe extern "C" fn(Ptr) -> Ptr,
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
        // diagram pictures; gdk-pixbuf is a hard GTK4 dependency, the SVG
        // loader inside it (librsvg) is the only soft one: without it the
        // loader simply fails and blocks stay code
        let pixbuf = expect(dlopen(c"libgdk_pixbuf-2.0.so.0".as_ptr(), RTLD_LAZY | RTLD_GLOBAL), "libgdk_pixbuf-2.0")?;
        let mut missing: Vec<String> = Vec::new();
        macro_rules! g {
            ($h:expr, $n:literal, $t:ty) => {
                match tf::<$t>($h, $n) {
                    Ok(f) => f,
                    Err(e) => { missing.push(e); std::mem::transmute::<*mut c_void, $t>(std::ptr::null_mut()) }
                }
            };
        }
        // picture-pipeline symbols (gdk-pixbuf and its diagnostics): a
        // machine missing one keeps code blocks as text - the GUI starts
        // and only loses the pictures (picture_for checks for nulls)
        macro_rules! gopt {
            ($h:expr, $n:literal, $t:ty) => {
                match tf::<$t>($h, $n) {
                    Ok(f) => f,
                    Err(_) => std::mem::transmute::<*mut c_void, $t>(std::ptr::null_mut()),
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
            (pixbuf $n:literal, $t:ty) => { g!(pixbuf, $n, $t) };
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
            gtk_text_buffer_create_tag: gs!("gtk_text_buffer_create_tag", unsafe extern "C" fn(Ptr, *const c_char, *const c_char, ...) -> Ptr),
            gtk_text_buffer_set_text: gs!("gtk_text_buffer_set_text", unsafe extern "C" fn(Ptr, *const c_char, c_int)),
            gtk_text_buffer_get_end_iter: gs!("gtk_text_buffer_get_end_iter", unsafe extern "C" fn(Ptr, *mut TextIter)),
            gtk_text_buffer_get_start_iter: gs!("gtk_text_buffer_get_start_iter", unsafe extern "C" fn(Ptr, *mut TextIter)),
            gtk_text_buffer_get_insert: gs!("gtk_text_buffer_get_insert", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_text_buffer_get_iter_at_mark: gs!("gtk_text_buffer_get_iter_at_mark", unsafe extern "C" fn(Ptr, *mut TextIter, Ptr)),
            gtk_text_buffer_get_iter_at_offset: gs!("gtk_text_buffer_get_iter_at_offset", unsafe extern "C" fn(Ptr, *mut TextIter, c_int)),
            gtk_text_view_get_visible_rect: gs!("gtk_text_view_get_visible_rect", unsafe extern "C" fn(Ptr, *mut c_int)),
            gtk_text_view_get_line_at_y: gs!("gtk_text_view_get_line_at_y", unsafe extern "C" fn(Ptr, *mut TextIter, c_int, *mut c_int)),
            gtk_text_view_scroll_to_mark: gs!("gtk_text_view_scroll_to_mark", unsafe extern "C" fn(Ptr, Ptr, f64, c_int, f64, f64)),
            gtk_text_buffer_create_child_anchor: gs!("gtk_text_buffer_create_child_anchor", unsafe extern "C" fn(Ptr, *const TextIter) -> Ptr),
            gtk_text_view_add_child_at_anchor: gs!("gtk_text_view_add_child_at_anchor", unsafe extern "C" fn(Ptr, Ptr, Ptr)),
            gtk_picture_new: gs!("gtk_picture_new", unsafe extern "C" fn() -> Ptr),
            gtk_picture_set_paintable: gs!("gtk_picture_set_paintable", unsafe extern "C" fn(Ptr, Ptr)),
            gdk_memory_texture_new: gs!("gdk_memory_texture_new", unsafe extern "C" fn(c_int, c_int, c_int, Ptr, usize) -> Ptr),
            g_bytes_new_with_free_func: gs!(glib "g_bytes_new_with_free_func", unsafe extern "C" fn(Ptr, usize, Option<unsafe extern "C" fn(Ptr)>, Ptr) -> Ptr),
            gdk_pixbuf_loader_new_with_type: gopt!(pixbuf, "gdk_pixbuf_loader_new_with_type", unsafe extern "C" fn(*const c_char, *mut Ptr) -> Ptr),
            gdk_pixbuf_loader_write: gopt!(pixbuf, "gdk_pixbuf_loader_write", unsafe extern "C" fn(Ptr, *const u8, usize, *mut Ptr) -> c_int),
            gdk_pixbuf_loader_close: gopt!(pixbuf, "gdk_pixbuf_loader_close", unsafe extern "C" fn(Ptr, *mut Ptr) -> c_int),
            gdk_pixbuf_loader_get_pixbuf: gopt!(pixbuf, "gdk_pixbuf_loader_get_pixbuf", unsafe extern "C" fn(Ptr) -> Ptr),
            gdk_pixbuf_get_width: gopt!(pixbuf, "gdk_pixbuf_get_width", unsafe extern "C" fn(Ptr) -> c_int),
            gdk_pixbuf_get_height: gopt!(pixbuf, "gdk_pixbuf_get_height", unsafe extern "C" fn(Ptr) -> c_int),
            gdk_pixbuf_scale_simple: gopt!(pixbuf, "gdk_pixbuf_scale_simple", unsafe extern "C" fn(Ptr, c_int, c_int, c_int) -> Ptr),
            gdk_pixbuf_loader_set_size: gopt!(pixbuf, "gdk_pixbuf_loader_set_size", unsafe extern "C" fn(Ptr, c_int, c_int)),
            gdk_pixbuf_get_n_channels: gopt!(pixbuf, "gdk_pixbuf_get_n_channels", unsafe extern "C" fn(Ptr) -> c_int),
            gdk_pixbuf_get_has_alpha: gopt!(pixbuf, "gdk_pixbuf_get_has_alpha", unsafe extern "C" fn(Ptr) -> c_int),
            gdk_pixbuf_get_bits_per_sample: gopt!(pixbuf, "gdk_pixbuf_get_bits_per_sample", unsafe extern "C" fn(Ptr) -> c_int),
            g_error_free: gopt!(glib, "g_error_free", unsafe extern "C" fn(Ptr)),
            gdk_pixbuf_get_pixels: gopt!(pixbuf, "gdk_pixbuf_get_pixels", unsafe extern "C" fn(Ptr) -> Ptr),
            gdk_pixbuf_get_rowstride: gopt!(pixbuf, "gdk_pixbuf_get_rowstride", unsafe extern "C" fn(Ptr) -> c_int),
            g_bytes_unref: gs!(glib "g_bytes_unref", unsafe extern "C" fn(Ptr)),
            gtk_text_view_get_iter_location: gs!("gtk_text_view_get_iter_location", unsafe extern "C" fn(Ptr, *const TextIter, *mut c_int)),
            gtk_scrolled_window_get_hadjustment: gs!("gtk_scrolled_window_get_hadjustment", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_scrolled_window_get_vadjustment: gs!("gtk_scrolled_window_get_vadjustment", unsafe extern "C" fn(Ptr) -> Ptr),
            gtk_adjustment_get_value: gs!("gtk_adjustment_get_value", unsafe extern "C" fn(Ptr) -> f64),
            gtk_adjustment_set_value: gs!("gtk_adjustment_set_value", unsafe extern "C" fn(Ptr, f64)),
            gtk_adjustment_get_upper: gs!("gtk_adjustment_get_upper", unsafe extern "C" fn(Ptr) -> f64),
            gtk_adjustment_get_page_size: gs!("gtk_adjustment_get_page_size", unsafe extern "C" fn(Ptr) -> f64),
            gtk_text_buffer_create_mark: gs!("gtk_text_buffer_create_mark", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, c_int) -> Ptr),
            gtk_text_buffer_move_mark: gs!("gtk_text_buffer_move_mark", unsafe extern "C" fn(Ptr, Ptr, *const TextIter)),
            gtk_text_buffer_insert: gs!("gtk_text_buffer_insert", unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int)),
            gtk_text_buffer_insert_with_tags_by_name: gs!("gtk_text_buffer_insert_with_tags_by_name", unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int, *const c_char, ...)),
            gtk_text_buffer_place_cursor: gs!("gtk_text_buffer_place_cursor", unsafe extern "C" fn(Ptr, *const TextIter)),
            gtk_text_buffer_apply_tag_by_name: gs!("gtk_text_buffer_apply_tag_by_name", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter)),
            gtk_text_buffer_remove_tag_by_name: gs!("gtk_text_buffer_remove_tag_by_name", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter)),
            gtk_text_buffer_get_bounds: gs!("gtk_text_buffer_get_bounds", unsafe extern "C" fn(Ptr, *mut TextIter, *mut TextIter)),
            gtk_text_iter_forward_search: gs!("gtk_text_iter_forward_search", unsafe extern "C" fn(*const TextIter, *const c_char, c_uint, *mut TextIter, *mut TextIter, *const TextIter) -> c_int),
            gtk_text_iter_compare: gs!("gtk_text_iter_compare", unsafe extern "C" fn(*const TextIter, *const TextIter) -> c_int),
            gtk_text_iter_get_offset: gs!("gtk_text_iter_get_offset", unsafe extern "C" fn(*const TextIter) -> c_int),
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
            pango_layout_set_markup: gs!(pango "pango_layout_set_markup", unsafe extern "C" fn(Ptr, *const c_char, c_int)),
            pango_layout_get_line_count: gs!(pango "pango_layout_get_line_count", unsafe extern "C" fn(Ptr) -> c_int),
            pango_layout_get_line_readonly: gs!(pango "pango_layout_get_line_readonly", unsafe extern "C" fn(Ptr, c_int) -> Ptr),
            pango_layout_line_get_start_index: gs!(pango "pango_layout_line_get_start_index", unsafe extern "C" fn(Ptr) -> c_int),
            pango_layout_get_context: gs!(pango "pango_layout_get_context", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_context_get_font_description: gs!(pango "pango_context_get_font_description", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_font_description_copy: gs!(pango "pango_font_description_copy", unsafe extern "C" fn(Ptr) -> Ptr),
            pango_layout_set_font_description: gs!(pango "pango_layout_set_font_description", unsafe extern "C" fn(Ptr, Ptr)),
            pango_font_description_from_string: gs!(pango "pango_font_description_from_string", unsafe extern "C" fn(*const c_char) -> Ptr),
            pango_font_description_set_absolute_size: gs!(pango "pango_font_description_set_absolute_size", unsafe extern "C" fn(Ptr, f64)),
            pango_font_description_set_weight: gs!(pango "pango_font_description_set_weight", unsafe extern "C" fn(Ptr, c_int)),
            pango_font_description_free: gs!(pango "pango_font_description_free", unsafe extern "C" fn(Ptr)),
            g_object_set: gs!(go "g_object_set", unsafe extern "C" fn(Ptr, *const c_char, ...)),
            g_object_unref: gs!(go "g_object_unref", unsafe extern "C" fn(Ptr)),
            g_object_ref: gs!(go "g_object_ref", unsafe extern "C" fn(Ptr) -> Ptr),
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
    /// chars inserted so far by the current render (buffer offsets are chars)
    emit_off: i32,
    /// buffer offset where each emitted block starts, in emission order, and
    /// the (offset, len) runs that exist only because a paragraph was wrapped
    /// (newline, bars and hanging pad of continuation lines): together they
    /// map view positions to anchors that survive a re-wrap (to_anchor)
    block_starts: Vec<i32>,
    synth: Vec<(i32, i32)>,
    /// where the top visible line goes back to after a re-render
    top_mark: Ptr,
    /// reused PangoLayout for wrap_spans_px (font and width are set per call)
    wlayout: Ptr,
    /// a scroll still converging after a re-render (see scroll_settled):
    /// the mark, whether it goes to the top (else: just into view), how many
    /// more ticks, the last vadjustment upper seen, and the top anchor being
    /// restored (reused if another render comes before it settled)
    rescroll: Option<(Ptr, bool)>,
    rescroll_left: u8,
    rescroll_upper: f64,
    pending_top: Option<(usize, i32)>,
    /// raw engine SVG per diagram block, keyed by (info string, source):
    /// theme-independent, so a light/dark flip never re-runs an engine nor
    /// respawns the CLI. None = failed, also cached, so a broken block never
    /// re-runs the engine on every frame. Cleared when another document loads.
    svg_cache: HashMap<(String, String), Option<String>>,
    /// decoded picture per (info string, source, dark) at the zoom it was
    /// made for; re-emits at the same zoom (scroll, resize, live reload)
    /// reuse it without touching librsvg again. Each entry holds one
    /// reference to its GdkPixbuf, dropped when the entry is pruned.
    raster_cache: HashMap<(String, String, bool), (f64, Ptr)>,
    /// keys the current emit actually used; the caches shrink to these at
    /// the end of every render, so live reloads cannot pile up revisions
    /// forever (they re-render with keep = true and would never clear)
    used_svg: HashSet<(String, String)>,
    used_raster: HashSet<(String, String, bool)>,
    /// the current document shows at least one picture (a theme flip must
    /// re-emit those: palettes are baked into the SVG at render time)
    has_diagrams: bool,
    /// resize debounce: a new column budget must be seen on two ticks in a row
    seen_cols: usize,
    seen_count: u8,
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
    let above: [c_int; 7] = [0, 14, 12, 10, 8, 8, 8];
    for lvl in 1..=6 {
        let t = tag(&format!("h{lvl}"));
        set_i(t, "weight", WEIGHT_BOLD);
        set_f(t, "scale", HEAD_SCALES[lvl]);
        set_i(t, "pixels-above-lines", above[lvl]);
        set_i(t, "pixels-below-lines", 2);
        // continuation lines of a pre-wrapped heading: same face, no gap above
        let t = tag(&format!("h{lvl}c"));
        set_i(t, "weight", WEIGHT_BOLD);
        set_f(t, "scale", HEAD_SCALES[lvl]);
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

    // rules are drawn as a line of `─` (see emit): GTK paints no paragraph
    // background on an empty line, so the old shaded-band rule never showed
    let t = tag("rule");
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
    unsafe {
        apply_theme_colors();
        // tags repaint live, but a picture's palette is baked in at render
        // time: re-emit the document when it shows any
        if (*app()).has_diagrams && (*app()).src_cache.is_some() {
            relayout_cached();
        }
    }
}

/// Measure the real pixel width of one monospace column at the current zoom:
/// 200 `0` glyphs laid out exactly like mono text renders (mono_run_px: the
/// pinned family list, the zoom size, unrounded Pango units). Called from
/// apply_css so margins and budgets follow every zoom change; a failed
/// measurement keeps the previous value (col_px falls back before the first).
unsafe fn measure_mono() {
    let w = mono_run_px(&"0".repeat(200), false);
    if w.is_finite() && w > 0.0 {
        (*app()).mono_px = w / 200.0;
    }
}

/// Measure `text` as it renders: a PangoLayout from the view (same context
/// and font options), the pinned mono family list, and the absolute size the
/// CSS zoom size implies. Width in pixels, unrounded (`get_size` returns Pango
/// units, 1/1024 px; `get_pixel_size` would round). NaN on any failure —
/// callers treat NaN as "skip fitting", never panic.
unsafe fn mono_run_px(text: &str, bold: bool) -> f64 {
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
    if bold {
        (l().pango_font_description_set_weight)(desc, WEIGHT_BOLD); // header cells render bold
    }
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
        set_i(t, "left-margin", cols(indent + para_gutter(depth, hanging)));
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
    // a NUL cannot cross the C boundary (cstr would drop the whole text):
    // show it as U+FFFD, one char either way, so offsets stay in step
    let fixed;
    let text = if text.contains('\0') {
        fixed = text.replace('\0', "\u{FFFD}");
        fixed.as_str()
    } else {
        text
    };
    let a = &mut *app();
    a.emit_off += text.chars().count() as c_int;
    let mut it = TextIter::default();
    (l().gtk_text_buffer_get_end_iter)(a.buffer, &mut it);
    let t = cstr(text);
    if tags.is_empty() {
        (l().gtk_text_buffer_insert)(a.buffer, &mut it, t.as_ptr(), t.as_bytes().len() as c_int);
        return;
    }
    // a fixed-arity call, NULL right after the real names: GTK stops reading
    // there, so unused slots cost nothing (padding with a dummy tag made GTK
    // apply it once per slot, measurably slowing big documents)
    debug_assert!(tags.len() <= 10);
    let names: Vec<CString> = tags.iter().take(10).map(|s| cstr(s)).collect();
    let mut p = [std::ptr::null::<c_char>(); 11];
    for (slot, n) in p.iter_mut().zip(&names) {
        *slot = n.as_ptr();
    }
    (l().gtk_text_buffer_insert_with_tags_by_name)(
        a.buffer,
        &mut it,
        t.as_ptr(),
        t.as_bytes().len() as c_int,
        p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9], p[10],
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

/// Letter-spacing (Pango units, 1/1024 px) that stretches `text`, measured at
/// `measured` px, onto exactly `target` px; None when the gap is under 1 px
/// (layout rounding, not drift) or nothing could be spaced. Pango adds the
/// spacing per cluster, so zero-width chars (combining marks, joiners,
/// variation selectors) do not count as slots.
fn fit_spacing(text: &str, target: f64, measured: f64) -> Option<i64> {
    let delta = target - measured;
    let slots = text.chars().filter(|&c| char_width(c) > 0).count();
    if !measured.is_finite() || delta.abs() < 1.0 || slots == 0 {
        return None;
    }
    Some((delta * 1024.0 / slots as f64).round() as i64)
}

/// The cached letter-spacing tag name that forces `text` to occupy exactly
/// `target` px, or None when no fitting happens: empty or non-suspect text,
/// a failed measurement, a sub-pixel gap, or a tag that could not be
/// created. Callers then behave exactly as before. `bold` measures the way a
/// table header renders.
unsafe fn fit_run(text: &str, target: f64, bold: bool) -> Option<String> {
    if text.is_empty() || !suspect(text) {
        return None;
    }
    let sp = fit_spacing(text, target, mono_run_px(text, bold))?;
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
        if let Some(name) = fit_run(run, 2.0 * run.chars().count() as f64 * col_px(), false) {
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

/// Pango markup that measures `spans` the way they render: bold and italic
/// spans and inline code (pinned mono family) have their own glyph widths,
/// so wrapping the plain text would let those lines overflow. The parsed
/// text is byte-identical to `spans_plain(spans)`, so Pango's line start
/// indices map straight back onto the spans. Control chars (1 byte, which
/// GMarkup may reject) are measured as spaces of the same byte length.
fn measure_markup(spans: &[Span]) -> String {
    let mut m = String::new();
    for s in spans {
        let mut attrs = String::new();
        if s.style.contains(Style::BOLD) {
            attrs.push_str(" font_weight=\"bold\"");
        }
        if s.style.contains(Style::ITALIC) {
            attrs.push_str(" font_style=\"italic\"");
        }
        if s.role == Role::Code {
            attrs.push_str(&format!(" font_family=\"{MONO_FAMILY}\""));
        }
        if !attrs.is_empty() {
            m.push_str(&format!("<span{attrs}>"));
        }
        for c in s.text.chars() {
            match c {
                '&' => m.push_str("&amp;"),
                '<' => m.push_str("&lt;"),
                '>' => m.push_str("&gt;"),
                c if (c as u32) < 0x20 || c == '\u{7f}' => m.push(' '),
                c => m.push(c),
            }
        }
        if !attrs.is_empty() {
            m.push_str("</span>");
        }
    }
    m
}

/// Cut `spans` at Pango line start byte offsets. None when the offsets are
/// not usable (not starting at 0, not increasing, past the end, or off a
/// char boundary), so the caller falls back to one unwrapped line.
fn cut_lines(spans: &[Span], starts: &[usize]) -> Option<Vec<Vec<Span>>> {
    let text = spans_plain(spans);
    if starts.first() != Some(&0) {
        return None;
    }
    let mut frags = Vec::new();
    for (k, &a) in starts.iter().enumerate() {
        let b = starts.get(k + 1).copied().unwrap_or(text.len());
        if b < a || b > text.len() || !text.is_char_boundary(a) || !text.is_char_boundary(b) {
            return None;
        }
        if b > a || k == 0 {
            frags.push(spans_slice(spans, a, b));
        }
    }
    Some(frags)
}

/// Pre-wrap a paragraph or heading into visual lines that fit `width_px`.
/// Body text renders in the proportional UI font, so column counts are
/// meaningless: Pango wraps it with the view's own context (family, font
/// options) at the size the view will render at. The size is always set
/// explicitly from the zoom: right after a zoom the context still carries the
/// old CSS size until the next frame. Heading lines render bold at
/// `font_scale`. On any failure the paragraph comes back as one unwrapped
/// line (it then overflows under the horizontal scrollbar); never panics.
unsafe fn wrap_spans_px(spans: &[Span], width_px: i32, font_scale: f64, bold: bool) -> Vec<Vec<Span>> {
    let unwrapped = || vec![spans.to_vec()];
    let a = &mut *app();
    if a.view.is_null() || width_px <= 0 || spans.is_empty() {
        return unwrapped();
    }
    if a.wlayout.is_null() {
        a.wlayout = (l().gtk_widget_create_pango_layout)(a.view, std::ptr::null());
    }
    let layout = a.wlayout;
    if layout.is_null() {
        return unwrapped();
    }
    let ctx = (l().pango_layout_get_context)(layout);
    let base = if ctx.is_null() { std::ptr::null_mut() } else { (l().pango_context_get_font_description)(ctx) };
    let desc = if base.is_null() {
        (l().pango_font_description_from_string)(c"Sans".as_ptr())
    } else {
        (l().pango_font_description_copy)(base)
    };
    if desc.is_null() {
        return unwrapped();
    }
    (l().pango_font_description_set_absolute_size)(desc, BASE_FONT_PX * a.zoom * font_scale * 1024.0);
    if bold {
        (l().pango_font_description_set_weight)(desc, WEIGHT_BOLD);
    }
    (l().pango_layout_set_font_description)(layout, desc);
    (l().pango_font_description_free)(desc);

    let markup = measure_markup(spans);
    (l().pango_layout_set_markup)(layout, markup.as_ptr() as *const c_char, markup.len() as c_int);
    (l().pango_layout_set_width)(layout, width_px.saturating_mul(1024)); // PANGO_SCALE
    (l().pango_layout_set_wrap)(layout, PANGO_WRAP_WORD_CHAR);
    // logical start byte of every visual line (Pango >= 1.50); the layout
    // line struct itself is not ABI-stable, so no field is read directly
    let mut starts: Vec<usize> = Vec::new();
    for k in 0..(l().pango_layout_get_line_count)(layout) {
        let line = (l().pango_layout_get_line_readonly)(layout, k);
        if line.is_null() {
            return unwrapped();
        }
        starts.push((l().pango_layout_line_get_start_index)(line).max(0) as usize);
    }
    cut_lines(spans, &starts).filter(|f| !f.is_empty()).unwrap_or_else(unwrapped)
}

/// Columns left of a paragraph's text: the margin tag's indent plus the
/// fixed two-column gutter top-level list markers sit in.
fn para_gutter(depth: usize, hanging: usize) -> usize {
    if depth == 0 && hanging > 0 {
        2
    } else {
        0
    }
}

/// Pixel width available to pre-wrapped text whose line starts `left` mono
/// columns in (list indent, gutter, bullet) at this quote depth: the column
/// budget minus the quote bars, `left`, and one column of slack against
/// rounding, so prose never reaches the horizontal scrollbar. Never narrower
/// than 8 columns: at absurd widths a little overflow beats one word a line.
unsafe fn wrap_px(depth: usize, left: usize) -> i32 {
    let cols = (*app()).budget.saturating_sub(1 + depth * 2 + left).max(8);
    (cols as f64 * col_px()) as i32
}

/// Split a list item's spans into its lead (the bullet and task box: the
/// whole spans covering the first `lead` columns, exactly the ones emit_spans
/// sets in mono) and the body text that wraps.
fn split_lead(spans: &[Span], lead: usize) -> (Vec<Span>, Vec<Span>) {
    let (mut col, mut i) = (0usize, 0usize);
    while i < spans.len() && col < lead {
        col += spans[i].text.chars().count();
        i += 1;
    }
    (spans[..i].to_vec(), spans[i..].to_vec())
}

/// Emit pre-wrapped lines. The first carries the quote bars and the lead
/// (bullet); every continuation is its own buffer paragraph, so the tag's
/// hanging indent never applies to it: it gets the bars plus `hanging`
/// columns of mono padding instead, which puts its text under the first
/// line's text. That newline, bars and padding exist only because of the
/// wrap and are recorded as synthetic runs for view-state anchors.
unsafe fn emit_wrapped(frags: &[Vec<Span>], lead: &[Span], first: &[String], cont: &[String], depth: usize, hanging: usize) {
    let n = frags.len();
    for (k, frag) in frags.iter().enumerate() {
        if k == 0 {
            let mut line = lead.to_vec();
            line.extend(frag.iter().cloned());
            emit_spans(&line, first, depth, hanging);
            continue;
        }
        if k == n - 1 && spans_plain(frag).trim().is_empty() {
            continue; // an empty trailing visual line
        }
        let s = (*app()).emit_off;
        insert_text("\n", cont);
        let pad = Span { text: " ".repeat(hanging), style: Style::empty(), role: Role::None, url: None };
        emit_spans(&[pad], cont, depth, hanging);
        let e = (*app()).emit_off;
        (*app()).synth.push((s, e - s));
        emit_spans(frag, cont, 0, 0);
    }
    insert_text("\n", if n > 1 { cont } else { first });
}

/// Raster for a diagram code block, or null when the block stays text.
/// `vector` decides which blocks are pictures and which SVG they get (both
/// cached per document); the SVG is themed for the current style and
/// decoded by gdk-pixbuf's SVG loader (librsvg), crisp at the current zoom.
/// Missing SVG loader, unparsable SVG, a panic in either engine, a picture
/// shape we cannot theme: all of them fall through to plain code.
unsafe fn picture_for(lang: &str, lines: &[String]) -> Ptr {
    if !(vector::is_mermaid(lang) || vector::art_lang(lang)) {
        return std::ptr::null_mut();
    }
    // no SVG pixbuf pipeline on this machine: no pictures, no problem
    let needed: [*const (); 6] = [
        (l().gdk_pixbuf_loader_new_with_type) as *const (),
        (l().gdk_pixbuf_loader_write) as *const (),
        (l().gdk_pixbuf_loader_close) as *const (),
        (l().gdk_pixbuf_loader_get_pixbuf) as *const (),
        (l().gdk_pixbuf_get_pixels) as *const (),
        (l().gdk_pixbuf_get_rowstride) as *const (),
    ];
    if needed.iter().any(|p| p.is_null()) {
        return std::ptr::null_mut();
    }
    let src = lines.join("\n");
    let dark = (l().adw_style_manager_get_dark)((l().adw_style_manager_get_default)()) != 0;
    let key = (lang.to_string(), src.clone());
    let raw = match (*app()).svg_cache.get(&key) {
        Some(hit) => hit.clone(),
        None => {
            let built = vector::engine_svg(lang, &src).or_else(|| {
                // built-in mmdr is 0.3.x; an installed newer CLI gets a vote
                if vector::is_mermaid(lang) { vector::mermaid_via_cli(&src) } else { None }
            });
            (*app()).svg_cache.insert(key.clone(), built.clone());
            built
        }
    };
    (*app()).used_svg.insert(key.clone());
    let Some(raw) = raw else { return std::ptr::null_mut() };
    let Some(svg) = vector::apply_theme(lang, &raw, dark) else { return std::ptr::null_mut() };
    let zoom = (*app()).zoom;
    let rkey = (key.0.clone(), key.1.clone(), dark);
    (*app()).used_raster.insert(rkey.clone());
    if let Some((z, pb)) = (*app()).raster_cache.get(&rkey) {
        if (z - zoom).abs() < 0.005 {
            return (l().g_object_ref)(*pb);
        }
    }
    let pb = rasterize(&svg, zoom);
    if pb.is_null() {
        return std::ptr::null_mut();
    }
    if let Some((_, old)) = (*app()).raster_cache.insert(rkey, (zoom, pb)) {
        (l().g_object_unref)(old); // the entry owns one reference...
    }
    (l().g_object_ref)(pb) // ...the caller takes another
}

/// Decode an SVG with gdk-pixbuf's SVG loader. First pass at natural size
/// to learn the geometry and check the pixel format; when zoom wants a
/// different size, a second pass asks the loader for it up front so librsvg
/// renders the vectors at that size instead of upscaling a 1x raster. The
/// scale factor is uniform (a 6000x400 flowchart clamps both axes the same,
/// whatever the zoom), so pictures never squash. Owned reference or null.
unsafe fn rasterize(svg: &str, zoom: f64) -> Ptr {
    let base = match load_svg(svg, 0, 0) {
        Some(pb) => pb,
        None => return std::ptr::null_mut(),
    };
    // the texture below declares 8-bit RGBA; a loader handing us anything
    // else is refused rather than misread (on machines whose pixbuf lacks
    // the introspection helpers the check is skipped, and librsvg's own
    // "SVG comes out RGBA" guarantee stands in)
    let check = !((l().gdk_pixbuf_get_n_channels) as *const ()).is_null()
        && !((l().gdk_pixbuf_get_has_alpha) as *const ()).is_null()
        && !((l().gdk_pixbuf_get_bits_per_sample) as *const ()).is_null();
    let ok = !check
        || ((l().gdk_pixbuf_get_n_channels)(base) == 4
            && (l().gdk_pixbuf_get_has_alpha)(base) != 0
            && (l().gdk_pixbuf_get_bits_per_sample)(base) == 8);
    if !ok {
        (l().g_object_unref)(base);
        return std::ptr::null_mut();
    }
    let (w, h) = ((l().gdk_pixbuf_get_width)(base), (l().gdk_pixbuf_get_height)(base));
    if w <= 0 || h <= 0 {
        (l().g_object_unref)(base);
        return std::ptr::null_mut();
    }
    // one uniform factor, one ceiling on the larger axis: never a 64 MP
    // allocation even at the maximum zoom
    let mx = w.max(h) as f64;
    let scale = if mx * zoom > 4096.0 { (4096.0 / mx).max(0.02) } else { zoom };
    if (scale - 1.0).abs() < 0.005 {
        return base; // already the wanted size; its reference goes to the caller
    }
    let tw = ((w as f64 * scale).round() as c_int).max(1);
    let th = ((h as f64 * scale).round() as c_int).max(1);
    let crisp = load_svg(svg, tw, th).unwrap_or(std::ptr::null_mut());
    // a loader that ignores the requested size falls back to resampling
    let out = if crisp.is_null() {
        (l().gdk_pixbuf_scale_simple)(base, tw, th, 2 /* GDK_INTERP_BILINEAR */)
    } else {
        crisp
    };
    (l().g_object_unref)(base);
    out
}

/// free() a GError when this gdk-pixbuf/glib exports g_error_free; without
/// the symbol the error paths leak one small struct per failure, which
/// still beats losing the GUI over a picture.
unsafe fn free_err(err: Ptr) {
    if !err.is_null() && !((l().g_error_free) as *const ()).is_null() {
        (l().g_error_free)(err);
    }
}

/// One gdk-pixbuf load pass; width/height 0 means the SVG's natural size.
/// Owned reference on success, null on any failure; every GError is freed.
unsafe fn load_svg(svg: &str, w: c_int, h: c_int) -> Option<Ptr> {
    let mut err = std::ptr::null_mut();
    let loader = (l().gdk_pixbuf_loader_new_with_type)(c"svg".as_ptr(), &mut err);
    if loader.is_null() {
        free_err(err);
        return None; // this machine has no SVG pixbuf loader
    }
    if w > 0 && h > 0 && !((l().gdk_pixbuf_loader_set_size) as *const ()).is_null() {
        (l().gdk_pixbuf_loader_set_size)(loader, w, h);
    }
    let wrote = (l().gdk_pixbuf_loader_write)(loader, svg.as_ptr(), svg.len(), &mut err) != 0;
    free_err(err);
    err = std::ptr::null_mut();
    let closed = if wrote { (l().gdk_pixbuf_loader_close)(loader, &mut err) != 0 } else { false };
    free_err(err);
    if !closed {
        (l().g_object_unref)(loader);
        return None;
    }
    let pb = (l().gdk_pixbuf_loader_get_pixbuf)(loader); // borrowed from the loader
    let out = if pb.is_null() { None } else { Some((l().g_object_ref)(pb)) };
    (l().g_object_unref)(loader);
    out
}

/// Wrap a GdkPixbuf's pixels in a GdkMemoryTexture (8-bit straight-alpha
/// RGBA, which is what the SVG loader yields). The GBytes carries an extra
/// reference to the pixbuf and drops it when the last user of the bytes is
/// gone, so the pixels stay alive exactly as long as the texture reads them.
unsafe fn texture_of_pixbuf(pb: Ptr) -> Ptr {
    let (w, h) = ((l().gdk_pixbuf_get_width)(pb), (l().gdk_pixbuf_get_height)(pb));
    let stride = (l().gdk_pixbuf_get_rowstride)(pb);
    let px = (l().gdk_pixbuf_get_pixels)(pb);
    if w <= 0 || h <= 0 || stride <= 0 || px.is_null() {
        return std::ptr::null_mut();
    }
    (l().g_object_ref)(pb); // the reference owned by the byte block below
    let bytes = (l().g_bytes_new_with_free_func)(
        px,
        stride as usize * h as usize,
        Some(l().g_object_unref),
        pb,
    );
    let tex = (l().gdk_memory_texture_new)(w, h, 5 /* GDK_MEMORY_R8G8B8A8 */, bytes, stride as usize);
    (l().g_bytes_unref)(bytes);
    tex
}

/// Emit a picture block: the quote bar (its line, its tags) plus the
/// raster at a child anchor; true when the block became a picture. An
/// anchor occupies one buffer char, which is what `emit_off` and the scroll
/// anchors must count.
unsafe fn emit_picture(pb: Ptr, depth: usize) -> bool {
    let tex = texture_of_pixbuf(pb);
    if tex.is_null() {
        (l().g_object_unref)(pb);
        return false; // keep the block as code after all
    }
    let a = &mut *app();
    a.has_diagrams = true;
    let t = vec![ensure_cb(depth)];
    if depth > 0 {
        let mut bt = t.clone();
        bt.push("lead".into());
        bt.push("role-bar".into());
        insert_text(&bar_text(depth), &bt);
    }
    let mut it = TextIter::default();
    (l().gtk_text_buffer_get_end_iter)(a.buffer, &mut it);
    let anchor = (l().gtk_text_buffer_create_child_anchor)(a.buffer, &it);
    // GtkPicture + GdkMemoryTexture, not the deprecated GtkImage pixbuf API
    // (on 4.22 a pixbuf image silently stays empty), and with an explicit
    // size request: GtkTextView measures a Picture child as 0x0 on its own
    // (verified on 4.22), so the natural size must be stated
    let img = (l().gtk_picture_new)();
    (l().gtk_picture_set_paintable)(img, tex);
    (l().g_object_unref)(tex);
    (l().gtk_widget_set_size_request)(img, (l().gdk_pixbuf_get_width)(pb), (l().gdk_pixbuf_get_height)(pb));
    (l().g_object_unref)(pb); // the texture holds what it needs in its bytes
    (l().gtk_text_view_add_child_at_anchor)(a.view, img, anchor);
    // deliberately NOT unreferenced: gtk_text_buffer_create_child_anchor is
    // transfer-none - the buffer owns the only reference and frees the
    // anchor with the character (verified on 4.22: unref here finalizes
    // under the live text and the next re-render g_errors; unref after the
    // buffer dropped the character is a double free, G_IS_OBJECT criticals)
    a.emit_off += 1;
    insert_text("\n", &t);
    true
}

unsafe fn emit(lines: &[Line], depth: usize) {
    for line in lines {
        let a = &mut *app();
        a.block_starts.push(a.emit_off);
        match line {
            Line::Blank => {
                if depth > 0 {
                    insert_text(&bar_text(depth), &["lead".into(), "role-bar".into()]);
                }
                insert_text("\n", &[]);
            }
            Line::Rule => {
                // a faint mono line across the text width, like the terminal.
                // Inside a quote it gets no `rule` spacing: the bar shares the
                // line, and the gap above and below would break the bar
                let base: Vec<String> = if depth > 0 { vec![] } else { vec!["rule".into()] };
                if depth > 0 {
                    insert_text(&bar_text(depth), &["lead".into(), "role-bar".into()]);
                }
                let n = (*app()).budget.saturating_sub(1 + depth * 2).max(3);
                let mut t = base.clone();
                t.extend(["lead".to_string(), "role-frame".to_string()]);
                insert_text(&"─".repeat(n), &t);
                insert_text("\n", &base);
            }
            Line::Heading { lvl, spans } => {
                let lvl = (*lvl).clamp(1, 6);
                let mut t = vec![format!("h{lvl}")];
                let mut c = vec![format!("h{lvl}c")];
                if let Some(p) = ensure_para_tag(depth, 0, 0) {
                    t.push(p.clone());
                    c.push(p);
                }
                // pre-wrapped: the view is WRAP_NONE, so each Pango line is
                // its own buffer line; continuations skip the gap above
                let frags = wrap_spans_px(spans, wrap_px(depth, 0), HEAD_SCALES[lvl], true);
                emit_wrapped(&frags, &[], &t, &c, depth, 0);
            }
            Line::Para { spans, indent, hanging } => {
                let mut t = Vec::new();
                if let Some(p) = ensure_para_tag(depth, *indent, *hanging) {
                    t.push(p);
                }
                // only the body wraps; the bullet stays on the first line and
                // the continuations are padded to the body's column
                let (lead, body) = split_lead(spans, *hanging);
                let left = indent + para_gutter(depth, *hanging) + hanging;
                let frags = wrap_spans_px(&body, wrap_px(depth, left), 1.0, false);
                emit_wrapped(&frags, &lead, &t, &t, depth, *hanging);
            }
            Line::Code { lang, lines } => {
                let pb = picture_for(lang, lines);
                if !pb.is_null() && emit_picture(pb, depth) {
                    continue;
                }
                let t = vec![ensure_cb(depth)];
                for cl in lines {
                    if depth > 0 {
                        // the block tag rides on the bar: paragraph shading
                        // comes from the tags on a line's first char
                        let mut bt = t.clone();
                        bt.push("lead".into());
                        bt.push("role-bar".into());
                        insert_text(&bar_text(depth), &bt);
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
                                let fit = fit_run(&plain, spans_width(&spans) as f64 * col_px(), header);
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

/// A view position that survives a re-render at another width: the block it
/// lies in (blocks are emitted in the same order at any width) and the
/// offset inside that block, not counting synthetic chars (the newline, bars
/// and padding a wrap adds). Offsets are buffer chars.
fn to_anchor(p: i32, blocks: &[i32], synth: &[(i32, i32)]) -> (usize, i32) {
    if blocks.is_empty() {
        return (0, p.max(0));
    }
    let k = blocks.partition_point(|&s| s <= p).saturating_sub(1);
    let start = blocks[k];
    let mut off = p - start;
    for &(s, len) in synth.iter().filter(|r| r.0 >= start && r.0 < p) {
        off -= len.min(p - s); // inside a synthetic run: its start
    }
    (k, off.max(0))
}

/// Inverse of to_anchor against a new render's blocks and synthetic runs,
/// clamped to the block (its text may have shrunk, e.g. a narrower table).
fn from_anchor(anchor: (usize, i32), blocks: &[i32], synth: &[(i32, i32)], end: i32) -> i32 {
    let (k, off) = anchor;
    let Some(&start) = blocks.get(k) else { return end };
    let stop = blocks.get(k + 1).copied().unwrap_or(end).min(end);
    let (mut phys, mut left) = (start, off);
    for &(s, len) in synth.iter().filter(|r| r.0 >= start && r.0 < stop) {
        let real = s - phys; // real chars before this synthetic run
        if left < real {
            break;
        }
        left -= real;
        phys = s + len;
    }
    (phys + left).clamp(0, stop.max(start))
}

/// Bring `mark` to the top of the view (`top`) or just into view (find),
/// and keep at it on the next ticks until it holds. Right after the buffer is
/// rebuilt GTK only has estimated heights for lines it has not laid out, so
/// one scroll lands short (measured: paragraph 21 instead of 200 in a
/// 300-paragraph document) and the vadjustment's range is still too small.
/// The top case moves only the vertical adjustment, by the mark's distance
/// from the top of the visible rect: GTK's scroll_to_mark with use_align
/// would also align horizontally and hide the left margin of wide content.
unsafe fn scroll_settled(mark: Ptr, top: bool) {
    let a = &mut *app();
    if mark.is_null() || a.view.is_null() {
        return;
    }
    if !top {
        (l().gtk_text_view_scroll_to_mark)(a.view, mark, 0.3, 0, 0.0, 0.5);
    }
    let idle = a.rescroll.is_none();
    a.rescroll = Some((mark, top));
    a.rescroll_left = 30; // 60 ms ticks: gives up after ~1.8 s
    a.rescroll_upper = -1.0;
    if idle {
        (l().g_timeout_add)(60, Some(on_rescroll), std::ptr::null_mut());
    }
}

/// One convergence step of scroll_settled; true once the mark holds.
unsafe fn rescroll_step(mark: Ptr, top: bool) -> bool {
    let a = &mut *app();
    let mut it = TextIter::default();
    (l().gtk_text_buffer_get_iter_at_mark)(a.buffer, &mut it, mark);
    let (mut loc, mut vis) = ([0 as c_int; 4], [0 as c_int; 4]); // GdkRectangle x, y, w, h
    (l().gtk_text_view_get_iter_location)(a.view, &it, loc.as_mut_ptr());
    (l().gtk_text_view_get_visible_rect)(a.view, vis.as_mut_ptr());
    if !top {
        if loc[1] >= vis[1] && loc[1] + loc[3] <= vis[1] + vis[3] {
            return true;
        }
        (l().gtk_text_view_scroll_to_mark)(a.view, mark, 0.3, 0, 0.0, 0.5);
        return false;
    }
    let delta = (loc[1] - vis[1]) as f64;
    if delta.abs() <= 1.0 {
        return true;
    }
    let adj = (l().gtk_scrolled_window_get_vadjustment)(a.scrolled);
    if adj.is_null() {
        return true;
    }
    let (value, upper) = ((l().gtk_adjustment_get_value)(adj), (l().gtk_adjustment_get_upper)(adj));
    let max = (upper - (l().gtk_adjustment_get_page_size)(adj)).max(0.0);
    if delta > 0.0 && value >= max - 0.5 && upper == a.rescroll_upper {
        return true; // near the end: the mark cannot reach the top
    }
    a.rescroll_upper = upper;
    (l().gtk_adjustment_set_value)(adj, (value + delta).clamp(0.0, max));
    false
}

unsafe extern "C" fn on_rescroll(_data: Ptr) -> c_int {
    unsafe {
        let a = &mut *app();
        let Some((mark, top)) = a.rescroll else { return 0 };
        let done = a.win.is_null() || a.rescroll_left == 0 || rescroll_step(mark, top);
        let a = &mut *app();
        if done {
            a.rescroll = None;
            a.pending_top = None;
            return 0;
        }
        a.rescroll_left -= 1;
        1
    }
}

/// Parse `src` with a `cols`-column budget and render it into the buffer.
/// With `keep` (resize, zoom, reload of the same document) the cursor and the
/// top visible line come back as anchors, so they land on the same text
/// however it re-wrapped, and an open find bar re-highlights its match.
/// Without (another document) the view starts at the top.
unsafe fn render_source(src: &str, cols: usize, keep: bool) {
    let cfg = Cfg::new(false, cols, true);
    let parsed = parse(src, &cfg);
    let a = &mut *app();
    let mut saved: Option<((usize, i32), (usize, i32))> = None; // (cursor, top line)
    if keep && !a.buffer.is_null() && !a.view.is_null() && !a.block_starts.is_empty() {
        let mut it = TextIter::default();
        (l().gtk_text_buffer_get_iter_at_mark)(a.buffer, &mut it, (l().gtk_text_buffer_get_insert)(a.buffer));
        let cur = (l().gtk_text_iter_get_offset)(&it);
        let mut rect = [0 as c_int; 4]; // GdkRectangle: x, y, width, height
        (l().gtk_text_view_get_visible_rect)(a.view, rect.as_mut_ptr());
        let mut top = 0;
        if rect[3] > 0 {
            let mut line_top = 0 as c_int;
            (l().gtk_text_view_get_line_at_y)(a.view, &mut it, rect[1], &mut line_top);
            top = (l().gtk_text_iter_get_offset)(&it);
        }
        // a restore still converging: the view is not there yet, so its
        // visible rect would save the wrong line; keep the anchor it aims at
        let top_anchor = match (a.rescroll, a.pending_top) {
            (Some((_, true)), Some(t)) => t,
            _ => to_anchor(top, &a.block_starts, &a.synth),
        };
        saved = Some((to_anchor(cur, &a.block_starts, &a.synth), top_anchor));
    }

    (l().gtk_text_buffer_set_text)(a.buffer, c"".as_ptr(), 0);
    a.emit_off = 0;
    a.block_starts.clear();
    a.synth.clear();
    a.has_diagrams = false;
    a.used_svg.clear();
    a.used_raster.clear();
    if !keep {
        a.svg_cache.clear(); // another document's pictures are not ours
        for (_, (_, pb)) in a.raster_cache.drain() {
            (l().g_object_unref)(pb);
        }
    }
    emit(&parsed, 0);
    // caches shrink to what this emit used: live reload re-renders with
    // keep = true, so without this every saved revision of every diagram
    // would sit in the caches (and hold its pixbuf) until the app closes
    let used = std::mem::take(&mut a.used_svg);
    a.svg_cache.retain(|k, _| used.contains(k));
    a.used_svg = used;
    let used = std::mem::take(&mut a.used_raster);
    let old = std::mem::take(&mut a.raster_cache);
    let mut fresh = HashMap::new();
    for (k, e) in old {
        if used.contains(&k) {
            fresh.insert(k, e);
        } else {
            (l().g_object_unref)(e.1);
        }
    }
    a.raster_cache = fresh;
    a.used_raster = used;

    let a = &mut *app();
    let mut it = TextIter::default();
    (l().gtk_text_buffer_get_end_iter)(a.buffer, &mut it);
    let end = (l().gtk_text_iter_get_offset)(&it);
    let (cur, top) = match saved {
        Some((c, t)) => (
            from_anchor(c, &a.block_starts, &a.synth, end),
            from_anchor(t, &a.block_starts, &a.synth, end),
        ),
        None => (0, 0),
    };
    a.pending_top = Some(saved.map(|s| s.1).unwrap_or((0, 0)));
    if !keep && !a.scrolled.is_null() {
        let h = (l().gtk_scrolled_window_get_hadjustment)(a.scrolled);
        if !h.is_null() {
            (l().gtk_adjustment_set_value)(h, 0.0); // another document: back to the left edge
        }
    }
    (l().gtk_text_buffer_get_iter_at_offset)(a.buffer, &mut it, cur);
    (l().gtk_text_buffer_place_cursor)(a.buffer, &it);
    (l().gtk_text_buffer_get_iter_at_offset)(a.buffer, &mut it, top);
    if a.top_mark.is_null() {
        a.top_mark = (l().gtk_text_buffer_create_mark)(a.buffer, c"mdglass-top".as_ptr(), &it, 1);
    } else {
        (l().gtk_text_buffer_move_mark)(a.buffer, a.top_mark, &it);
    }
    scroll_settled(a.top_mark, true);
    if a.find_shown {
        // same document: re-highlight and stay on the same match; another
        // document: its matches, from the top
        do_search(!keep, false, keep);
    }
}

/// Width of the text area in monospace columns: what one table line may use.
unsafe fn view_cols() -> usize {
    let a = &*app();
    let mut w = (l().gtk_widget_get_width)(a.view);
    if w <= 1 {
        // not allocated yet: fall back to the window's configured default width
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
    render_source(&src, cols, true);
}

/// Read and render the current document; `keep` preserves the view state
/// (a reload of the same file), otherwise it starts at the top.
unsafe fn load_current(keep: bool) {
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
    render_source(&src, cols, keep);

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

/// Highlight every match and move to one: the first (`from_top`), the next
/// or previous one from the cursor, or with `keep` the match the cursor is
/// on (after a re-render the cursor comes back to the same match's end).
unsafe fn do_search(from_top: bool, backward: bool, keep: bool) {
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
            } else if keep {
                pick = hits.iter().position(|h| (l().gtk_text_iter_compare)(&h.1, &cursor) >= 0);
            } else {
                pick = hits.iter().position(|h| (l().gtk_text_iter_compare)(&h.0, &cursor) >= 0);
            }
            if pick.is_none() {
                pick = Some(if backward { hits.len() - 1 } else { 0 });
            }
        }
        let k = pick.unwrap_or(0);
        (l().gtk_text_buffer_place_cursor)(a.buffer, &hits[k].1);
        // via the insert mark and scroll_settled: right after a re-render the
        // new lines only have estimated heights
        scroll_settled((l().gtk_text_buffer_get_insert)(a.buffer), false);
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
    load_current(false);
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
    load_current(false);
}

unsafe extern "C" fn act_find(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { show_findbar() }
}

unsafe extern "C" fn act_reload(_: Ptr, _: Ptr, _: Ptr) {
    unsafe { load_current(true) }
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
    unsafe { do_search(false, false, false) }
}

unsafe extern "C" fn click_find_prev(_: Ptr, _: Ptr) {
    unsafe { do_search(false, true, false) }
}

unsafe extern "C" fn click_find_close(_: Ptr, _: Ptr) {
    unsafe { hide_findbar() }
}

unsafe extern "C" fn entry_changed(_: Ptr, _: Ptr) {
    unsafe { do_search(true, false, false) }
}

unsafe extern "C" fn entry_activate(_: Ptr, _: Ptr) {
    unsafe { do_search(false, false, false) }
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

// A resize is picked up by polling: GtkWidget's notify::width never fired
// here (not even for the first allocation), and one width read per tick is
// as cheap as the mtime poll. A new budget must hold for two ticks in a row
// before the (full) re-render, so a drag-resize costs one render, not one
// per tick.
unsafe extern "C" fn on_relayout_tick(_data: Ptr) -> c_int {
    unsafe {
        let a = &mut *app();
        if a.status_shown || a.win.is_null() || a.src_cache.is_none() {
            return 1;
        }
        let c = view_cols();
        if c == a.budget {
            a.seen_count = 0;
            return 1;
        }
        if c == a.seen_cols {
            a.seen_count += 1;
        } else {
            (a.seen_cols, a.seen_count) = (c, 1);
        }
        if a.seen_count >= 2 {
            a.seen_count = 0;
            relayout_cached();
        }
    }
    1
}

static mut FIRST_LOAD_TRIES: u32 = 0;

// First document load, deferred until the view is allocated (~1.5 s at most,
// then the default-size estimate is used).
unsafe extern "C" fn on_first_load(_data: Ptr) -> c_int {
    unsafe {
        let a = &*app();
        if a.win.is_null() {
            return 0;
        }
        FIRST_LOAD_TRIES += 1;
        if (l().gtk_widget_get_width)(a.view) <= 1 && FIRST_LOAD_TRIES < 50 {
            return 1;
        }
        load_current(false);
    }
    0
}

unsafe extern "C" fn on_tick(_data: Ptr) -> c_int {
    unsafe {
        let a = &*app();
        if !a.win.is_null() && a.content.is_none() {
            if let Some(p) = current_path() {
                if let Ok(t) = std::fs::metadata(&p).map(|m| m.modified()) {
                    if let Ok(t) = t {
                        if a.mtime.is_some() && a.mtime != Some(t) {
                            load_current(true);
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
        // render once the view has its real width: rendering now, at the
        // default size, would mean a second full render a tick later
        (l().g_timeout_add)(30, Some(on_first_load), std::ptr::null_mut());
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
        svg_cache: HashMap::new(),
        raster_cache: HashMap::new(),
        used_svg: HashSet::new(),
        used_raster: HashSet::new(),
        has_diagrams: false,
        emit_off: 0,
        block_starts: Vec::new(),
        synth: Vec::new(),
        top_mark: std::ptr::null_mut(),
        wlayout: std::ptr::null_mut(),
        rescroll: None,
        rescroll_left: 0,
        rescroll_upper: -1.0,
        pending_top: None,
        seen_cols: 0,
        seen_count: 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(text: &str, style: Style, role: Role) -> Span {
        Span { text: text.to_string(), style, role, url: None }
    }

    /// markup text with the tags removed and entities decoded, as Pango parses it
    fn markup_text(m: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in m.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
    }

    #[test]
    fn anchors_survive_a_rewrap() {
        // render A: block 0 = "aaaa bbbb" wrapped once (synthetic "\n" + 2 pad
        // chars at offset 5), block 1 = "cc\n"
        // A text: "aaaa " "\n  " "bbbb" "\n" | "cc\n"
        let (blocks_a, synth_a) = (vec![0, 13], vec![(5, 3)]);
        // render B (wider): no wrap, "aaaa bbbb\n" | "cc\n"
        let (blocks_b, synth_b) = (vec![0, 10], vec![]);
        // the 'b' at A offset 9 (logical 6 in block 0) is at B offset 6
        let an = to_anchor(9, &blocks_a, &synth_a);
        assert_eq!(an, (0, 6));
        assert_eq!(from_anchor(an, &blocks_b, &synth_b, 13), 6);
        // and back to A
        assert_eq!(from_anchor(to_anchor(6, &blocks_b, &synth_b), &blocks_a, &synth_a, 16), 9);
        // a position inside a synthetic run maps to its start
        assert_eq!(to_anchor(6, &blocks_a, &synth_a), (0, 5));
        // the second block keeps its offset whatever happened before it
        assert_eq!(from_anchor(to_anchor(14, &blocks_a, &synth_a), &blocks_b, &synth_b, 13), 11);
    }

    #[test]
    fn anchors_clamp_to_shrunken_blocks_and_the_end() {
        assert_eq!(from_anchor((0, 50), &[0, 10], &[], 20), 10);
        assert_eq!(from_anchor((5, 0), &[0, 10], &[], 20), 20);
        assert_eq!(to_anchor(7, &[], &[]), (0, 7));
    }

    #[test]
    fn fit_spacing_math() {
        // 4 wide glyphs 8 px short: 2 px = 2048 units per glyph
        assert_eq!(fit_spacing("漢字漢字", 40.0, 32.0), Some(2048));
        // too wide: negative spacing
        assert_eq!(fit_spacing("漢字", 20.0, 24.0), Some(-2048));
        // sub-pixel gaps and failed measurements are left alone
        assert_eq!(fit_spacing("漢字", 20.0, 19.5), None);
        assert_eq!(fit_spacing("漢字", 20.0, f64::NAN), None);
        // zero-width chars are not spacing slots
        assert_eq!(fit_spacing("漢\u{301}", 20.0, 18.0), Some(2048));
        assert_eq!(fit_spacing("\u{301}", 20.0, 10.0), None);
    }

    #[test]
    fn suspect_runs() {
        assert!(!suspect("plain ASCII │ ─ ┼ кириллица"));
        assert!(suspect("漢"));
        assert!(suspect("✅"));
        assert!(suspect("a → b")); // U+2192: not DejaVu-exact by assumption
    }

    #[test]
    fn markup_parses_back_to_the_plain_text() {
        let spans = vec![
            sp("a < b & c > d ", Style::empty(), Role::None),
            sp("bold", Style::BOLD, Role::None),
            sp(" x\u{7}y ", Style::ITALIC, Role::None),
            sp("code", Style::empty(), Role::Code),
        ];
        let m = measure_markup(&spans);
        assert!(m.contains("font_weight=\"bold\"") && m.contains("font_style=\"italic\""));
        assert!(m.contains(MONO_FAMILY));
        let plain = spans_plain(&spans).replace('\u{7}', " ");
        assert_eq!(markup_text(&m), plain);
        assert_eq!(markup_text(&m).len(), spans_plain(&spans).len()); // same byte offsets
    }

    #[test]
    fn cut_lines_maps_starts_onto_spans() {
        let spans = vec![sp("hello ", Style::BOLD, Role::None), sp("wide world", Style::empty(), Role::None)];
        let f = cut_lines(&spans, &[0, 11]).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(spans_plain(&f[0]), "hello wide ");
        assert_eq!(spans_plain(&f[1]), "world");
        assert_eq!(f[0][0].style, Style::BOLD);
        // unusable offsets: not from 0, decreasing, past the end, mid-char
        assert!(cut_lines(&spans, &[3]).is_none());
        assert!(cut_lines(&spans, &[0, 8, 4]).is_none());
        assert!(cut_lines(&spans, &[0, 99]).is_none());
        assert!(cut_lines(&[sp("漢字", Style::empty(), Role::None)], &[0, 1]).is_none());
        // a repeated start yields no empty fragment
        assert_eq!(cut_lines(&spans, &[0, 11, 11]).unwrap().len(), 2);
    }

    #[test]
    fn split_lead_takes_whole_bullet_spans() {
        let spans = vec![
            sp("•", Style::empty(), Role::Bullet),
            sp(" ", Style::empty(), Role::None),
            sp("☐ ", Style::empty(), Role::Todo),
            sp("text", Style::empty(), Role::None),
        ];
        let (lead, body) = split_lead(&spans, 4);
        assert_eq!(spans_plain(&lead), "• ☐ ");
        assert_eq!(spans_plain(&body), "text");
        let (lead, body) = split_lead(&spans, 0);
        assert!(lead.is_empty() && body.len() == 4);
    }
}
