//! Paginated output shared by headless PDF export and the native print dialog.
//! Libraries are loaded only for an output job; terminal startup stays unchanged.

pub mod layout;
pub mod native;

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{atomic::AtomicBool, Arc};

use layout::{Item, PagePlan};
use native::Native;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paper {
    A4,
    A3,
    Letter,
    Legal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitMode {
    Auto,
    Fit,
    Tile,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub paper: Paper,
    pub landscape: bool,
    /// All dimensions, including font size, are in PDF points (1/72 inch).
    pub margin: f64,
    pub font_size: f64,
    pub table_mode: FitMode,
    pub diagram_mode: FitMode,
    pub repeat_first_column: bool,
    pub overlap: f64,
    pub diagrams: bool,
    pub urls: bool,
    /// Internal override for the physical dimensions selected by a printer.
    pub page_size_override: Option<(f64, f64)>,
    /// Cooperative cancellation, checked while paginating and between pages.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            paper: Paper::A4,
            landscape: false,
            margin: 36.0,
            font_size: 10.0,
            table_mode: FitMode::Auto,
            diagram_mode: FitMode::Auto,
            repeat_first_column: true,
            overlap: 12.0,
            diagrams: true,
            urls: true,
            page_size_override: None,
            cancel: None,
        }
    }
}

impl Settings {
    pub fn check_cancelled(&self) -> Result<(), String> {
        check_cancelled(&self.cancel)
    }

