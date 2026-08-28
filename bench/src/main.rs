//! Reproducible large-asset memory/time benchmark comparing two Typst
//! binaries. See `README.md` in this directory for usage.

use std::error::Error;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!(
            "usage: typst-bench BASELINE_TYPST TYPST [TYPST ...]\n\
             \n\
             Each argument is a path to a Typst binary, optionally as\n\
             LABEL=PATH. The first one is the baseline (assumed to be\n\
             upstream): scenarios that use fork-only flags are skipped for\n\
             it and run for every later binary."
        );
        std::process::exit(2);
    }

    let binaries: Vec<(String, String)> = args
        .iter()
        .enumerate()
        .map(|(index, arg)| match arg.split_once('=') {
            Some((label, path)) => (label.to_string(), path.to_string()),
            None if index == 0 => ("baseline".to_string(), arg.clone()),
            None => (format!("binary{}", index + 1), arg.clone()),
        })
        .collect();

    let warmup = env_u32("TYPST_BENCH_WARMUP", 1);
    let runs = env_u32("TYPST_BENCH_RUNS", 5);
    let size = std::env::var("TYPST_BENCH_SIZE").unwrap_or_else(|_| "7200x18000".into());
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .ok_or("TYPST_BENCH_SIZE must look like WIDTHxHEIGHT")?;

    let dir = tempfile::Builder::new().prefix("typst-large-bench").tempdir()?;
    let root = dir.path();
    write_fixtures(root, width, height)?;

    let mut rows = Vec::new();
    for (index, (name, binary)) in binaries.iter().enumerate() {
        for scenario in SCENARIOS {
            if scenario.fork_only && index == 0 {
                continue;
            }
            rows.push(report(root, name, binary, scenario, warmup, runs)?);
        }
    }

    print_table(&rows);
    Ok(())
}

/// One measured configuration.
struct Scenario {
    /// The row label.
    label: &'static str,
    /// Which `.typ` fixture to compile.
    fixture: &'static str,
    /// The `--ppi` to compile at.
    ///
    /// A page `N` points wide at 72 ppi is exactly `N` device pixels, so a
    /// full-bleed asset of the same pixel size lands at its native
    /// resolution and needs no resampling. Any other value puts the same
    /// asset through the resampling path instead, which is both the more
    /// realistic case (a poster's assets rarely match the output grid
    /// exactly) and a materially different code path, so it is measured
    /// separately.
    ppi: &'static str,
    /// Extra CLI arguments.
    extra: &'static [&'static str],
    /// Whether this scenario uses flags only the fork has, and so must be
    /// skipped for the baseline binary.
    fork_only: bool,
}

const SCENARIOS: &[Scenario] = &[
    // Background alone, at native resolution: the simplest large-asset case.
    Scenario {
        label: "opaque",
        fixture: "opaque",
        ppi: "72",
        extra: &[],
        fork_only: false,
    },
    Scenario {
        label: "alpha",
        fixture: "alpha",
        ppi: "72",
        extra: &[],
        fork_only: false,
    },
    Scenario {
        label: "svg",
        fixture: "svg",
        ppi: "72",
        extra: &[],
        fork_only: false,
    },
    // The shape real documents have: a full-bleed background with text and
    // data drawn on top of it.
    Scenario {
        label: "poster",
        fixture: "poster",
        ppi: "72",
        extra: &[],
        fork_only: false,
    },
    // The same poster with the background resampled rather than blitted at
    // native resolution -- see `Scenario::ppi`.
    Scenario {
        label: "poster-scaled",
        fixture: "poster",
        ppi: "71",
        extra: &[],
        fork_only: false,
    },
    // The render-thread sweep: `--render-threads 1` is the strictly
    // sequential render-then-encode loop this fork used to have, so it is the
    // reference point for what tiling and the render/encode pipeline buy. The
    // plain `poster` rows above use the flag's default.
    Scenario {
        label: "poster-rt1",
        fixture: "poster",
        ppi: "72",
        extra: &["--render-threads", "1"],
        fork_only: true,
    },
    Scenario {
        label: "poster-rt8",
        fixture: "poster",
        ppi: "72",
        extra: &["--render-threads", "8"],
        fork_only: true,
    },
    Scenario {
        label: "poster-scaled-rt1",
        fixture: "poster",
        ppi: "71",
        extra: &["--render-threads", "1"],
        fork_only: true,
    },
    Scenario {
        label: "poster-scaled-rt8",
        fixture: "poster",
        ppi: "71",
        extra: &["--render-threads", "8"],
        fork_only: true,
    },
    Scenario {
        label: "constrained",
        fixture: "poster",
        ppi: "71",
        extra: &["--max-memory", "512"],
        fork_only: true,
    },
    // Isolates the memory/time win of the rendering path from the effect of
    // the fork's default `--png-compression fast`: same fixture as
    // `opaque`, re-encoded at `balanced`, which is the effort the `png`
    // crate (and so the baseline) uses by default. Without this row, part of
    // `opaque`'s time gap is just the cheaper compression default.
    Scenario {
        label: "balanced",
        fixture: "opaque",
        ppi: "72",
        extra: &["--png-compression", "balanced"],
        fork_only: true,
    },
];

