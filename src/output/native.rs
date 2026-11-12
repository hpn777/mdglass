//! Headless, dynamically loaded Cairo/Pango backend. No GTK initialization is
//! involved. Library handles deliberately remain open for the process lifetime.
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::sync::OnceLock;

use crate::doc::{Role, Span, Style};

type Ptr = *mut c_void;
const SCALE: f64 = 1024.0;

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> Ptr;
    fn dlsym(handle: Ptr, name: *const c_char) -> Ptr;
    fn dlerror() -> *const c_char;
}

fn library(name: &str) -> Result<Ptr, String> {
    let name = CString::new(name).unwrap();
    let handle = unsafe { dlopen(name.as_ptr(), 2) }; // RTLD_NOW, local symbols
    if handle.is_null() {
        let detail = unsafe { dlerror() };
        let detail = if detail.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(detail) }
                .to_string_lossy()
                .into_owned()
        };
        Err(format!(
            "Cannot load {}: {detail}. Install the corresponding system library.",
            name.to_string_lossy()
        ))
    } else {
        Ok(handle)
    }
}

// Every conversion uses a checked, non-null symbol and a concrete function type.
macro_rules! load {
    ($lib:expr, $symbol:literal, $ty:ty) => {{
        let p = unsafe { dlsym($lib, concat!($symbol, "\0").as_ptr().cast()) };
        if p.is_null() {
            return Err(format!("Missing native symbol {}", $symbol));
        }
        unsafe { std::mem::transmute::<Ptr, $ty>(p) }
    }};
}

macro_rules! functions {
    ($name:ident { $( $field:ident : $ty:ty = $lib:ident, $symbol:literal; )* }) => {
        struct $name { $( $field: $ty, )* }
    };
}

functions!(Api {
    surface_create: unsafe extern "C" fn(c_int, c_int, c_int) -> Ptr = c, "cairo_image_surface_create";
    pdf_create: unsafe extern "C" fn(*const c_char, f64, f64) -> Ptr = c, "cairo_pdf_surface_create";
    pdf_size: unsafe extern "C" fn(Ptr, f64, f64) = c, "cairo_pdf_surface_set_size";
    surface_status: unsafe extern "C" fn(Ptr) -> c_int = c, "cairo_surface_status";
    surface_finish: unsafe extern "C" fn(Ptr) = c, "cairo_surface_finish";
    surface_destroy: unsafe extern "C" fn(Ptr) = c, "cairo_surface_destroy";
    create: unsafe extern "C" fn(Ptr) -> Ptr = c, "cairo_create";
    destroy: unsafe extern "C" fn(Ptr) = c, "cairo_destroy";
    status: unsafe extern "C" fn(Ptr) -> c_int = c, "cairo_status";
    status_string: unsafe extern "C" fn(c_int) -> *const c_char = c, "cairo_status_to_string";
    save: unsafe extern "C" fn(Ptr) = c, "cairo_save";
    restore: unsafe extern "C" fn(Ptr) = c, "cairo_restore";
    move_to: unsafe extern "C" fn(Ptr, f64, f64) = c, "cairo_move_to";
    rgb: unsafe extern "C" fn(Ptr, f64, f64, f64) = c, "cairo_set_source_rgb";
    rectangle: unsafe extern "C" fn(Ptr, f64, f64, f64, f64) = c, "cairo_rectangle";
    fill: unsafe extern "C" fn(Ptr) = c, "cairo_fill";
    stroke: unsafe extern "C" fn(Ptr) = c, "cairo_stroke";
    line_width: unsafe extern "C" fn(Ptr, f64) = c, "cairo_set_line_width";
    clip: unsafe extern "C" fn(Ptr) = c, "cairo_clip";
    new_path: unsafe extern "C" fn(Ptr) = c, "cairo_new_path";
    show_page: unsafe extern "C" fn(Ptr) = c, "cairo_show_page";
    options_new: unsafe extern "C" fn() -> Ptr = c, "cairo_font_options_create";
    options_hint: unsafe extern "C" fn(Ptr, c_int) = c, "cairo_font_options_set_hint_metrics";
    options_destroy: unsafe extern "C" fn(Ptr) = c, "cairo_font_options_destroy";
    context_options: unsafe extern "C" fn(Ptr, Ptr) = pc, "pango_cairo_context_set_font_options";
    layout_new: unsafe extern "C" fn(Ptr) -> Ptr = pc, "pango_cairo_create_layout";
    layout_show: unsafe extern "C" fn(Ptr, Ptr) = pc, "pango_cairo_show_layout";
    resolution: unsafe extern "C" fn(Ptr, f64) = pc, "pango_cairo_context_set_resolution";
    layout_context: unsafe extern "C" fn(Ptr) -> Ptr = p, "pango_layout_get_context";
    context_changed: unsafe extern "C" fn(Ptr) = p, "pango_layout_context_changed";
    context_matrix: unsafe extern "C" fn(Ptr, *const c_void) = p, "pango_context_set_matrix";
    markup: unsafe extern "C" fn(Ptr, *const c_char, c_int) = p, "pango_layout_set_markup";
    font_new: unsafe extern "C" fn() -> Ptr = p, "pango_font_description_new";
    font_family: unsafe extern "C" fn(Ptr, *const c_char) = p, "pango_font_description_set_family";
    font_size: unsafe extern "C" fn(Ptr, c_int) = p, "pango_font_description_set_size";
    font_weight: unsafe extern "C" fn(Ptr, c_int) = p, "pango_font_description_set_weight";
    font_free: unsafe extern "C" fn(Ptr) = p, "pango_font_description_free";
    layout_font: unsafe extern "C" fn(Ptr, Ptr) = p, "pango_layout_set_font_description";
    layout_width: unsafe extern "C" fn(Ptr, c_int) = p, "pango_layout_set_width";
    layout_wrap: unsafe extern "C" fn(Ptr, c_int) = p, "pango_layout_set_wrap";
    layout_extents: unsafe extern "C" fn(Ptr, *mut PangoRect, *mut PangoRect) = p, "pango_layout_get_extents";
    iter_new: unsafe extern "C" fn(Ptr) -> Ptr = p, "pango_layout_get_iter";
    iter_line: unsafe extern "C" fn(Ptr) -> *const PangoLine = p, "pango_layout_iter_get_line_readonly";
    iter_extents: unsafe extern "C" fn(Ptr, *mut PangoRect, *mut PangoRect) = p, "pango_layout_iter_get_line_extents";
    iter_next: unsafe extern "C" fn(Ptr) -> c_int = p, "pango_layout_iter_next_line";
    iter_free: unsafe extern "C" fn(Ptr) = p, "pango_layout_iter_free";
    unref: unsafe extern "C" fn(Ptr) = g, "g_object_unref";
    error_free: unsafe extern "C" fn(*mut GError) = gl, "g_error_free";
});

