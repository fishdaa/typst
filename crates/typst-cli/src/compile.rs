use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;

use chrono::{DateTime, Datelike, Timelike, Utc};
use ecow::eco_format;
use parking_lot::RwLock;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use typst::diag::{
    At, HintedStrResult, HintedString, SourceDiagnostic, SourceResult, StrResult, Warned,
    bail,
};
use typst::foundations::{Datetime, Smart};
use typst::layout::PageRanges;
use typst::syntax::Span;
use typst_bundle::{Bundle, BundleOptions, VirtualFs};
use typst_html::{HtmlDocument, HtmlOptions};
use typst_kit::diagnostics::DiagnosticWorld;
use typst_kit::timer::Timer;
use typst_layout::{Page, PagedDocument};
use typst_pdf::{PdfOptions, PdfStandards, Timestamp};
use typst_render::RenderOptions;
use typst_svg::SvgOptions;
use typst_utils::Scalar;

use crate::args::{
    CompileArgs, CompileCommand, DepsFormat, DiagnosticFormat, Input, Output,
    OutputFormat, PdfStandard, PngCompression, WatchCommand,
};
use crate::deps::write_deps;
use crate::watch::Status;
use crate::world::SystemWorld;
use crate::{set_failed, terminal};

#[cfg(feature = "http-server")]
use typst_kit::server::HttpServer;

/// Execute a compilation command.
pub fn compile(command: &'static CompileCommand) -> HintedStrResult<()> {
    let mut timer = Timer::new_or_placeholder(command.args.timings.clone());
    let mut config = CompileConfig::new(command)?;
    let mut world = SystemWorld::new(
        Some(&command.args.input),
        &command.args.world,
        &command.args.process,
    )
    .map_err(|err| eco_format!("{err}"))?;
    timer.record(&mut world, |world| compile_once(world, &mut config))?
}

/// A preprocessed `CompileCommand`.
pub struct CompileConfig {
    /// Static warnings to emit after compilation.
    pub warnings: Vec<HintedString>,
    /// Whether we are watching.
    pub watching: bool,
    /// Path to input Typst file or stdin.
    pub input: Input,
    /// Path to output file (PDF, PNG, SVG, or HTML).
    pub output: Output,
    /// The format of the output file.
    pub output_format: OutputFormat,
    /// Whether to make the serialized document pretty.
    pub pretty: bool,
    /// Which pages to export.
    pub pages: Option<PageRanges>,
    /// The document's creation date formatted as a UNIX timestamp, with UTC suffix.
    pub creation_timestamp: Option<DateTime<Utc>>,
    /// The format to emit diagnostics in.
    pub diagnostic_format: DiagnosticFormat,
    /// Opens the output file with the default viewer or a specific program after
    /// compilation.
    pub open: Option<Option<String>>,
    /// A list of standards the PDF should conform to.
    pub pdf_standards: PdfStandards,
    /// Whether to write PDF (accessibility) tags.
    pub tagged: bool,
    /// A destination to write a list of dependencies to.
    pub deps: Option<Output>,
    /// The format to use for dependencies.
    pub deps_format: DepsFormat,
    /// The PPI (pixels per inch) to use for PNG export.
    pub ppi: f64,
    /// The compression effort to use for PNG export.
    pub png_compression: PngCompression,
    /// Caps peak memory used while rendering a page to PNG, in mebibytes.
    /// `None` uses a fixed built-in budget. See `CompileArgs::max_memory`.
    pub max_memory: Option<u64>,
    /// The export cache for images, used for caching output files in `typst
    /// watch` sessions with images.
    pub export_cache: ExportCache,
    /// Server for `typst watch` to HTML.
    #[cfg(feature = "http-server")]
    pub server: Option<HttpServer>,
}

impl CompileConfig {
    /// Preprocess a `CompileCommand`, producing a compilation config.
    pub fn new(command: &CompileCommand) -> HintedStrResult<Self> {
        Self::new_impl(&command.args, None)
    }

    /// Preprocess a `WatchCommand`, producing a compilation config.
    pub fn watching(command: &WatchCommand) -> HintedStrResult<Self> {
        Self::new_impl(&command.args, Some(command))
    }