struct Row {
    binary: String,
    scenario: String,
    mean_secs: f64,
    stddev_secs: f64,
    max_rss_kib: u64,
    output_bytes: u64,
}

fn print_table(rows: &[Row]) {
    let headers = ["BINARY", "SCENARIO", "TIME", "PEAK RSS", "OUTPUT"];
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            [
                r.binary.clone(),
                r.scenario.clone(),
                fmt_time(r.mean_secs, r.stddev_secs),
                fmt_size(r.max_rss_kib * 1024),
                fmt_size(r.output_bytes),
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &cells {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }

    let print_row = |cols: &[&str]| {
        let padded: Vec<String> =
            cols.iter().zip(widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", padded.join("  ").trim_end());
    };
    print_row(&headers);
    for row in &cells {
        print_row(&[&row[0], &row[1], &row[2], &row[3], &row[4]]);
    }
}

fn fmt_time(mean_secs: f64, stddev_secs: f64) -> String {
    if mean_secs < 1.0 {
        format!("{:.2} ms ± {:.2} ms", mean_secs * 1000.0, stddev_secs * 1000.0)
    } else {
        format!("{mean_secs:.3} s ± {stddev_secs:.3} s")
    }
}

fn fmt_size(bytes: u64) -> String {
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        format!("{kib:.1} KiB")
    } else {
        format!("{:.1} MiB", kib / 1024.0)
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn write_fixtures(root: &Path, width: u32, height: u32) -> Result<(), Box<dyn Error>> {
    write_png(&root.join("opaque.png"), width, height, false)?;
    write_png(&root.join("alpha.png"), width, height, true)?;
    fs::write(
        root.join("asset.svg"),
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}"><rect width="100%" height="100%" fill="#306090"/><circle cx="50%" cy="50%" r="20%" fill="#f0c040"/></svg>"##
        ),
    )?;
    for (scenario, asset) in
        [("opaque", "opaque.png"), ("alpha", "alpha.png"), ("svg", "asset.svg")]
    {
        fs::write(
            root.join(format!("{scenario}.typ")),
            format!(
                "#set page(width: {width}pt, height: {height}pt, margin: 0pt)\n#image(\"{asset}\", width: 100%, height: 100%)\n"
            ),
        )?;
    }

    // Report what was generated: the asset's compression ratio decides how
    // much of the measured time is real inflate/deflate work, so it belongs
    // in the run's own output rather than only in the harness's source.
    for name in ["opaque.png", "alpha.png"] {
        let path = root.join(name);
        let bytes = fs::metadata(&path)?.len();
        let channels = if name.starts_with("alpha") { 4 } else { 3 };
        let raw = width as u64 * height as u64 * channels;
        eprintln!(
            "fixture {name}: {} ({:.2}x compression of {})",
            fmt_size(bytes),
            raw as f64 / bytes.max(1) as f64,
            fmt_size(raw),
        );
    }

    // A full-bleed background with text and data drawn over it, which is
    // what documents in this shape actually look like -- and which makes the
    // per-band cost of re-walking the page's contents visible, where a
    // background-only fixture hides it.
    let rows = (height / 40).clamp(1, 400);
    let mut poster = format!(
        "#set page(width: {width}pt, height: {height}pt, margin: 0pt)\n\
         #place(top + left, image(\"opaque.png\", width: 100%, height: 100%))\n"
    );
    for row in 0..rows {
        let dy = row * (height / rows.max(1));
        let value = (row * 137) % 991;
        poster.push_str(&format!(
            "#place(top + left, dx: 64pt, dy: {dy}pt, \
             text(size: 24pt, fill: white)[Row {row} -- measured value {value}])\n"
        ));
    }
    fs::write(root.join("poster.typ"), poster)?;

    Ok(())
}

/// Streams rows straight into the PNG encoder instead of building a
/// full-size buffer first, since the benchmark's own point is exercising the
/// bounded-memory path rather than defeating it while generating fixtures.
///
/// The content is built to compress roughly like a photograph -- about
/// three-fold at the default size, in the range a PNG of real photographic
/// content achieves. That matters in both directions: a flat-color fixture
/// compresses several hundred-fold, making both inflating the source and
/// deflating the output nearly free, while pure per-pixel noise is
/// incompressible and makes them nearly memcpy-cheap instead. Either way the
/// compression work a large-asset benchmark exists to measure disappears. So
/// the content here is a smooth gradient plus *spatially coherent* noise
/// (one value per 8x8 block, and a small dither on top), which is close to
/// what PNG's row filters see in a photograph.
fn write_png(
    path: &Path,
    width: u32,
    height: u32,
    alpha: bool,
) -> Result<(), Box<dyn Error>> {
    let file = fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_color(if alpha { png::ColorType::Rgba } else { png::ColorType::Rgb });
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?.into_stream_writer()?;

    let channels = if alpha { 4 } else { 3 };
    let mut row = vec![0_u8; width as usize * channels];
    for y in 0..height {
        for x in 0..width {
            // A deterministic hash of the coarse block coordinates, so the
            // noise is coherent over 8x8 pixel blocks rather than
            // independent per pixel.
            let block = hash32(x / 8, y / 8);
            let coarse = (block % 33) as i32 - 16;
            let dither = (hash32(x, y) % 3) as i32 - 1;

            let base_r = (x * 255 / width.max(1)) as i32;
            let base_g = (y * 255 / height.max(1)) as i32;
            let base_b = 128 + (base_r - base_g) / 3;

            let offset = x as usize * channels;
            row[offset] = (base_r + coarse + dither).clamp(0, 255) as u8;
            row[offset + 1] = (base_g + coarse - dither).clamp(0, 255) as u8;
            row[offset + 2] = (base_b + coarse).clamp(0, 255) as u8;
            if alpha {
                row[offset + 3] = 0xff;
            }
        }
        writer.write_all(&row)?;
    }
    writer.finish()?;
    Ok(())
}

/// A small deterministic integer hash (a 2D variant of the finalizer from
/// `MurmurHash3`), used to make fixture noise reproducible across runs and
/// machines without pulling in a random-number generator.
fn hash32(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x85eb_ca6b) ^ y.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^ (h >> 16)
}