impl Api {
    fn load() -> Result<Self, String> {
        let c = library("libcairo.so.2")?;
        let p = library("libpango-1.0.so.0")?;
        let pc = library("libpangocairo-1.0.so.0")?;
        let g = library("libgobject-2.0.so.0")?;
        let gl = library("libglib-2.0.so.0")?;
        // Keep declarations and loading in one place using a local constructor.
        macro_rules! build {
            ($( $field:ident : $ty:ty = $lib:ident, $symbol:literal; )*) => {
                Ok(Self { $( $field: load!($lib, $symbol, $ty), )* })
            }
        }
        build! {
            surface_create: unsafe extern "C" fn(c_int,c_int,c_int)->Ptr = c,"cairo_image_surface_create";
            pdf_create: unsafe extern "C" fn(*const c_char,f64,f64)->Ptr = c,"cairo_pdf_surface_create";
            pdf_size: unsafe extern "C" fn(Ptr,f64,f64) = c,"cairo_pdf_surface_set_size";
            surface_status: unsafe extern "C" fn(Ptr)->c_int = c,"cairo_surface_status";
            surface_finish: unsafe extern "C" fn(Ptr) = c,"cairo_surface_finish";
            surface_destroy: unsafe extern "C" fn(Ptr) = c,"cairo_surface_destroy";
            create: unsafe extern "C" fn(Ptr)->Ptr = c,"cairo_create";
            destroy: unsafe extern "C" fn(Ptr) = c,"cairo_destroy";
            status: unsafe extern "C" fn(Ptr)->c_int = c,"cairo_status";
            status_string: unsafe extern "C" fn(c_int)->*const c_char = c,"cairo_status_to_string";
            save: unsafe extern "C" fn(Ptr) = c,"cairo_save";
            restore: unsafe extern "C" fn(Ptr) = c,"cairo_restore";
            move_to: unsafe extern "C" fn(Ptr,f64,f64) = c,"cairo_move_to";
            rgb: unsafe extern "C" fn(Ptr,f64,f64,f64) = c,"cairo_set_source_rgb";
            rectangle: unsafe extern "C" fn(Ptr,f64,f64,f64,f64) = c,"cairo_rectangle";
            fill: unsafe extern "C" fn(Ptr) = c,"cairo_fill";
            stroke: unsafe extern "C" fn(Ptr) = c,"cairo_stroke";
            line_width: unsafe extern "C" fn(Ptr,f64) = c,"cairo_set_line_width";
            clip: unsafe extern "C" fn(Ptr) = c,"cairo_clip";
            new_path: unsafe extern "C" fn(Ptr) = c,"cairo_new_path";
            show_page: unsafe extern "C" fn(Ptr) = c,"cairo_show_page";
            options_new: unsafe extern "C" fn()->Ptr = c,"cairo_font_options_create";
            options_hint: unsafe extern "C" fn(Ptr,c_int) = c,"cairo_font_options_set_hint_metrics";
            options_destroy: unsafe extern "C" fn(Ptr) = c,"cairo_font_options_destroy";
            context_options: unsafe extern "C" fn(Ptr,Ptr) = pc,"pango_cairo_context_set_font_options";
            layout_new: unsafe extern "C" fn(Ptr)->Ptr = pc,"pango_cairo_create_layout";
            layout_show: unsafe extern "C" fn(Ptr,Ptr) = pc,"pango_cairo_show_layout";
            resolution: unsafe extern "C" fn(Ptr,f64) = pc,"pango_cairo_context_set_resolution";
            layout_context: unsafe extern "C" fn(Ptr)->Ptr = p,"pango_layout_get_context";
            context_changed: unsafe extern "C" fn(Ptr) = p,"pango_layout_context_changed";
            context_matrix: unsafe extern "C" fn(Ptr,*const c_void) = p,"pango_context_set_matrix";
            markup: unsafe extern "C" fn(Ptr,*const c_char,c_int) = p,"pango_layout_set_markup";
            font_new: unsafe extern "C" fn()->Ptr = p,"pango_font_description_new";
            font_family: unsafe extern "C" fn(Ptr,*const c_char) = p,"pango_font_description_set_family";
            font_size: unsafe extern "C" fn(Ptr,c_int) = p,"pango_font_description_set_size";
            font_weight: unsafe extern "C" fn(Ptr,c_int) = p,"pango_font_description_set_weight";
            font_free: unsafe extern "C" fn(Ptr) = p,"pango_font_description_free";
            layout_font: unsafe extern "C" fn(Ptr,Ptr) = p,"pango_layout_set_font_description";
            layout_width: unsafe extern "C" fn(Ptr,c_int) = p,"pango_layout_set_width";
            layout_wrap: unsafe extern "C" fn(Ptr,c_int) = p,"pango_layout_set_wrap";
            layout_extents: unsafe extern "C" fn(Ptr,*mut PangoRect,*mut PangoRect) = p,"pango_layout_get_extents";
            iter_new: unsafe extern "C" fn(Ptr)->Ptr = p,"pango_layout_get_iter";
            iter_line: unsafe extern "C" fn(Ptr)->*const PangoLine = p,"pango_layout_iter_get_line_readonly";
            iter_extents: unsafe extern "C" fn(Ptr,*mut PangoRect,*mut PangoRect) = p,"pango_layout_iter_get_line_extents";
            iter_next: unsafe extern "C" fn(Ptr)->c_int = p,"pango_layout_iter_next_line";
            iter_free: unsafe extern "C" fn(Ptr) = p,"pango_layout_iter_free";
            unref: unsafe extern "C" fn(Ptr) = g,"g_object_unref";
            error_free: unsafe extern "C" fn(*mut GError) = gl,"g_error_free";
        }
    }