    /// The shared implementation of [`CompileConfig::new`] and
    /// [`CompileConfig::watching`].
    fn new_impl(
        args: &CompileArgs,
        watch: Option<&WatchCommand>,
    ) -> HintedStrResult<Self> {
        let mut warnings = Vec::new();
        let input = args.input.clone();

        let output_format = if let Some(specified) = args.format {
            specified
        } else if let Some(Output::Path(output)) = &args.output {
            match output.extension() {
                Some(ext) if ext.eq_ignore_ascii_case("pdf") => OutputFormat::Pdf,
                Some(ext) if ext.eq_ignore_ascii_case("png") => OutputFormat::Png,
                Some(ext) if ext.eq_ignore_ascii_case("svg") => OutputFormat::Svg,
                Some(ext) if ext.eq_ignore_ascii_case("html") => OutputFormat::Html,
                _ => bail!(
                    "could not infer output format for path {}.\n\
                     consider providing the format manually with `--format/-f`",
                    output.display(),
                ),
            }
        } else {
            OutputFormat::Pdf
        };

        let output = args.output.clone().unwrap_or_else(|| {
            let Input::Path(path) = &input else {
                panic!("output must be specified when input is from stdin, as guarded by the CLI");
            };
            Output::Path(path.with_extension(
                match output_format {
                    OutputFormat::Pdf => "pdf",
                    OutputFormat::Png => "png",
                    OutputFormat::Svg => "svg",
                    OutputFormat::Html => "html",
                    OutputFormat::Bundle => "",
                },
            ))
        });

        let pages = args.pages.as_ref().map(|export_ranges| {
            PageRanges::new(export_ranges.iter().map(|r| r.0.clone()).collect())
        });

        let tagged = !args.no_pdf_tags && pages.is_none();
        if output_format == OutputFormat::Pdf && pages.is_some() && !args.no_pdf_tags {
            warnings.push(
                HintedString::from("using --pages implies --no-pdf-tags").with_hints([
                    "the resulting PDF will be inaccessible".into(),
                    "add --no-pdf-tags to silence this warning".into(),
                ]),
            );
        }

        if !tagged {
            const ACCESSIBLE: &[(PdfStandard, &str)] = &[
                (PdfStandard::A_1a, "PDF/A-1a"),
                (PdfStandard::A_2a, "PDF/A-2a"),
                (PdfStandard::A_3a, "PDF/A-3a"),
                (PdfStandard::UA_1, "PDF/UA-1"),
            ];

            for (standard, name) in ACCESSIBLE {
                if args.pdf_standard.contains(standard) {
                    if args.no_pdf_tags {
                        bail!("cannot disable PDF tags when exporting a {name} document");
                    } else {
                        bail!(
                            "cannot disable PDF tags when exporting a {name} document";
                            hint: "using --pages implies --no-pdf-tags";
                        );
                    }
                }
            }
        }

        let pdf_standards = PdfStandards::new(
            &args.pdf_standard.iter().copied().map(Into::into).collect::<Vec<_>>(),
        )?;

        #[cfg(feature = "http-server")]
        let server = if let Some(command) = watch
            && !command.server.no_serve
            && matches!(output_format, OutputFormat::Html | OutputFormat::Bundle)
        {
            Some(HttpServer::new(
                &eco_format!("{input}"),
                command.server.port,
                !command.server.no_reload,
            )?)
        } else {
            None
        };

        let mut deps = args.deps.clone();
        let mut deps_format = args.deps_format;

        if let Some(path) = &args.make_deps
            && deps.is_none()
        {
            deps = Some(Output::Path(path.clone()));
            deps_format = DepsFormat::Make;
            warnings.push(
                "--make-deps is deprecated, use --deps and --deps-format instead".into(),
            );
        }

        match (&output, &deps, watch) {
            (Output::Stdout, _, Some(_)) => {
                bail!("cannot write document to stdout in watch mode");
            }
            (_, Some(Output::Stdout), Some(_)) => {
                bail!("cannot write dependencies to stdout in watch mode")
            }
            (Output::Stdout, Some(Output::Stdout), _) => {
                bail!("cannot write both output and dependencies to stdout")
            }
            _ => {}
        }

        Ok(Self {
            warnings,
            watching: watch.is_some(),
            input,
            output,
            output_format,
            pretty: args.pretty,
            pages,
            pdf_standards,
            tagged,
            creation_timestamp: args
                .world
                .creation_timestamp
                .map(|time| {
                    chrono::DateTime::from_timestamp(time, 0)
                        .ok_or("creation timestamp is out of range")
                })
                .transpose()?,
            ppi: args.ppi,
            png_compression: args.png_compression,
            max_memory: args.max_memory,
            diagnostic_format: args.process.diagnostic_format,
            open: args.open.clone(),
            export_cache: ExportCache::new(),
            deps,
            deps_format,
            #[cfg(feature = "http-server")]
            server,
        })
    }
}