struct Sample {
    elapsed_secs: f64,
    max_rss_kib: u64,
}

fn report(
    root: &Path,
    binary_name: &str,
    binary: &str,
    scenario: &Scenario,
    warmup: u32,
    runs: u32,
) -> Result<Row, Box<dyn Error>> {
    let source = root.join(format!("{}.typ", scenario.fixture));
    let output = root.join(format!("{binary_name}-{}.png", scenario.label));

    for _ in 0..warmup {
        run_once(binary, &source, &output, scenario)?;
    }

    let mut samples = Vec::with_capacity(runs as usize);
    for _ in 0..runs {
        samples.push(run_once(binary, &source, &output, scenario)?);
    }

    let mean_secs =
        samples.iter().map(|s| s.elapsed_secs).sum::<f64>() / samples.len() as f64;
    let variance = if samples.len() > 1 {
        samples
            .iter()
            .map(|s| {
                let deviation = s.elapsed_secs - mean_secs;
                deviation * deviation
            })
            .sum::<f64>()
            / (samples.len() - 1) as f64
    } else {
        0.0
    };
    let max_rss_kib = samples.iter().map(|s| s.max_rss_kib).max().unwrap_or(0);
    let output_bytes = fs::metadata(&output)?.len();

    Ok(Row {
        binary: binary_name.into(),
        scenario: scenario.label.into(),
        mean_secs,
        stddev_secs: variance.sqrt(),
        max_rss_kib,
        output_bytes,
    })
}

fn run_once(
    binary: &str,
    source: &Path,
    output: &PathBuf,
    scenario: &Scenario,
) -> Result<Sample, Box<dyn Error>> {
    let mut child = Command::new(binary)
        .args(["compile", "-j", "1", "--ppi", scenario.ppi])
        .args(scenario.extra)
        .arg(source)
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let pid = child.id() as libc::pid_t;
    let start = Instant::now();
    let (status, max_rss_kib) = wait4(pid)?;
    let elapsed_secs = start.elapsed().as_secs_f64();

    if !exited_successfully(status) {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        return Err(format!("{binary} exited with status {status}: {stderr}").into());
    }

    Ok(Sample { elapsed_secs, max_rss_kib })
}

/// Reaps the child ourselves via `wait4` (rather than `Child::wait`, which
/// discards the kernel's rusage) so peak RSS comes straight from the OS
/// instead of needing an external `/usr/bin/time`.
fn wait4(pid: libc::pid_t) -> Result<(i32, u64), Box<dyn Error>> {
    let mut status: i32 = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // `ru_maxrss` is already in KiB on Linux.
    Ok((status, usage.ru_maxrss as u64))
}

fn exited_successfully(status: i32) -> bool {
    status & 0x7f == 0 && (status >> 8) & 0xff == 0
}
