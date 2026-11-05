// Native GNOME back end: GTK4 + libadwaita, no WebKit, no build-time C
// dependency. The system's GNOME libraries are loaded at startup with dlopen
// (only runtime `.so` files are required, never the -dev packages), so the
// binary stays a single small file. Documents render into a styled GtkTextView
// with buffer tags: an AdwToolbarView + header bar, an open dialog, in-buffer
// find, zoom, next/prev file, and live reload when the file changes.

use std::sync::OnceLock;
use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::path::Path;
use std::time::SystemTime;

use crate::doc::{parse, table_text, Cfg, Line, Role, Span, Style};

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
const WRAP_WORD_CHAR: c_int = 3;
const POLICY_NEVER: c_int = 2;
const POLICY_AUTOMATIC: c_int = 1;
const PHASE_CAPTURED: c_int = 1;
const NON_UNIQUE: c_uint = 16; // G_APPLICATION_NON_UNIQUE
const PROVIDER_PRIORITY: c_uint = 600; // GTK_STYLE_PROVIDER_PRIORITY_APPLICATION
const WEIGHT_BOLD: c_int = 700;
const STYLE_ITALIC: c_int = 2;
const UNDERLINE_SINGLE: c_int = 1;
const SEARCH_FLAGS: c_uint = 1 | 2; // TEXT_ONLY | CASE_INSENSITIVE
const KEY_ESCAPE: c_uint = 0xff1b;
const COL_PX: c_int = 8; // approx width of one terminal column, for indents

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
    gtk_init_check: unsafe extern "C" fn(Ptr, *mut c_int, *mut *mut Ptr) -> c_int,
    gtk_window_set_title: unsafe extern "C" fn(Ptr, *const c_char),
    gtk_window_set_default_size: unsafe extern "C" fn(Ptr, c_int, c_int),
    gtk_window_close: unsafe extern "C" fn(Ptr),
    gtk_window_set_child: unsafe extern "C" fn(Ptr, Ptr),
    gtk_window_present: unsafe extern "C" fn(Ptr),
    gtk_header_bar_set_title_widget: unsafe extern "C" fn(Ptr, Ptr),
    gtk_header_bar_pack_end: unsafe extern "C" fn(Ptr, Ptr),
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
        unsafe extern "C" fn(*const TextIter, *const c_char, c_uint, *mut TextIter, *mut TextIter) -> c_int,
    gtk_text_iter_compare: unsafe extern "C" fn(*const TextIter, *const TextIter) -> c_int,
    gtk_box_new: unsafe extern "C" fn(c_int, c_int) -> Ptr,
    gtk_box_append: unsafe extern "C" fn(Ptr, Ptr),
    gtk_button_new_from_icon_name: unsafe extern "C" fn(*const c_char) -> Ptr,
    gtk_button_new_with_label: unsafe extern "C" fn(*const c_char) -> Ptr,
    gtk_menu_button_new: unsafe extern "C" fn() -> Ptr,
    gtk_label_new: unsafe extern "C" fn(*const c_char) -> Ptr,
    gtk_label_set_text: unsafe extern "C" fn(Ptr, *const c_char),
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
    g_object_set: unsafe extern "C" fn(Ptr, *const c_char, ...),
    g_object_unref: unsafe extern "C" fn(Ptr),
    g_signal_connect_data: unsafe extern "C" fn(Ptr, *const c_char, Ptr, Ptr, Ptr, c_ulong) -> c_ulong,
    g_menu_new: unsafe extern "C" fn() -> Ptr,
    g_menu_append: unsafe extern "C" fn(Ptr, *const c_char, *const c_char, ...) -> c_uint,
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
        }
        let lib = Lib {
            gtk_init_check: gs!("gtk_init_check", unsafe extern "C" fn(Ptr, *mut c_int, *mut *mut Ptr) -> c_int),
            gtk_window_set_title: gs!("gtk_window_set_title", unsafe extern "C" fn(Ptr, *const c_char)),
            gtk_window_set_default_size: gs!("gtk_window_set_default_size", unsafe extern "C" fn(Ptr, c_int, c_int)),
            gtk_window_close: gs!("gtk_window_close", unsafe extern "C" fn(Ptr)),
            gtk_window_set_child: gs!("gtk_window_set_child", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_window_present: gs!("gtk_window_present", unsafe extern "C" fn(Ptr)),
            gtk_header_bar_set_title_widget: gs!("gtk_header_bar_set_title_widget", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_header_bar_pack_end: gs!("gtk_header_bar_pack_end", unsafe extern "C" fn(Ptr, Ptr)),
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
            gtk_text_buffer_insert: gs!("gtk_text_buffer_insert", unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int)),
            gtk_text_buffer_insert_with_tags_by_name: gs!("gtk_text_buffer_insert_with_tags_by_name", unsafe extern "C" fn(Ptr, *mut TextIter, *const c_char, c_int, *const c_char, ...)),
            gtk_text_buffer_place_cursor: gs!("gtk_text_buffer_place_cursor", unsafe extern "C" fn(Ptr, *const TextIter)),
            gtk_text_buffer_apply_tag_by_name: gs!("gtk_text_buffer_apply_tag_by_name", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter)),
            gtk_text_buffer_remove_tag_by_name: gs!("gtk_text_buffer_remove_tag_by_name", unsafe extern "C" fn(Ptr, *const c_char, *const TextIter, *const TextIter)),
            gtk_text_buffer_get_bounds: gs!("gtk_text_buffer_get_bounds", unsafe extern "C" fn(Ptr, *mut TextIter, *mut TextIter)),
            gtk_text_iter_forward_search: gs!("gtk_text_iter_forward_search", unsafe extern "C" fn(*const TextIter, *const c_char, c_uint, *mut TextIter, *mut TextIter) -> c_int),
            gtk_text_iter_compare: gs!("gtk_text_iter_compare", unsafe extern "C" fn(*const TextIter, *const TextIter) -> c_int),
            gtk_box_new: gs!("gtk_box_new", unsafe extern "C" fn(c_int, c_int) -> Ptr),
            gtk_box_append: gs!("gtk_box_append", unsafe extern "C" fn(Ptr, Ptr)),
            gtk_button_new_from_icon_name: gs!("gtk_button_new_from_icon_name", unsafe extern "C" fn(*const c_char) -> Ptr),
            gtk_button_new_with_label: gs!("gtk_button_new_with_label", unsafe extern "C" fn(*const c_char) -> Ptr),
            gtk_menu_button_new: gs!("gtk_menu_button_new", unsafe extern "C" fn() -> Ptr),
            gtk_label_new: gs!("gtk_label_new", unsafe extern "C" fn(*const c_char) -> Ptr),
            gtk_label_set_text: gs!("gtk_label_set_text", unsafe extern "C" fn(Ptr, *const c_char)),
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
            g_object_set: gs!(go "g_object_set", unsafe extern "C" fn(Ptr, *const c_char, ...)),
            g_object_unref: gs!(go "g_object_unref", unsafe extern "C" fn(Ptr)),
            g_signal_connect_data: gs!(go "g_signal_connect_data", unsafe extern "C" fn(Ptr, *const c_char, Ptr, Ptr, Ptr, c_ulong) -> c_ulong),
            g_menu_new: gs!(gio "g_menu_new", unsafe extern "C" fn() -> Ptr),
            g_menu_append: gs!(gio "g_menu_append", unsafe extern "C" fn(Ptr, *const c_char, *const c_char, ...) -> c_uint),
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
    files: Vec<String>,
    idx: usize,
    content: Option<String>, // set when the document came from stdin
    mtime: Option<SystemTime>,
    ptags: HashSet<String>,
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

unsafe fn make_tags() {
    let base = tag("base"); // sentinel so varargs always have one name
    let _ = base;
    let scales: [f64; 7] = [1.0, 1.5, 1.3, 1.15, 1.05, 1.0, 1.0];
    let above: [c_int; 7] = [0, 14, 12, 10, 8, 8, 8];
    for lvl in 1..=6 {
        let t = tag(&format!("h{lvl}"));
        set_i(t, "weight", WEIGHT_BOLD);
        set_f(t, "scale", scales[lvl]);
        set_i(t, "pixels-above-lines", above[lvl]);
        set_i(t, "pixels-below-lines", 2);
    }
    set_i(tag("bold"), "weight", WEIGHT_BOLD);
    set_i(tag("italic"), "style", STYLE_ITALIC);
    set_i(tag("strike"), "strikethrough", 1);
    set_i(tag("under"), "underline", UNDERLINE_SINGLE);

    let t = tag("role-code");
    set_s(t, "family", "Monospace");
    set_s(t, "background", "rgba(127,127,127,0.18)");
    set_i(t, "background-set", 1);
    set_f(t, "scale", 0.92);

    let t = tag("role-link");
    set_s(t, "foreground", "#3584e4");
    set_i(t, "foreground-set", 1);
    set_i(t, "underline", UNDERLINE_SINGLE);

    for (name, color) in
        [("role-faint", "#77767b"), ("role-bullet", "#ff7800"), ("role-image", "#9141ac"),
         ("role-done", "#2ec27e"), ("role-bar", "#1c71d8")]
    {
        let t = tag(name);
        set_s(t, "foreground", color);
        set_i(t, "foreground-set", 1);
    }

    let t = tag("mono");
    set_s(t, "family", "Monospace");
    set_f(t, "scale", 0.92);

    let t = tag("rule");
    set_s(t, "paragraph-background", "rgba(127,127,127,0.28)");
    set_i(t, "paragraph-background-set", 1);
    set_i(t, "pixels-above-lines", 8);
    set_i(t, "pixels-below-lines", 8);

    let t = tag("hit");
    set_s(t, "background", "rgba(246,201,83,0.45)");
    set_i(t, "background-set", 1);
}

/// Code-block tags per blockquote depth (margins include the quote bars).
unsafe fn ensure_cb(depth: usize) -> String {
    let name = format!("cb{depth}");
    let fresh = unsafe { (*app()).ptags.insert(name.clone()) };
    if fresh {
        let t = tag(&name);
        set_s(t, "family", "Monospace");
        set_s(t, "background", "rgba(127,127,127,0.11)");
        set_i(t, "background-set", 1);
        set_i(t, "left-margin", 18 + depth as c_int * 2 * COL_PX);
        set_i(t, "right-margin", 18);
        set_f(t, "scale", 0.92);
    }
    name
}

/// Paragraph margins per (quote depth, indent, hanging).
unsafe fn ensure_para_tag(depth: usize, indent: usize, hanging: usize) -> Option<String> {
    if depth == 0 && indent == 0 && hanging == 0 {
        return None;
    }
    let name = format!("p{depth}-{indent}-{hanging}");
    let fresh = unsafe { (*app()).ptags.insert(name.clone()) };
    if fresh {
        let t = tag(&name);
        set_i(t, "left-margin", (depth as c_int * 2 + indent as c_int + hanging as c_int) * COL_PX);
        if hanging > 0 {
            set_i(t, "indent", -(hanging as c_int * COL_PX)); // first line hangs back
        }
    }
    Some(name)
}

/// Paragraph margin (no hanging) for tables and rules inside quotes.
unsafe fn ensure_margin_tag(depth: usize) -> Option<String> {
    if depth == 0 {
        return None;
    }
    Some(unsafe { ensure_para_tag(depth, 0, 0) }.unwrap())
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
    let mut names: Vec<CString> = tags.iter().map(|s| cstr(s)).collect();
    while names.len() < 6 {
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

unsafe fn emit_spans(spans: &[Span], base: &[String], depth: usize) {
    if depth > 0 {
        let bar = format!("{} ", "┃".repeat(depth));
        let mut bt = base.to_vec();
        bt.push("role-bar".into());
        insert_text(&bar, &bt);
    }
    for s in spans {
        let mut t = base.to_vec();
        t.extend(style_tag_names(s));
        insert_text(&s.text, &t);
    }
}

unsafe fn emit(lines: &[Line], depth: usize) {
    for line in lines {
        match line {
            Line::Blank => insert_text("\n", &[]),
            Line::Rule => {
                let mut t = vec!["rule".to_string()];
                if let Some(m) = ensure_margin_tag(depth) {
                    t.push(m);
                }
                insert_text("\n", &t);
            }
            Line::Heading { lvl, spans } => {
                let mut t = vec![format!("h{}", (*lvl).clamp(1, 6))];
                if let Some(p) = ensure_para_tag(depth, 0, 0) {
                    t.push(p);
                }
                emit_spans(spans, &t, depth);
                insert_text("\n", &t);
            }
            Line::Para { spans, indent, hanging } => {
                let mut t = Vec::new();
                if let Some(p) = ensure_para_tag(depth, *indent, *hanging) {
                    t.push(p);
                }
                emit_spans(spans, &t, depth);
                insert_text("\n", &t);
            }
            Line::Code { lines, .. } => {
                let t = vec![ensure_cb(depth)];
                for cl in lines {
                    insert_text(cl, &t);
                    insert_text("\n", &t);
                }
            }
            Line::Table(t) => {
                let mut tags = vec!["mono".to_string()];
                if let Some(m) = ensure_margin_tag(depth) {
                    tags.push(m);
                }
                for row in table_text(t) {
                    insert_text(&row, &tags);
                    insert_text("\n", &tags);
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

unsafe fn load_current() {
    let src = match &(*app()).content {
        Some(c) => c.clone(),
        None => match current_path() {
            Some(p) => match std::fs::read(&p) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => format!("mdv: cannot read {p}: {e}"),
            },
            None => String::new(),
        },
    };
    let cfg = Cfg::new(false, 120, true);
    let parsed = parse(&src, &cfg);
    let a = &mut *app();
    (l().gtk_text_buffer_set_text)(a.buffer, c"".as_ptr(), 0);
    emit(&parsed, 0);

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
    let t = cstr(&format!("mdv - {name}"));
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
            (l().gtk_text_iter_forward_search)(&it, needle_c.as_ptr(), SEARCH_FLAGS, &mut s, &mut e);
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
    let zoom = (*app()).zoom;
    let css = format!(
        ".mdvview {{ font-size: {:.1}px; }}\n.mdvfind {{ border-top: 1px solid alpha(currentColor, 0.12); }}\n",
        15.0 * zoom
    );
    let c = cstr(&css);
    (l().gtk_css_provider_load_from_string)((*app()).css, c.as_ptr());
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
        apply_css();
    }
}

unsafe extern "C" fn act_zoom_out(_: Ptr, _: Ptr, _: Ptr) {
    unsafe {
        (*app()).zoom = ((*app()).zoom / 1.15).max(0.5);
        apply_css();
    }
}

unsafe extern "C" fn act_zoom_reset(_: Ptr, _: Ptr, _: Ptr) {
    unsafe {
        (*app()).zoom = 1.0;
        apply_css();
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

// key handler: (controller, keyval, keycode, state, user_data); the gboolean
// return of this signal is ignored (the closure has no return type), so the
// event also does its default — which for Escape is harmless.
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

    let toolbar = (l().adw_toolbar_view_new)();
    let header = (l().adw_header_bar_new)();
    a.wtitle = (l().adw_window_title_new)(std::ptr::null(), c"mdv".as_ptr());
    (l().gtk_header_bar_set_title_widget)(header, a.wtitle);
    let menu_btn = (l().gtk_menu_button_new)();
    set_s(menu_btn, "icon-name", "open-menu-symbolic");
    set_b(menu_btn, "primary", true);
    build_menu(menu_btn);
    (l().gtk_header_bar_pack_end)(header, menu_btn);
    (l().adw_toolbar_view_add_top_bar)(toolbar, header);

    let vbox = (l().gtk_box_new)(ORIENT_VERTICAL, 0);
    a.scrolled = (l().gtk_scrolled_window_new)();
    (l().gtk_widget_set_hexpand)(a.scrolled, 1);
    (l().gtk_widget_set_vexpand)(a.scrolled, 1);
    (l().gtk_scrolled_window_set_policy)(a.scrolled, POLICY_NEVER, POLICY_AUTOMATIC);
    a.view = (l().gtk_text_view_new)();
    (l().gtk_text_view_set_wrap_mode)(a.view, WRAP_WORD_CHAR);
    (l().gtk_text_view_set_editable)(a.view, 0);
    (l().gtk_text_view_set_cursor_visible)(a.view, 1);
    (l().gtk_text_view_set_left_margin)(a.view, 28);
    (l().gtk_text_view_set_right_margin)(a.view, 28);
    (l().gtk_text_view_set_top_margin)(a.view, 12);
    (l().gtk_text_view_set_bottom_margin)(a.view, 36);
    (l().gtk_widget_add_css_class)(a.view, c"mdvview".as_ptr());
    a.buffer = (l().gtk_text_view_get_buffer)(a.view);
    make_tags();
    (l().gtk_scrolled_window_set_child)(a.scrolled, a.view);

    a.status = build_status_page();
    a.status_shown = true;

    a.findbar = build_findbar();
    (l().gtk_box_append)(vbox, a.scrolled);
    (l().gtk_box_append)(vbox, a.findbar);
    (l().adw_toolbar_view_set_content)(toolbar, vbox);
    (l().gtk_window_set_child)(a.win, toolbar);

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
}

// ------------------------------------------------------------- widgets ---

unsafe fn build_status_page() -> Ptr {
    let page = (l().adw_status_page_new)();
    (l().adw_status_page_set_title)(page, c"No document".as_ptr());
    (l().adw_status_page_set_description)(page, c"Open a Markdown file (Ctrl+O), or run mdv --gui FILE".as_ptr());
    (l().gtk_widget_set_hexpand)(page, 1);
    (l().gtk_widget_set_vexpand)(page, 1);
    let btn = (l().gtk_button_new_with_label)(c"Open…".as_ptr());
    set_s(btn, "action-name", "win.open");
    (l().adw_status_page_set_child)(page, btn);
    page
}

unsafe fn build_findbar() -> Ptr {
    let bar = (l().gtk_box_new)(0 /* horizontal */, 6);
    (l().gtk_widget_add_css_class)(bar, c"mdvfind".as_ptr());
    (l().gtk_widget_set_margin_start)(bar, 20);
    (l().gtk_widget_set_margin_end)(bar, 20);
    (l().gtk_widget_set_margin_top)(bar, 8);
    (l().gtk_widget_set_margin_bottom)(bar, 8);
    (l().gtk_widget_set_visible)(bar, 0);

    build_findbar_entry(&bar);

    let label = (l().gtk_label_new)(c" ".as_ptr());
    (l().gtk_widget_set_margin_end)(label, 8);
    set_hits_label(label);
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
    connect(ctrl, "key-press", cbptr::<unsafe extern "C" fn(Ptr, c_uint, c_uint, c_uint, Ptr) -> c_int>(key_press));
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
        (l().g_menu_append)(menu, a.as_ptr(), lb.as_ptr());
    };
    add("win.open", "_Open…");
    add("win.find", "_Find…");
    add("win.reload", "_Reload");
    let section = (l().g_menu_new)();
    let sec_add = | menu: Ptr, action: &str, label: &str | {
        let (a, lb) = (cstr(action), cstr(label));
        (l().g_menu_append)(menu, a.as_ptr(), lb.as_ptr());
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

// ----------------------------------------------------------------- run ---

/// Open the GNOME preview window. Blocks until the window is closed.
pub fn run(files: Vec<String>, content: Option<String>) -> Result<(), String> {
    let lib = load_lib()?;
    if LIB.set(lib).is_err() {
        return Err("gui::run called twice".into());
    }
    if unsafe { (l().gtk_init_check)(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) } == 0
    {
        return Err("no GNOME display available (need a Wayland or X11 session)".into());
    }
    let app = unsafe { (l().adw_application_new)(c"org.mdv.Viewer".as_ptr(), NON_UNIQUE) };
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
        files,
        idx: 0,
        content,
        mtime: None,
        ptags: HashSet::new(),
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