/// Compile a single time.
///
/// Returns whether it compiled without errors.
#[typst_macros::time(name = "compile once")]
pub fn compile_once(
    world: &mut SystemWorld,
    config: &mut CompileConfig,
) -> HintedStrResult<()> {
    let start = std::time::Instant::now();
    if config.watching {
        Status::Compiling.print(config).unwrap();
    }

    let Warned { output, mut warnings } = compile_and_export(world, config);

    // Add static warnings (for deprecated CLI flags and such).
    for warning in config.warnings.iter() {
        warnings.push(
            SourceDiagnostic::warning(Span::detached(), warning.message())
                .with_hints(warning.hints().iter().map(Into::into)),
        );
    }

    match &output {
        // Print success message and possibly warnings.
        Ok(_) => {
            let duration = start.elapsed();
            if config.watching {
                if warnings.is_empty() {
                    Status::Success(duration).print(config).unwrap();
                } else {
                    Status::PartialSuccess(duration).print(config).unwrap();
                }
            }

            print_diagnostics(world, &[], &warnings, config.diagnostic_format)
                .map_err(|err| eco_format!("failed to print diagnostics ({err})"))?;

            open_output(config)?;
        }

        // Print failure message and diagnostics.
        Err(errors) => {
            set_failed();

            if config.watching {
                Status::Error.print(config).unwrap();
            }

            print_diagnostics(world, errors, &warnings, config.diagnostic_format)
                .map_err(|err| eco_format!("failed to print diagnostics ({err})"))?;
        }
    }

    if let Some(dest) = &config.deps {
        write_deps(world, dest, config.deps_format, output.as_deref().ok())
            .map_err(|err| eco_format!("failed to create dependency file ({err})"))?;
    }

    // Final sweep: PDF/HTML/bundle exports (unlike PNG, see
    // `export_image_page`) don't trim per-output, so do it once here for the
    // whole compile.
    trim_malloc_best_effort();

    Ok(())
}

/// Compile and then export the document.
fn compile_and_export(
    world: &mut SystemWorld,
    config: &mut CompileConfig,
) -> Warned<SourceResult<Vec<Output>>> {
    match config.output_format {
        OutputFormat::Pdf | OutputFormat::Png | OutputFormat::Svg => {
            let Warned { output, warnings } = typst::compile::<PagedDocument>(world);
            let result = output.and_then(|document| export_paged(&document, config));
            Warned { output: result, warnings }
        }
        OutputFormat::Html => {
            let Warned { output, warnings } = typst::compile::<HtmlDocument>(world);
            let result = output.and_then(|document| export_html(&document, config));
            Warned {
                output: result.map(|()| vec![config.output.clone()]),
                warnings,
            }
        }
        OutputFormat::Bundle => {
            let Warned { output, warnings } = typst::compile::<Bundle>(world);
            let result = output.and_then(|bundle| export_bundle(bundle, config));
            Warned { output: result, warnings }
        }
    }
}

/// Export to HTML.
fn export_html(document: &HtmlDocument, config: &CompileConfig) -> SourceResult<()> {
    let options = HtmlOptions { pretty: config.pretty };
    let html = typst_html::html(document, &options)?;
    let result = config.output.write(html.as_bytes());

    #[cfg(feature = "http-server")]
    if let Some(server) = &config.server {
        server.set_html(html);
    }

    result
        .map_err(|err| eco_format!("failed to write HTML file ({err})"))
        .at(Span::detached())
}

/// Export to a paged target format.
fn export_paged(
    document: &PagedDocument,
    config: &CompileConfig,
) -> SourceResult<Vec<Output>> {
    match config.output_format {
        OutputFormat::Pdf => {
            export_pdf(document, config).map(|()| vec![config.output.clone()])
        }
        OutputFormat::Png => {
            export_image(document, config, ImageExportFormat::Png).at(Span::detached())
        }
        OutputFormat::Svg => {
            export_image(document, config, ImageExportFormat::Svg).at(Span::detached())
        }
        OutputFormat::Html | OutputFormat::Bundle => unreachable!(),
    }
}

/// Export to a PDF.
fn export_pdf(document: &PagedDocument, config: &CompileConfig) -> SourceResult<()> {
    let options = pdf_options(config);
    let buffer = typst_pdf::pdf(document, &options)?;
    config
        .output
        .write(&buffer)
        .map_err(|err| eco_format!("failed to write PDF file ({err})"))
        .at(Span::detached())?;
    Ok(())
}