    fn check(&self, status: c_int) -> Result<(), String> {
        if status == 0 {
            return Ok(());
        }
        let message = unsafe { (self.status_string)(status) };
        if message.is_null() {
            return Err(format!("Cairo status {status}"));
        }
        Err(format!(
            "Cairo: {}",
            unsafe { CStr::from_ptr(message) }.to_string_lossy()
        ))
    }

    fn check_context(&self, cr: Ptr) -> Result<(), String> {
        if cr.is_null() {
            return Err("Null Cairo context".into());
        }
        self.check(unsafe { (self.status)(cr) })
    }
}

#[repr(C)]
#[derive(Default)]
struct PangoRect {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
}
#[repr(C)]
// Read-only prefix of PangoLayoutLine; the remaining fields (runs/bitfields)
// are never accessed, allocated or copied by Rust.
struct PangoLine {
    layout: Ptr,
    start_index: c_int,
    length: c_int,
}
#[repr(C)]
struct GError {
    domain: u32,
    code: c_int,
    message: *const c_char,
}
#[repr(C)]
struct RsvgRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Default)]
struct RsvgLength {
    length: f64,
    unit: c_int,
}

struct Rsvg {
    new: unsafe extern "C" fn(*const u8, usize, *mut *mut GError) -> Ptr,
    size: unsafe extern "C" fn(Ptr, *mut f64, *mut f64) -> c_int,
    render: unsafe extern "C" fn(Ptr, Ptr, *const RsvgRect, *mut *mut GError) -> c_int,
    dpi: unsafe extern "C" fn(Ptr, f64),
    dimensions: unsafe extern "C" fn(
        Ptr,
        *mut c_int,
        *mut RsvgLength,
        *mut c_int,
        *mut RsvgLength,
        *mut c_int,
        *mut RsvgRect,
    ),
}
impl Rsvg {
    fn load() -> Result<Self, String> {
        let r = library("librsvg-2.so.2")?;
        Ok(Self {
            new: load!(
                r,
                "rsvg_handle_new_from_data",
                unsafe extern "C" fn(*const u8, usize, *mut *mut GError) -> Ptr
            ),
            size: load!(
                r,
                "rsvg_handle_get_intrinsic_size_in_pixels",
                unsafe extern "C" fn(Ptr, *mut f64, *mut f64) -> c_int
            ),
            render: load!(
                r,
                "rsvg_handle_render_document",
                unsafe extern "C" fn(Ptr, Ptr, *const RsvgRect, *mut *mut GError) -> c_int
            ),
            dpi: load!(r, "rsvg_handle_set_dpi", unsafe extern "C" fn(Ptr, f64)),
            dimensions: load!(
                r,
                "rsvg_handle_get_intrinsic_dimensions",
                unsafe extern "C" fn(
                    Ptr,
                    *mut c_int,
                    *mut RsvgLength,
                    *mut c_int,
                    *mut RsvgLength,
                    *mut c_int,
                    *mut RsvgRect,
                )
            ),
        })
    }
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();
static RSVG: OnceLock<Result<Rsvg, String>> = OnceLock::new();
pub struct Native {
    api: &'static Api,
}
pub struct TextLayout {
    pub width: f64,
    pub height: f64,
    pub lines: Vec<TextLine>,
}
pub struct TextLine {
    pub start: usize,
    pub end: usize,
    pub height: f64,
}

struct Object<'a> {
    ptr: Ptr,
    api: &'a Api,
}
impl Drop for Object<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.unref)(self.ptr) }
    }
}

