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
    let [upstream, fork] = args.as_slice() else {
        eprintln!("usage: typst-bench UPSTREAM_TYPST FORK_TYPST");
        std::process::exit(2);
    };

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
    for (name, binary) in [("upstream", upstream.as_str()), ("fork", fork.as_str())] {
        for scenario in ["opaque", "alpha", "svg"] {
            rows.push(report(root, name, binary, scenario, scenario, &[], warmup, runs)?);
        }
    }
    rows.push(report(
        root,
        "fork",
        fork,
        "constrained",
        "opaque",
        &["--max-memory", "512"],
        warmup,
        runs,
    )?);
    // Isolates the memory/time win of the rendering path from the effect of
    // the fork's default `--png-compression fast` (vs. upstream's harder,
    // slower default): same fixture and binary as the `opaque` row above,
    // just re-encoded at `high` effort.
    rows.push(report(
        root,
        "fork",
        fork,
        "high-compression",
        "opaque",
        &["--png-compression", "high"],
        warmup,
        runs,
    )?);

    print_table(&rows);
    Ok(())
}

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
    Ok(())
}

/// Streams rows straight into the PNG encoder instead of building a
/// full-size buffer first, since the benchmark's own point is exercising the
/// bounded-memory path rather than defeating it while generating fixtures.
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
    let mut writer = encoder.write_header()?.into_stream_writer()?;

    let pixel: &[u8] =
        if alpha { &[0x30, 0x60, 0x90, 0x80] } else { &[0x30, 0x60, 0x90] };
    let row: Vec<u8> = pixel
        .iter()
        .copied()
        .cycle()
        .take(pixel.len() * width as usize)
        .collect();
    for _ in 0..height {
        writer.write_all(&row)?;
    }
    writer.finish()?;
    Ok(())
}

struct Sample {
    elapsed_secs: f64,
    max_rss_kib: u64,
}

fn report(
    root: &Path,
    binary_name: &str,
    binary: &str,
    label: &str,
    fixture: &str,
    extra_args: &[&str],
    warmup: u32,
    runs: u32,
) -> Result<Row, Box<dyn Error>> {
    let source = root.join(format!("{fixture}.typ"));
    let output = root.join(format!("{binary_name}-{label}.png"));

    for _ in 0..warmup {
        run_once(binary, &source, &output, extra_args)?;
    }

    let mut samples = Vec::with_capacity(runs as usize);
    for _ in 0..runs {
        samples.push(run_once(binary, &source, &output, extra_args)?);
    }

    let mean_secs =
        samples.iter().map(|s| s.elapsed_secs).sum::<f64>() / samples.len() as f64;
    let variance = if samples.len() > 1 {
        samples
            .iter()
            .map(|s| (s.elapsed_secs - mean_secs).powi(2))
            .sum::<f64>()
            / (samples.len() - 1) as f64
    } else {
        0.0
    };
    let max_rss_kib = samples.iter().map(|s| s.max_rss_kib).max().unwrap_or(0);
    let output_bytes = fs::metadata(&output)?.len();

    Ok(Row {
        binary: binary_name.into(),
        scenario: label.into(),
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
    extra_args: &[&str],
) -> Result<Sample, Box<dyn Error>> {
    let mut child = Command::new(binary)
        .args(["compile", "-j", "1", "--ppi", "72"])
        .args(extra_args)
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