/// Export to a bundle, a collection of files in a directory.
fn export_bundle(bundle: Bundle, config: &CompileConfig) -> SourceResult<Vec<Output>> {
    let options = BundleOptions {
        html: html_options(config),
        pdf: pdf_options(config),
        png: png_options(config),
        svg: svg_options(config),
    };

    let fs = typst_bundle::export(&bundle, &options)?;
    let root = match &config.output {
        Output::Path(path) => path,
        Output::Stdout => {
            bail!(Span::detached(), "cannot write bundle to standard output")
        }
    };

    let outputs = write_virtual_fs(root, &fs).at(Span::detached())?;

    #[cfg(feature = "http-server")]
    if let Some(server) = &config.server {
        server.set_bundle(bundle, fs);
    }

    Ok(outputs)
}

/// Writes a bundle's files to disk.
fn write_virtual_fs(root: &Path, fs: &VirtualFs) -> StrResult<Vec<Output>> {
    std::fs::create_dir_all(root)
        .map_err(|err| eco_format!("failed to create output directory ({err})"))?;

    fs.par_iter()
        .map(|(path, data)| {
            let realized = path
                .realize(root)
                .map_err(|err| eco_format!("failed to realize path ({err})"))?;

            if let Some(parent) = realized.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| eco_format!("failed to create directory ({err})"))?;
            }

            std::fs::write(&realized, data)
                .map_err(|err| eco_format!("failed to write file ({err})"))?;
            Ok(Output::Path(realized))
        })
        .collect()
}

/// Convert [`chrono::DateTime`] to [`Datetime`]
fn convert_datetime<Tz: chrono::TimeZone>(
    date_time: chrono::DateTime<Tz>,
) -> Option<Datetime> {
    Datetime::from_ymd_hms(
        date_time.year(),
        date_time.month().try_into().ok()?,
        date_time.day().try_into().ok()?,
        date_time.hour().try_into().ok()?,
        date_time.minute().try_into().ok()?,
        date_time.second().try_into().ok()?,
    )
}

/// An image format to export in.
#[derive(Copy, Clone)]
enum ImageExportFormat {
    Png,
    Svg,
}

/// Export to one or multiple images.
fn export_image(
    document: &PagedDocument,
    config: &CompileConfig,
    fmt: ImageExportFormat,
) -> StrResult<Vec<Output>> {
    // Determine whether we have indexable templates in output
    let can_handle_multiple = match config.output {
        Output::Stdout => false,
        Output::Path(ref output) => {
            output_template::has_indexable_template(output.to_str().unwrap_or_default())
        }
    };

    let exported_pages = document
        .pages()
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            config.pages.as_ref().is_none_or(|exported_page_ranges| {
                exported_page_ranges.includes_page_index(*i)
            })
        })
        .collect::<Vec<_>>();

    if !can_handle_multiple && exported_pages.len() > 1 {
        let err = match config.output {
            Output::Stdout => "to stdout",
            Output::Path(_) => {
                "without a page number template ({p}, {0p}) in the output path"
            }
        };
        bail!("cannot export multiple images {err}");
    }

    // `exported_pages.par_iter()` below can have this many pages mid-band
    // simultaneously, so `--max-memory`'s per-page budget (`band_budget`)
    // must be divided across it to keep the flag's cap meaningful for
    // multi-page documents. `current_num_threads()` reflects `-j`/`--jobs`
    // (set via `rayon::ThreadPoolBuilder::build_global` in
    // `SystemWorld::new`) or the CPU count if unset.
    let concurrency = rayon::current_num_threads().min(exported_pages.len().max(1));

    // The results are collected in a `Vec<()>` which does not allocate.
    exported_pages
        .par_iter()
        .map(|(i, page)| {
            // Use output with converted path.
            let output = match &config.output {
                Output::Path(path) => {
                    let storage;
                    let path = if can_handle_multiple {
                        storage = output_template::format(
                            path.to_str().unwrap_or_default(),
                            i + 1,
                            document.pages().len(),
                        );
                        Path::new(&storage)
                    } else {
                        path
                    };

                    // If we are not watching, don't use the cache.
                    // If the frame is in the cache, skip it.
                    // If the file does not exist, always create it.
                    if config.watching
                        && config.export_cache.is_cached(*i, page)
                        && path.exists()
                    {
                        return Ok(Output::Path(path.to_path_buf()));
                    }

                    Output::Path(path.to_owned())
                }
                Output::Stdout => Output::Stdout,
            };

            export_image_page(config, page, concurrency, &output, fmt)?;
            Ok(output)
        })
        .collect::<StrResult<Vec<Output>>>()
}

mod output_template {
    const INDEXABLE: [&str; 3] = ["{p}", "{0p}", "{n}"];

    pub fn has_indexable_template(output: &str) -> bool {
        INDEXABLE.iter().any(|template| output.contains(template))
    }