pub struct Pdf {
    api: &'static Api,
    surface: Ptr,
    cr: Ptr,
}
impl Drop for Pdf {
    fn drop(&mut self) {
        unsafe {
            if !self.cr.is_null() {
                (self.api.destroy)(self.cr);
            }
            if !self.surface.is_null() {
                (self.api.surface_destroy)(self.surface);
            }
        }
    }
}
impl Pdf {
    pub fn context(&self) -> Ptr {
        self.cr
    }
    pub fn page(&self, width: f64, height: f64) -> Result<(), String> {
        dimensions(width, height)?;
        unsafe {
            (self.api.pdf_size)(self.surface, width, height);
        }
        self.api
            .check(unsafe { (self.api.surface_status)(self.surface) })?;
        self.api.check_context(self.cr)
    }
    pub fn end_page(&self) -> Result<(), String> {
        unsafe {
            (self.api.show_page)(self.cr);
        }
        self.api.check_context(self.cr)?;
        self.api
            .check(unsafe { (self.api.surface_status)(self.surface) })
    }
    pub fn finish(self) -> Result<(), String> {
        self.api.check_context(self.cr)?;
        unsafe {
            (self.api.surface_finish)(self.surface);
        }
        self.api
            .check(unsafe { (self.api.surface_status)(self.surface) })
    }
}

fn dimensions(w: f64, h: f64) -> Result<(), String> {
    // Stay within both Pango's signed fixed-point coordinates and Cairo's
    // internal fixed-point geometry, leaving room for arithmetic/rounding.
    if [w, h]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0 && *v <= 1_000_000.0)
    {
        Ok(())
    } else {
        Err("Dimensions must be positive and at most 1,000,000 points".into())
    }
}

fn escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            // GMarkup normalizes literal CRLF/CR to LF. A character reference
            // keeps the original bytes represented in Pango's line indices.
            '\r' => out.push_str("&#13;"),
            _ => out.push(ch),
        }
    }
}