    pub fn page_size(&self) -> (f64, f64) {
        if let Some(size) = self.page_size_override {
            return size;
        }
        let (w, h) = match self.paper {
            Paper::A4 => (595.275590551, 841.88976378),
            Paper::A3 => (841.88976378, 1190.551181102),
            Paper::Letter => (612.0, 792.0),
            Paper::Legal => (612.0, 1008.0),
        };
        if self.landscape {
            (h, w)
        } else {
            (w, h)
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let (w, h) = self.page_size();
        if !w.is_finite()
            || !h.is_finite()
            || !(72.0..=14400.0).contains(&w)
            || !(72.0..=14400.0).contains(&h)
        {
            return Err("page dimensions must be finite and between 72 and 14400 points".into());
        }
        if !self.font_size.is_finite() || !(4.0..=72.0).contains(&self.font_size) {
            return Err("font size must be between 4 and 72 points".into());
        }
        if !self.margin.is_finite()
            || self.margin < 0.0
            || w - 2.0 * self.margin < 48.0
            || h - 2.0 * self.margin < 48.0
        {
            return Err(
                "margins must be nonnegative and leave at least 48 points of page content".into(),
            );
        }
        if !self.overlap.is_finite()
            || self.overlap < 0.0
            || self.overlap >= (w.min(h) - 2.0 * self.margin) / 2.0
        {
            return Err(
                "overlap must be nonnegative and less than half the content width and height"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Immutable, measured document snapshot. The same page painter serves PDF and
/// printing; changing the live document cannot change an in-flight print job.
pub struct PrintJob {
    native: Native,
    plan: PagePlan,
    cancel: Option<Arc<AtomicBool>>,
}

impl PrintJob {
    pub fn new(src: &str, settings: &Settings) -> Result<Self, String> {
        settings.validate()?;
        settings.check_cancelled()?;
        let native = Native::load()?;
        let plan = layout::paginate(src, settings, &native)?;
        settings.check_cancelled()?;
        Ok(Self { native, plan, cancel: settings.cancel.clone() })
    }

    pub fn for_page(
        src: &str,
        settings: &Settings,
        width: f64,
        height: f64,
    ) -> Result<Self, String> {
        let mut settings = settings.clone();
        settings.page_size_override = Some((width, height));
        Self::new(src, &settings)
    }

    pub fn page_count(&self) -> usize {
        self.plan.pages.len()
    }
    pub fn warnings(&self) -> &[String] {
        &self.plan.warnings
    }

    /// `cairo` must be a live Cairo context, used on its owning thread.
    pub fn draw_page(&self, index: usize, cairo: *mut c_void) -> Result<(), String> {
        check_cancelled(&self.cancel)?;
        if cairo.is_null() {
            return Err("print backend returned no drawing context".into());
        }
        let page = self
            .plan
            .pages
            .get(index)
            .ok_or("invalid output page number")?;
        self.native.rect(
            cairo,
            0.0,
            0.0,
            page.width,
            page.height,
            Some((1.0, 1.0, 1.0)),
            None,
        )?;
        for item in &page.items {
            check_cancelled(&self.cancel)?;
            match item {
                Item::Text {
                    x,
                    y,
                    spans,
                    size,
                    bold,
                    mono,
                    width,
                } => self
                    .native
                    .draw_text(cairo, *x, *y, spans, *size, *bold, *mono, *width)?,
                Item::Rect {
                    x,
                    y,
                    width,
                    height,
                    fill,
                    stroke,
                } => self
                    .native
                    .rect(cairo, *x, *y, *width, *height, *fill, *stroke)?,
                Item::Svg {
                    x,
                    y,
                    width,
                    height,
                    svg,
                    clip,
                } => self
                    .native
                    .draw_svg(cairo, svg, *x, *y, *width, *height, *clip)?,
            }
        }
        Ok(())
    }
}

fn check_cancelled(cancel: &Option<Arc<AtomicBool>>) -> Result<(), String> {
    if cancel.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err("PDF export cancelled".into())
    } else { Ok(()) }
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Write beside the destination, then rename only after Cairo successfully
/// finishes the PDF. An interrupted/failed export never truncates an old file.
pub fn export_pdf(src: &str, path: &Path, settings: &Settings) -> Result<Vec<String>, String> {
    let job = PrintJob::new(src, settings)?;
    save_pdf(job, path, settings, PrintJob::draw_page)
}

fn save_pdf(
    job: PrintJob,
    path: &Path,
    settings: &Settings,
    mut draw: impl FnMut(&PrintJob, usize, *mut c_void) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if path.file_name().is_none() {
        return Err("PDF destination must be a file".into());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = None;
    for _ in 0..128 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let name = format!(".mdglass-{}-{id}.tmp", std::process::id());
        let candidate = parent.join(name);
        match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => {
                temporary = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create PDF in {}: {e}", parent.display())),
        }
    }
    let directory = temporary.ok_or("cannot allocate a private temporary PDF directory")?;
    let temporary = directory.join("document.pdf");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("document.pdf"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let _cleanup = Cleanup(directory);
    // Keep restrictive permissions after the private directory is removed and
    // the file becomes visible at its final destination.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|e| format!("cannot create temporary PDF: {e}"))?;
    let (width, height) = settings.page_size();
    let pdf = job.native.pdf(&temporary, width, height)?;
    for (i, page) in job.plan.pages.iter().enumerate() {
        settings.check_cancelled()?;
        pdf.page(page.width, page.height)?;
        draw(&job, i, pdf.context())?;
        pdf.end_page()?;
    }
    pdf.finish()?;
    settings.check_cancelled()?;
    std::fs::rename(&temporary, path)
        .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    Ok(job.plan.warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_and_orientation() {
        let mut s = Settings {
            paper: Paper::Letter,
            ..Default::default()
        };
        assert_eq!(s.page_size(), (612.0, 792.0));
        s.landscape = true;
        assert_eq!(s.page_size(), (792.0, 612.0));
        s.page_size_override = Some((300.0, 500.0));
        assert_eq!(s.page_size(), (300.0, 500.0));
    }

    #[test]
    fn pre_cancelled_job_does_not_load_or_write_output() {
        let settings = Settings {
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..Default::default()
        };
        let error = export_pdf(
            "cancelled",
            Path::new("/nonexistent-mdglass-test-directory/output.pdf"),
            &settings,
        )
        .unwrap_err();
        assert_eq!(error, "PDF export cancelled");
    }

    #[test]
    fn rejects_nonfinite_and_impossible_settings() {
        for x in [f64::NAN, f64::INFINITY, -1.0, 0.0, 100.0] {
            assert!(Settings {
                font_size: x,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        for x in [f64::NAN, f64::INFINITY, -1.0, 400.0] {
            assert!(Settings {
                margin: x,
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        assert!(Settings::default().validate().is_ok());
        assert!(Settings {
            margin: 0.0,
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(Settings {
            overlap: 300.0,
            ..Default::default()
        }
        .validate()
        .is_err());
        for dimensions in [
            (f64::NAN, 500.0),
            (500.0, f64::INFINITY),
            (0.0, 500.0),
            (500.0, 14401.0),
        ] {
            assert!(Settings {
                page_size_override: Some(dimensions),
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        assert!(Settings {
            margin: 12.0,
            overlap: 23.0,
            page_size_override: Some((72.0, 72.0)),
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(Settings {
            margin: 12.0,
            overlap: 24.0,
            page_size_override: Some((72.0, 72.0)),
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn paint_failure_preserves_destination_and_cleans_temporary_directory() {
        let settings = Settings::default();
        let job = match PrintJob::new("Original content", &settings) {
            Ok(job) => job,
            Err(e) if e.contains("cannot load") || e.contains("not available") => return,
            Err(e) => panic!("{e}"),
        };
        let dir = std::env::temp_dir().join(format!(
            "mdglass-output-failure-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let dest = dir.join("old.pdf");
        std::fs::write(&dest, b"original destination").unwrap();
        let result = save_pdf(job, &dest, &settings, |job, index, cr| {
            job.draw_page(index, cr)?;
            Err("injected painting failure".into())
        });
        assert!(result.unwrap_err().contains("injected painting failure"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"original destination");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let cancel = Arc::new(AtomicBool::new(false));
        let settings = Settings {
            cancel: Some(cancel.clone()),
            ..settings
        };
        let job = PrintJob::new("Cancelled content", &settings).unwrap();
        let result = save_pdf(job, &dest, &settings, |job, index, cr| {
            job.draw_page(index, cr)?;
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(result.unwrap_err(), "PDF export cancelled");
        assert_eq!(std::fs::read(&dest).unwrap(), b"original destination");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(dest).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