    pub fn format(output: &str, this_page: usize, total_pages: usize) -> String {
        // Find the base 10 width of number `i`
        fn width(i: usize) -> usize {
            1 + i.checked_ilog10().unwrap_or(0) as usize
        }

        let other_templates = ["{t}"];
        INDEXABLE.iter().chain(other_templates.iter()).fold(
            output.to_string(),
            |out, template| {
                let replacement = match *template {
                    "{p}" => format!("{this_page}"),
                    "{0p}" | "{n}" => format!("{:01$}", this_page, width(total_pages)),
                    "{t}" => format!("{total_pages}"),
                    _ => unreachable!("unhandled template placeholder {template}"),
                };
                out.replace(template, replacement.as_str())
            },
        )
    }
}

/// Export single image.
fn export_image_page(
    config: &CompileConfig,
    page: &Page,
    concurrency: usize,
    output: &Output,
    fmt: ImageExportFormat,
) -> StrResult<()> {
    let result = match fmt {
        ImageExportFormat::Png => {
            let options = png_options(config);
            render_and_encode_png_in_bands(
                page,
                &options,
                config.png_compression,
                config.max_memory,
                concurrency,
                output,
            )
            .map_err(|err| eco_format!("failed to encode PNG file ({err})"))
        }
        ImageExportFormat::Svg => {
            let options = svg_options(config);
            let svg = typst_svg::svg(page, &options);
            output
                .write(svg.as_bytes())
                .map_err(|err| eco_format!("failed to write SVG file ({err})"))
        }
    };

    // Give the large transient band/texture buffers this page's export just
    // freed back to the OS now, rather than leaving them on glibc's
    // per-thread free list -- see `main::limit_malloc_arenas` for why this
    // matters most on a warm-reused (e.g. serverless) process. Run
    // regardless of success or failure, and per-page rather than only once
    // for the whole document, since `exported_pages.par_iter()` may still
    // have other pages in flight on other threads.
    trim_malloc_best_effort();

    result
}

/// Best-effort hint to glibc to release any memory on its free lists back to
/// the OS. See `main::limit_malloc_arenas` for the rationale. Linux+glibc
/// only (`malloc_trim` isn't available on musl or other platforms) and never
/// required for correctness, so a no-op elsewhere.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_malloc_best_effort() {
    // SAFETY: `malloc_trim` only returns free heap memory to the OS; it
    // doesn't affect Rust-level memory safety and is safe to call from any
    // thread at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_malloc_best_effort() {}

/// The default per-band byte budget and page-cache eviction interval, used
/// when `--max-memory` isn't given. Tuned for typical documents: e.g. at a
/// poster's width (~7200px) `DEFAULT_MAX_BAND_BYTES` yields bands of roughly
/// 500 rows.
const DEFAULT_MAX_BAND_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_EVICT_CHUNK_BYTES: u64 = 32 * 1024 * 1024;

/// A rough allowance for peak memory `--max-memory` doesn't control: the
/// in-memory document model, font/glyph caches, and other per-process
/// overhead that exists regardless of how small banding is made. Without
/// this, a tight `--max-memory` value would ask for band/eviction sizes far
/// below what's actually achievable, without making the result any smaller.
const BASE_OVERHEAD_BYTES: u64 = 64 * 1024 * 1024;

/// Derives the per-band byte budget and page-cache eviction interval (see
/// `EvictingFileWriter`) from a user-specified memory cap in mebibytes, so
/// the same flag value scales both to fit *any* document -- rather than
/// hardcoding a band size that happens to work for one particular page size
/// or asset resolution. Falls back to fixed defaults when no cap is given.
///
/// `concurrency` is the number of pages that may be mid-export at once (see
/// `export_image`'s `rayon::current_num_threads()`-derived value): the cap
/// bounds *total* process memory, but `exported_pages.par_iter()` can have
/// that many pages banding/encoding simultaneously, so each gets only
/// `1 / concurrency` of the post-overhead budget. Without this, `--max-memory
/// 512` on a 4-page document exported with `-j 4` would let 4 pages each use
/// up to ~512 MiB worth of bands at once, breaking the cap by ~4x.
///
/// This is a heuristic, not an exact guarantee -- see `BASE_OVERHEAD_BYTES`.
/// A band's raw canvas, its demultiplied copy, and the source image's
/// row-range decode buffer (`RasterImage::decode_rgba_row_range`) can all be
/// alive at once, so the remaining per-worker budget is divided across
/// roughly that many same-order buffers, plus headroom for the eviction
/// interval.
fn band_budget(max_memory_mib: Option<u64>, concurrency: usize) -> (usize, u64) {
    let Some(mib) = max_memory_mib else {
        return (DEFAULT_MAX_BAND_BYTES, DEFAULT_EVICT_CHUNK_BYTES);
    };
    let budget = mib.saturating_mul(1024 * 1024).saturating_sub(BASE_OVERHEAD_BYTES);
    let per_worker = budget / (concurrency.max(1) as u64);
    let band_bytes = ((per_worker / 6) as usize).max(4096);
    let evict_bytes = (per_worker / 4).clamp(1024 * 1024, 128 * 1024 * 1024);
    (band_bytes, evict_bytes)
}