fn markup(spans: &[Span]) -> Result<CString, String> {
    let mut out = String::new();
    for span in spans {
        if span.text.chars().any(|ch| matches!(ch, '\0'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{fffe}' | '\u{ffff}')) {
            return Err("Text contains a control character unsupported by Pango markup".into());
        }
        // All attributes are constants; user text and URL never become markup.
        let color = match span.role {
            Role::Link => "#175bb5",
            Role::Code => "#8a2454",
            Role::Faint | Role::Done => "#60666d",
            Role::Image => "#566376",
            Role::Bullet | Role::Todo => "#805b18",
            Role::None => "#20242a",
        };
        out.push_str(&format!("<span foreground=\"{color}\""));
        if span.style.contains(Style::BOLD) {
            out.push_str(" weight=\"bold\"");
        }
        if span.style.contains(Style::ITALIC) {
            out.push_str(" style=\"italic\"");
        }
        if span.style.contains(Style::STRIKE) {
            out.push_str(" strikethrough=\"true\"");
        }
        if span.style.contains(Style::UNDER) || span.role == Role::Link {
            out.push_str(" underline=\"single\"");
        }
        if span.role == Role::Code {
            out.push_str(" font_family=\"monospace\"");
        }
        out.push('>');
        escape(&span.text, &mut out);
        out.push_str("</span>");
    }
    if out.len() > c_int::MAX as usize {
        return Err("Text exceeds Pango's size limit".into());
    }
    CString::new(out).map_err(|_| "Text contains a NUL byte".into())
}

impl Native {
    pub fn load() -> Result<Self, String> {
        Ok(Self {
            api: API.get_or_init(Api::load).as_ref().map_err(Clone::clone)?,
        })
    }

    fn layout(
        &self,
        cr: Ptr,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    ) -> Result<Object<'_>, String> {
        self.api.check_context(cr)?;
        if !size.is_finite() || size <= 0.0 || size * SCALE > c_int::MAX as f64 {
            return Err("Invalid font size".into());
        }
        if let Some(w) = width {
            if !w.is_finite() || w <= 0.0 || w * SCALE > c_int::MAX as f64 {
                return Err("Invalid text width".into());
            }
        }
        let text = markup(spans)?;
        unsafe {
            let ptr = (self.api.layout_new)(cr);
            if ptr.is_null() {
                return Err("Cannot create Pango layout".into());
            }
            let layout = Object { ptr, api: self.api };
            let context = (self.api.layout_context)(ptr);
            if context.is_null() {
                return Err("Cannot create Pango context".into());
            }
            (self.api.resolution)(context, 72.0);
            // Shape in document points, not the printer's device transform.
            // Cairo applies its existing transform when showing the layout.
            (self.api.context_matrix)(context, std::ptr::null());
            let options = (self.api.options_new)();
            if options.is_null() {
                return Err("Cannot create Cairo font options".into());
            }
            // Avoid device-pixel rounding: image measurement and vector PDF
            // drawing must use the same advances and line breaks.
            (self.api.options_hint)(options, 1); // CAIRO_HINT_METRICS_OFF
            (self.api.context_options)(context, options);
            (self.api.options_destroy)(options);
            (self.api.context_changed)(ptr);
            let font = (self.api.font_new)();
            if font.is_null() {
                return Err("Cannot create Pango font description".into());
            }
            (self.api.font_family)(
                font,
                if mono {
                    c"monospace".as_ptr()
                } else {
                    c"sans-serif".as_ptr()
                },
            );
            (self.api.font_size)(font, (size * SCALE).round().max(1.0) as c_int);
            (self.api.font_weight)(font, if bold { 700 } else { 400 });
            (self.api.layout_font)(ptr, font);
            (self.api.font_free)(font);
            (self.api.layout_width)(
                ptr,
                width.map_or(-1, |w| (w * SCALE).round().max(1.0) as c_int),
            );
            (self.api.layout_wrap)(ptr, 2); // PANGO_WRAP_WORD_CHAR
            (self.api.markup)(ptr, text.as_ptr(), text.as_bytes().len() as c_int);
            Ok(layout)
        }
    }

    pub fn text(
        &self,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    ) -> Result<TextLayout, String> {
        // A tiny image surface only provides a font context; no display required.
        let surface = unsafe { (self.api.surface_create)(0, 1, 1) };
        if surface.is_null() {
            return Err("Cannot create Cairo measurement surface".into());
        }
        let cr = unsafe { (self.api.create)(surface) };
        let scratch = Pdf {
            api: self.api,
            surface,
            cr,
        };
        self.api
            .check(unsafe { (self.api.surface_status)(surface) })?;
        let layout = self.layout(scratch.cr, spans, size, bold, mono, width)?;
        let mut logical = PangoRect::default();
        let mut lines = Vec::new();
        unsafe {
            (self.api.layout_extents)(layout.ptr, std::ptr::null_mut(), &mut logical);
            if logical.width < 0 || logical.height < 0 {
                return Err("Text exceeds Pango's coordinate range".into());
            }
            let iter = (self.api.iter_new)(layout.ptr);
            if iter.is_null() {
                return Err("Cannot iterate Pango layout".into());
            }
            loop {
                let line = (self.api.iter_line)(iter);
                if !line.is_null() {
                    let mut rect = PangoRect::default();
                    (self.api.iter_extents)(iter, std::ptr::null_mut(), &mut rect);
                    // Pango excludes paragraph separators from each line's
                    // length, but includes them when advancing start_index.
                    lines.push(TextLine {
                        start: (*line).start_index.max(0) as usize,
                        end: ((*line).start_index as i64 + (*line).length as i64).max(0) as usize,
                        height: (rect.height as f64 / SCALE).max(size),
                    });
                }
                if (self.api.iter_next)(iter) == 0 {
                    break;
                }
            }
            (self.api.iter_free)(iter);
        }
        self.api.check_context(cr)?;
        Ok(TextLayout {
            width: logical.width as f64 / SCALE,
            height: (logical.height as f64 / SCALE).max(size),
            lines,
        })
    }

    pub fn pdf(&self, path: &Path, width: f64, height: f64) -> Result<Pdf, String> {
        use std::os::unix::ffi::OsStrExt;
        dimensions(width, height)?;
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| "PDF path contains NUL")?;
        let surface = unsafe { (self.api.pdf_create)(path.as_ptr(), width, height) };
        if surface.is_null() {
            return Err("Cannot create PDF surface".into());
        }
        let cr = unsafe { (self.api.create)(surface) };
        let pdf = Pdf {
            api: self.api,
            surface,
            cr,
        };
        self.api
            .check(unsafe { (self.api.surface_status)(surface) })?;
        self.api.check_context(cr)?;
        Ok(pdf)
    }

    pub fn draw_text(
        &self,
        cr: Ptr,
        x: f64,
        y: f64,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    ) -> Result<(), String> {
        if !x.is_finite() || !y.is_finite() {
            return Err("Invalid text position".into());
        }
        let layout = self.layout(cr, spans, size, bold, mono, width)?;
        unsafe {
            (self.api.save)(cr);
            (self.api.move_to)(cr, x, y);
            (self.api.layout_show)(cr, layout.ptr);
            (self.api.restore)(cr);
        }
        self.api.check_context(cr)
    }

    pub fn rect(
        &self,
        cr: Ptr,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        fill: Option<(f64, f64, f64)>,
        stroke: Option<(f64, f64, f64)>,
    ) -> Result<(), String> {
        self.api.check_context(cr)?;
        if ![x, y, w, h].iter().all(|v| v.is_finite()) || w < 0.0 || h < 0.0 {
            return Err("Invalid rectangle".into());
        }
        for color in [fill, stroke].into_iter().flatten() {
            if ![color.0, color.1, color.2].iter().all(|v| v.is_finite()) {
                return Err("Invalid rectangle color".into());
            }
        }
        unsafe {
            (self.api.save)(cr);
            (self.api.new_path)(cr);
            if let Some((r, g, b)) = fill {
                (self.api.rgb)(cr, r, g, b);
                (self.api.rectangle)(cr, x, y, w, h);
                (self.api.fill)(cr);
            }
            if let Some((r, g, b)) = stroke {
                (self.api.rgb)(cr, r, g, b);
                (self.api.line_width)(cr, 0.75);
                (self.api.rectangle)(cr, x, y, w, h);
                (self.api.stroke)(cr);
            }
            (self.api.restore)(cr);
        }
        self.api.check_context(cr)
    }

    fn svg<'a>(&'a self, svg: &str) -> Result<(&'static Rsvg, Object<'a>), String> {
        safe_svg(svg)?;
        let r = RSVG
            .get_or_init(Rsvg::load)
            .as_ref()
            .map_err(|e| format!("SVG support requires librsvg 2.52 or newer: {e}"))?;
        let mut error = std::ptr::null_mut();
        // No base URI is ever supplied: librsvg cannot resolve filesystem or
        // network resources. The conservative preflight also rejects references.
        let ptr = unsafe { (r.new)(svg.as_ptr(), svg.len(), &mut error) };
        if ptr.is_null() {
            return Err(self.svg_error(error, "Cannot parse SVG"));
        }
        if !error.is_null() {
            unsafe {
                (self.api.error_free)(error);
            }
        }
        unsafe {
            (r.dpi)(ptr, 96.0);
        }
        Ok((r, Object { ptr, api: self.api }))
    }

    fn svg_error(&self, error: *mut GError, fallback: &str) -> String {
        if error.is_null() {
            return fallback.into();
        }
        unsafe {
            let message = if (*error).message.is_null() {
                fallback.into()
            } else {
                CStr::from_ptr((*error).message)
                    .to_string_lossy()
                    .into_owned()
            };
            (self.api.error_free)(error);
            format!("SVG: {message}")
        }
    }

    pub fn svg_size(&self, svg: &str) -> Result<(f64, f64), String> {
        let (r, handle) = self.svg(svg)?;
        let (mut w, mut h) = (0.0, 0.0);
        if unsafe { (r.size)(handle.ptr, &mut w, &mut h) } == 0 {
            // Mermaid commonly emits width="100%" and a viewBox. Use that
            // natural coordinate size when CSS percentages lack a viewport.
            let (mut has_w, mut has_h, mut has_viewbox) = (0, 0, 0);
            let (mut rw, mut rh) = (RsvgLength::default(), RsvgLength::default());
            let mut viewbox = RsvgRect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            };
            unsafe {
                (r.dimensions)(
                    handle.ptr,
                    &mut has_w,
                    &mut rw,
                    &mut has_h,
                    &mut rh,
                    &mut has_viewbox,
                    &mut viewbox,
                );
            }
            if has_viewbox == 0 {
                return Err("SVG has neither intrinsic dimensions nor a viewBox".into());
            }
            w = viewbox.width;
            h = viewbox.height;
        }
        dimensions(w, h)?;
        Ok((w, h))
    }

    pub fn draw_svg(
        &self,
        cr: Ptr,
        svg: &str,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        clip: Option<(f64, f64, f64, f64)>,
    ) -> Result<(), String> {
        self.api.check_context(cr)?;
        dimensions(width, height)?;
        if !x.is_finite() || !y.is_finite() {
            return Err("Invalid SVG position".into());
        }
        if let Some((cx, cy, cw, ch)) = clip {
            if ![cx, cy, cw, ch].iter().all(|v| v.is_finite()) || cw < 0.0 || ch < 0.0 {
                return Err("Invalid SVG clip".into());
            }
        }
        let (r, handle) = self.svg(svg)?;
        let viewport = RsvgRect {
            x,
            y,
            width,
            height,
        };
        let mut error = std::ptr::null_mut();
        let ok = unsafe {
            (self.api.save)(cr);
            // Cairo's current path is NOT part of save/restore. Clear it so a
            // caller's path cannot enlarge the clip, and always clip the SVG
            // viewport (render_document does not promise to do that for us).
            (self.api.new_path)(cr);
            (self.api.rectangle)(cr, x, y, width, height);
            (self.api.clip)(cr);
            if let Some((cx, cy, cw, ch)) = clip {
                (self.api.rectangle)(cr, cx, cy, cw, ch);
                (self.api.clip)(cr);
            }
            let ok = (r.render)(handle.ptr, cr, &viewport, &mut error);
            (self.api.restore)(cr);
            ok
        };
        if ok == 0 {
            return Err(self.svg_error(error, "Cannot render SVG"));
        }
        if !error.is_null() {
            unsafe {
                (self.api.error_free)(error);
            }
        }
        self.api.check_context(cr)
    }
}