/// Wraps a plain file (not stdout, which may be a pipe or terminal rather
/// than a normal file) so that once `evict_chunk_bytes` have been written,
/// they're synced to disk and the OS is told to drop them from the page
/// cache.
///
/// Without this, writing a large encoded PNG (a high-DPI poster can run to
/// hundreds of megabytes or more) leaves all of it resident as dirty (then
/// clean) page cache by the time the export finishes. A memory-constrained
/// cgroup charges page cache the same as heap memory, so that cache would
/// otherwise dominate peak memory regardless of how small
/// `render_and_encode_png_in_bands` keeps the actual render/encode buffers.
struct EvictingFileWriter {
    file: std::fs::File,
    evict_chunk_bytes: u64,
    written: u64,
    evicted: u64,
}

impl EvictingFileWriter {
    fn create(path: &Path, evict_chunk_bytes: u64) -> io::Result<Self> {
        Ok(Self {
            file: std::fs::File::create(path)?,
            evict_chunk_bytes,
            written: 0,
            evicted: 0,
        })
    }

    /// Syncs and evicts everything written so far. Best-effort: an error
    /// (or running on a platform without `posix_fadvise`) just leaves the
    /// data cached, which is the pre-existing behavior, not a correctness
    /// problem.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn evict_written(&mut self) {
        use std::os::unix::io::AsRawFd;

        if self.file.sync_data().is_err() {
            return;
        }

        // SAFETY: `self.file` is a valid, open file descriptor for the
        // duration of this call. `posix_fadvise` only affects the OS page
        // cache, never the file's contents or Rust-level memory safety.
        unsafe {
            libc::posix_fadvise(
                self.file.as_raw_fd(),
                0,
                self.written as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            );
        }
        self.evicted = self.written;
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn evict_written(&mut self) {}
}

impl Write for EvictingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.written += n as u64;
        if self.written - self.evicted >= self.evict_chunk_bytes {
            self.evict_written();
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Renders a page and encodes it as a PNG, honoring the configured
/// compression effort, without ever materializing the full-page canvas --
/// or the full encoded output -- in memory at once.
///
/// This reimplements `tiny_skia::Pixmap::encode_png` instead of calling it
/// directly for two reasons: that method hardcodes the `png` crate's default
/// compression (`Balanced`, slow for very large pages), and it always
/// operates on a whole `Pixmap`, which for a large page (e.g. a big poster)
/// means holding the whole rendered canvas in memory. Instead, render and
/// stream out one horizontal band at a time via `typst_render::render_band`,
/// so peak memory is bounded by a single band rather than the full page.
///
/// The encoded bytes are streamed straight into `output` rather than
/// collected into an in-memory buffer first: for a large enough page (e.g. a
/// high-DPI poster), the encoded PNG itself can be hundreds of megabytes to
/// low gigabytes, which would otherwise dominate peak memory regardless of
/// how small the per-band canvas is kept.
fn render_and_encode_png_in_bands(
    page: &Page,
    opts: &RenderOptions,
    compression: PngCompression,
    max_memory_mib: Option<u64>,
    concurrency: usize,
    output: &Output,
) -> Result<(), png::EncodingError> {
    let (band_bytes, evict_bytes) = band_budget(max_memory_mib, concurrency);

    // A plain file gets the page-cache-evicting writer (see
    // `EvictingFileWriter`); stdout may be a pipe or terminal rather than a
    // regular file, so it's written as-is.
    match output {
        Output::Path(path) => encode_bands(
            page,
            opts,
            compression,
            band_bytes,
            EvictingFileWriter::create(path, evict_bytes)?,
        ),
        Output::Stdout => encode_bands(page, opts, compression, band_bytes, output.open()?),
    }
}

/// Does the actual banded render + PNG encode into `out`, shared between
/// [`render_and_encode_png_in_bands`]'s file and stdout cases.
fn encode_bands(
    page: &Page,
    opts: &RenderOptions,
    compression: PngCompression,
    max_band_bytes: usize,
    mut out: impl Write,
) -> Result<(), png::EncodingError> {
    let (width, height) = typst_render::pixel_dimensions(page, opts);
    let row_bytes = (width as usize).saturating_mul(4).max(1);
    let band_rows = if typst_render::uses_relative_paint(page) {
        // See `uses_relative_paint`: banding a gradient/pattern can shift its
        // colors slightly due to f32 precision loss, so render such pages as
        // a single band (the whole page), matching un-banded behavior.
        height.max(1)
    } else {
        ((max_band_bytes / row_bytes) as u32).clamp(1, height.max(1))
    };

    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(compression.into());
    let mut writer = encoder.write_header()?;
    let mut stream = writer.stream_writer()?;

    let mut y = 0;
    while y < height {
        let band_height = band_rows.min(height - y);
        let band = typst_render::render_band(page, opts, y, band_height);
        let demultiplied_data = band.take_demultiplied();
        stream.write_all(&demultiplied_data)?;
        y += band_height;
    }

    stream.finish()?;

    Ok(())
}

/// Creates options for HTML export.
fn html_options(config: &CompileConfig) -> HtmlOptions {
    HtmlOptions { pretty: config.pretty }
}

/// Creates options for PDF export.
fn pdf_options(config: &CompileConfig) -> PdfOptions {
    // If the timestamp is provided through the CLI, use UTC suffix,
    // else, use the current local time and timezone.
    let timestamp = match config.creation_timestamp {
        Some(timestamp) => convert_datetime(timestamp).map(Timestamp::new_utc),
        None => {
            let local_datetime = chrono::Local::now();
            convert_datetime(local_datetime).and_then(|datetime| {
                Timestamp::new_local(
                    datetime,
                    local_datetime.offset().local_minus_utc() / 60,
                )
            })
        }
    };

    PdfOptions {
        ident: Smart::Auto,
        creator: Smart::Auto,
        timestamp,
        page_ranges: config.pages.clone(),
        standards: config.pdf_standards.clone(),
        tagged: config.tagged,
        pretty: config.pretty,
    }
}

/// Creates options for SVG export.
fn svg_options(config: &CompileConfig) -> SvgOptions {
    SvgOptions { render_bleed: false, pretty: config.pretty }
}

/// Creates options for PNG export.
fn png_options(config: &CompileConfig) -> RenderOptions {
    RenderOptions {
        pixel_per_pt: Scalar::new(config.ppi / 72.0),
        render_bleed: false,
    }
}

/// Caches exported files so that we can avoid re-exporting them if they haven't
/// changed.
///
/// This is done by having a list of size `files.len()` that contains the hashes
/// of the last rendered frame in each file. If a new frame is inserted, this
/// will invalidate the rest of the cache, this is deliberate as to decrease the
/// complexity and memory usage of such a cache.
pub struct ExportCache {
    /// The hashes of last compilation's frames.
    pub cache: RwLock<Vec<u128>>,
}

impl ExportCache {
    /// Creates a new export cache.
    pub fn new() -> Self {
        Self { cache: RwLock::new(Vec::with_capacity(32)) }
    }

    /// Returns true if the entry is cached and appends the new hash to the
    /// cache (for the next compilation).
    pub fn is_cached(&self, i: usize, page: &Page) -> bool {
        let hash = typst::utils::hash128(page);

        let mut cache = self.cache.upgradable_read();
        if i >= cache.len() {
            cache.with_upgraded(|cache| cache.push(hash));
            return false;
        }

        cache.with_upgraded(|cache| std::mem::replace(&mut cache[i], hash) == hash)
    }
}

/// Opens the output if desired.
fn open_output(config: &mut CompileConfig) -> StrResult<()> {
    let Some(viewer) = config.open.take() else { return Ok(()) };

    #[cfg(feature = "http-server")]
    if let Some(server) = &config.server {
        let url = format!("http://{}", server.addr());
        return open_path(OsStr::new(&url), viewer.as_deref());
    }

    // Can't open stdout.
    let Output::Path(path) = &config.output else { return Ok(()) };

    // Some resource openers require the path to be canonicalized.
    let path = path
        .canonicalize()
        .map_err(|err| eco_format!("failed to canonicalize path ({err})"))?;

    open_path(path.as_os_str(), viewer.as_deref())
}

/// Opens the given file using:
///
/// - The default file viewer if `app` is `None`.
/// - The given viewer provided by `app` if it is `Some`.
fn open_path(path: &OsStr, viewer: Option<&str>) -> StrResult<()> {
    if let Some(viewer) = viewer {
        open::with_detached(path, viewer)
            .map_err(|err| eco_format!("failed to open file with {viewer} ({err})"))
    } else {
        open::that_detached(path).map_err(|err| {
            let openers = open::commands(path)
                .iter()
                .map(|command| command.get_program().to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ");
            eco_format!(
                "failed to open file with any of these resource openers: {openers} \
                 ({err})",
            )
        })
    }
}

/// Print diagnostic messages to the terminal.
pub fn print_diagnostics(
    world: &dyn DiagnosticWorld,
    errors: &[SourceDiagnostic],
    warnings: &[SourceDiagnostic],
    format: DiagnosticFormat,
) -> Result<(), codespan_reporting::files::Error> {
    typst_kit::diagnostics::emit(
        &mut terminal::out(),
        world,
        errors.iter().chain(warnings),
        match format {
            DiagnosticFormat::Human => typst_kit::diagnostics::DiagnosticFormat::Human,
            DiagnosticFormat::Short => typst_kit::diagnostics::DiagnosticFormat::Short,
        },
    )
}

impl From<PdfStandard> for typst_pdf::PdfStandard {
    fn from(standard: PdfStandard) -> Self {
        match standard {
            PdfStandard::V_1_4 => typst_pdf::PdfStandard::V_1_4,
            PdfStandard::V_1_5 => typst_pdf::PdfStandard::V_1_5,
            PdfStandard::V_1_6 => typst_pdf::PdfStandard::V_1_6,
            PdfStandard::V_1_7 => typst_pdf::PdfStandard::V_1_7,
            PdfStandard::V_2_0 => typst_pdf::PdfStandard::V_2_0,
            PdfStandard::A_1b => typst_pdf::PdfStandard::A_1b,
            PdfStandard::A_1a => typst_pdf::PdfStandard::A_1a,
            PdfStandard::A_2b => typst_pdf::PdfStandard::A_2b,
            PdfStandard::A_2u => typst_pdf::PdfStandard::A_2u,
            PdfStandard::A_2a => typst_pdf::PdfStandard::A_2a,
            PdfStandard::A_3b => typst_pdf::PdfStandard::A_3b,
            PdfStandard::A_3u => typst_pdf::PdfStandard::A_3u,
            PdfStandard::A_3a => typst_pdf::PdfStandard::A_3a,
            PdfStandard::A_4 => typst_pdf::PdfStandard::A_4,
            PdfStandard::A_4f => typst_pdf::PdfStandard::A_4f,
            PdfStandard::A_4e => typst_pdf::PdfStandard::A_4e,
            PdfStandard::UA_1 => typst_pdf::PdfStandard::Ua_1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `band_budget` derives byte budgets purely from the memory cap, not
    /// from any page/asset dimensions, so the same `--max-memory` value
    /// scales down banding for a huge poster exactly like it would for a
    /// tiny page: `band_rows = band_bytes / row_bytes` then adapts to
    /// whatever `row_bytes` (i.e. page width) turns out to be.
    #[test]
    fn test_band_budget_scales_with_cap_not_content() {
        let (default_band, default_evict) = band_budget(None, 1);
        assert_eq!(default_band, DEFAULT_MAX_BAND_BYTES);
        assert_eq!(default_evict, DEFAULT_EVICT_CHUNK_BYTES);

        let (small_band, small_evict) = band_budget(Some(128), 1);
        let (large_band, large_evict) = band_budget(Some(2048), 1);
        assert!(small_band < large_band, "{small_band} should be < {large_band}");
        assert!(small_evict < large_evict, "{small_evict} should be < {large_evict}");

        // A cap at or below the base overhead allowance still yields a
        // usable (if minimal) band -- at least one row -- rather than
        // zero/underflowing.
        let (floor_band, floor_evict) = band_budget(Some(1), 1);
        assert!(floor_band > 0);
        assert!(floor_evict > 0);
    }

    /// A cap unset by `--jobs` still bounds *total* memory when multiple
    /// pages are exported concurrently: each concurrent worker must get a
    /// proportionally smaller slice of the same overall cap.
    #[test]
    fn test_band_budget_scales_with_concurrency() {
        let (band_1, evict_1) = band_budget(Some(512), 1);
        let (band_4, evict_4) = band_budget(Some(512), 4);
        assert!(band_4 < band_1, "{band_4} should be < {band_1}");
        assert!(evict_4 < evict_1, "{evict_4} should be < {evict_1}");
        // Roughly a 4x reduction (integer division, so allow some slack).
        assert!(band_1 / band_4 >= 3, "expected ~4x smaller, got {band_1}/{band_4}");

        // `concurrency == 0` is treated the same as `1` (never divide by
        // zero / hand out an unbounded budget).
        assert_eq!(band_budget(Some(512), 0), band_budget(Some(512), 1));

        // `None` (no cap requested) is unaffected by concurrency.
        assert_eq!(band_budget(None, 4), band_budget(None, 1));
    }
}