/// A deliberately conservative policy for untrusted SVG. Internal fragment
/// references work; external images, stylesheets, entities and CSS escapes don't.
/// librsvg's no-base-URI policy is the second, independent protection layer.
fn safe_svg(svg: &str) -> Result<(), String> {
    if svg.len() > 16 * 1024 * 1024 {
        return Err("SVG exceeds the 16 MiB safety limit".into());
    }
    // XML character references are decoded BEFORE CSS is parsed. Inspect that
    // decoded spelling too, so e.g. u&#114;l(...) and &#64;import cannot bypass
    // the policy. Named XML entities cannot encode CSS punctuation/keywords;
    // custom entities are prohibited below along with all DTDs.
    let mut decoded = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(pos) = rest.find("&#") {
        decoded.push_str(&rest[..pos]);
        rest = &rest[pos + 2..];
        let Some(end) = rest.find(';') else {
            return Err("Malformed SVG character reference".into());
        };
        let digits = &rest[..end];
        let value = if let Some(hex) = digits.strip_prefix('x') {
            u32::from_str_radix(hex, 16)
        } else {
            digits.parse::<u32>()
        };
        let ch = value
            .ok()
            .and_then(char::from_u32)
            .ok_or("Invalid SVG character reference")?;
        decoded.push(ch);
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    let lower = decoded.to_ascii_lowercase();
    if lower.contains("<!doctype")
        || lower.contains("<!entity")
        || lower.contains("<?xml-stylesheet")
        || lower.contains("xml:base")
        || lower.contains('\\')
        || lower.contains('@')
        || lower.contains('\0')
        || lower.contains("/*")
    {
        return Err(
            "SVG external resources, entities, base URIs and CSS imports/escapes are disabled"
                .into(),
        );
    }
    for (needle, css) in [("href", false), ("url", true)] {
        let mut rest = lower.as_str();
        while let Some(pos) = rest.find(needle) {
            rest = &rest[pos + needle.len()..];
            let tail = rest.trim_start();
            let marker = if css { '(' } else { '=' };
            if !tail.starts_with(marker) {
                continue;
            }
            let value = tail[1..]
                .trim_start()
                .trim_start_matches(['\'', '"'])
                .trim_start();
            if !value.starts_with('#') {
                return Err(
                    "SVG external references are disabled; only #fragment references are allowed"
                        .into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markup_is_escaped() {
        let span = Span {
            text: "<é&\"'>".into(),
            style: Style::BOLD | Style::ITALIC,
            role: Role::Link,
            url: Some("\"evil".into()),
        };
        let m = markup(&[span]).unwrap();
        let m = m.to_str().unwrap();
        assert!(m.contains("&lt;é&amp;&quot;&apos;&gt;"));
        assert!(!m.contains("evil"));
    }
    #[test]
    fn svg_resource_policy() {
        assert!(safe_svg("<svg><use href='#a'/><path fill='url(#a)'/></svg>").is_ok());
        assert!(safe_svg("<svg><use href='&#35;a'/></svg>").is_ok());
        for s in [
            "<svg><image href='file:///etc/passwd'/></svg>",
            "<svg xml:base='file:///'/>",
            "<!DOCTYPE svg []><svg/>",
            "<svg><style>@import 'x';</style></svg>",
            "<svg><path fill='url(https://example.org)'/></svg>",
            "<svg><path fill='u&#114;l(file:///etc/passwd)'/></svg>",
            "<svg><style>&#64;import 'file:///etc/passwd';</style></svg>",
            "<svg><style>/* comment */ @import 'x';</style></svg>",
            "<svg><path fill='&#x5c;75rl(file:///etc/passwd)'/></svg>",
            "<svg><image href='data:image/png;base64,AAAA'/></svg>",
            "<?xml-stylesheet href='file:///etc/passwd'?><svg/>",
            "<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><svg>&x;</svg>",
        ] {
            assert!(safe_svg(s).is_err(), "{s}");
        }
    }

    #[test]
    fn metrics_ignore_printer_device_scale() -> Result<(), String> {
        let Ok(native) = Native::load() else {
            return Ok(());
        };
        let c = library("libcairo.so.2")?;
        let scale = load!(c, "cairo_scale", unsafe extern "C" fn(Ptr, f64, f64));
        let surface = unsafe { (native.api.surface_create)(0, 1, 1) };
        let cr = unsafe { (native.api.create)(surface) };
        let scratch = Pdf {
            api: native.api,
            surface,
            cr,
        };
        let spans = [Span {
            text: "Styled é a\u{301}文 text with wrapping".into(),
            style: Style::BOLD | Style::ITALIC,
            role: Role::Code,
            url: None,
        }];
        let expected = native.text(&spans, 12.0, false, false, Some(81.0))?;
        for factor in [1.0, 300.0 / 72.0, 600.0 / 72.0] {
            unsafe {
                (native.api.save)(cr);
                scale(cr, factor, factor);
            }
            let layout = native.layout(cr, &spans, 12.0, false, false, Some(81.0))?;
            let mut logical = PangoRect::default();
            unsafe {
                (native.api.layout_extents)(layout.ptr, std::ptr::null_mut(), &mut logical);
            }
            assert_eq!(logical.width as f64 / SCALE, expected.width);
            assert_eq!(logical.height as f64 / SCALE, expected.height);
            native.draw_text(cr, 0.0, 0.0, &spans, 12.0, false, false, Some(81.0))?;
            unsafe {
                (native.api.restore)(cr);
            }
        }
        drop(scratch);
        Ok(())
    }

    #[test]
    fn tiny_widths_preserve_graphemes_and_zero_width_text() {
        let Ok(native) = Native::load() else {
            return;
        };
        let text = "a\u{301}👩\u{200d}💻文";
        let spans = [Span {
            text: text.into(),
            style: Style::empty(),
            role: Role::None,
            url: None,
        }];
        let layout = native
            .text(&spans, 12.0, false, false, Some(f64::MIN_POSITIVE))
            .unwrap();
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(
            layout
                .lines
                .iter()
                .map(|l| &text[l.start..l.end])
                .collect::<Vec<_>>(),
            ["a\u{301}", "👩\u{200d}💻", "文"]
        );
        let spans = [Span {
            text: "\u{200b}\u{200d}".into(),
            style: Style::empty(),
            role: Role::None,
            url: None,
        }];
        let layout = native
            .text(&spans, 12.0, false, false, Some(100.0))
            .unwrap();
        assert_eq!(layout.width, 0.0);
        assert!(layout.height > 0.0);
        assert!(native
            .text(&[], 12.0, false, false, Some(f64::MAX))
            .is_err());
        assert!(dimensions(f64::MAX, 100.0).is_err());
        assert!(native
            .draw_text(
                std::ptr::null_mut(),
                0.0,
                0.0,
                &[],
                12.0,
                false,
                false,
                None
            )
            .is_err());
    }

    #[test]
    fn pdf_write_errors_are_returned() {
        let Ok(native) = Native::load() else {
            return;
        };
        if !Path::new("/dev/full").exists() {
            return;
        }
        // Cairo buffers output; disk errors can appear only when finishing.
        if let Ok(pdf) = native.pdf(Path::new("/dev/full"), 100.0, 100.0) {
            assert!(pdf.finish().is_err());
        }
        assert!(native
            .pdf(
                Path::new("/nonexistent-mdglass-directory/out.pdf"),
                100.0,
                100.0
            )
            .is_err());
    }

    #[test]
    fn svg_clips_viewport_and_ignores_inherited_paths() -> Result<(), String> {
        let Ok(native) = Native::load() else {
            return Ok(());
        };
        if RSVG.get_or_init(Rsvg::load).is_err() {
            return Ok(());
        }
        let c = library("libcairo.so.2")?;
        let flush = load!(c, "cairo_surface_flush", unsafe extern "C" fn(Ptr));
        let data = load!(
            c,
            "cairo_image_surface_get_data",
            unsafe extern "C" fn(Ptr) -> *mut u8
        );
        let stride = load!(
            c,
            "cairo_image_surface_get_stride",
            unsafe extern "C" fn(Ptr) -> c_int
        );
        let surface = unsafe { (native.api.surface_create)(0, 50, 50) };
        let cr = unsafe { (native.api.create)(surface) };
        let _scratch = Pdf {
            api: native.api,
            surface,
            cr,
        };
        native
            .api
            .check(unsafe { (native.api.surface_status)(surface) })?;
        let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 20 20' overflow='visible'><rect x='-20' y='-20' width='60' height='60' fill='blue'/></svg>";
        unsafe {
            (native.api.rectangle)(cr, 0.0, 0.0, 50.0, 50.0);
        }
        native.draw_svg(
            cr,
            svg,
            10.0,
            10.0,
            20.0,
            20.0,
            Some((15.0, 0.0, 5.0, 50.0)),
        )?;
        unsafe {
            flush(surface);
            let bytes = data(surface);
            assert!(!bytes.is_null());
            let stride = stride(surface) as usize;
            for (x, y, colored) in [
                (16, 15, true),
                (11, 15, false),
                (25, 15, false),
                (16, 5, false),
                (16, 35, false),
            ] {
                let pixel = std::ptr::read_unaligned(bytes.add(y * stride + x * 4).cast::<u32>());
                assert_eq!(pixel != 0, colored, "pixel ({x}, {y})");
            }
        }
        Ok(())
    }

    #[test]
    fn independent_headless_jobs_work_on_multiple_threads() {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    let Ok(native) = Native::load() else {
                        return;
                    };
                    for _ in 0..8 {
                        assert!(
                            native
                                .text(&[], 12.0, false, false, Some(100.0))
                                .unwrap()
                                .height
                                > 0.0
                        );
                        if RSVG.get_or_init(Rsvg::load).is_ok() {
                            assert!(native
                                .svg_size(
                                    "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 10 10'/>"
                                )
                                .is_ok());
                            assert!(native.svg_size("<svg broken").is_err());
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
    #[test]
    fn headless_measurement_and_pdf() {
        let Ok(native) = Native::load() else {
            return;
        }; // system libraries optional
        let spans = [Span {
            text: "é文\n\nlast\r\n".into(),
            style: Style::empty(),
            role: Role::None,
            url: None,
        }];
        let measured = native
            .text(&spans, 12.0, false, false, Some(200.0))
            .unwrap();
        assert!(measured.width > 0.0 && measured.height > 0.0);
        let ranges: Vec<_> = measured.lines.iter().map(|l| (l.start, l.end)).collect();
        assert_eq!(ranges, [(0, 5), (6, 6), (7, 11), (13, 13)]);
        assert!(measured.lines.iter().all(|l| l.height > 0.0));
        let empty = native.text(&[], 12.0, false, false, None).unwrap();
        assert!(empty.height > 0.0);
        let wrapped = native.text(&spans, 12.0, false, false, Some(15.0)).unwrap();
        assert!(wrapped.lines.len() > measured.lines.len());
        assert!(wrapped
            .lines
            .iter()
            .all(|l| spans[0].text.is_char_boundary(l.start)
                && spans[0].text.is_char_boundary(l.end)));
        let path = std::env::temp_dir().join(format!("mdglass-native-{}.pdf", std::process::id()));
        let pdf = native.pdf(&path, 200.0, 200.0).unwrap();
        pdf.page(200.0, 200.0).unwrap();
        native
            .draw_text(
                pdf.context(),
                10.0,
                10.0,
                &spans,
                12.0,
                false,
                false,
                Some(180.0),
            )
            .unwrap();
        native
            .rect(
                pdf.context(),
                10.0,
                100.0,
                30.0,
                20.0,
                Some((1.0, 0.0, 0.0)),
                None,
            )
            .unwrap();
        if RSVG.get_or_init(Rsvg::load).is_ok() {
            native.draw_svg(pdf.context(), "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 20 20'><rect width='20' height='20' fill='blue'/></svg>", 100.0,100.0,50.0,50.0,Some((110.0,100.0,20.0,50.0))).unwrap();
        }
        pdf.end_page().unwrap();
        pdf.page(300.0, 400.0).unwrap();
        native
            .draw_text(pdf.context(), 10.0, 10.0, &spans, 12.0, false, false, None)
            .unwrap();
        pdf.end_page().unwrap();
        pdf.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
        // Newer Cairo versions compress PDF dictionaries into object streams;
        // don't assume MediaBox or ToUnicode is visible in the raw file.
        assert!(bytes.len() > 500);
        assert!(bytes.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn svg_viewbox_size() {
        let Ok(native) = Native::load() else {
            return;
        };
        if RSVG.get_or_init(Rsvg::load).is_err() {
            return;
        }
        assert_eq!(
            native
                .svg_size("<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 120 60'/>")
                .unwrap(),
            (120.0, 60.0)
        );
        assert!(native
            .svg_size(
                "<svg xmlns='http://www.w3.org/2000/svg'><image href='file:///etc/passwd'/></svg>"
            )
            .is_err());
    }
}
