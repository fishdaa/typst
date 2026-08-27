# Large-asset benchmark

This is a small, deterministic, Linux-oriented harness for comparing Typst
binaries on documents built around a large raster or vector asset. It is a
plain Rust binary in the workspace (no shell script, Python, or external
benchmarking tool) that generates fixtures, runs each scenario for a warmup
plus several timed repetitions, and reports mean/stddev wall time. Peak
resident set size is read straight from the kernel via `wait4`'s `rusage` on
each repetition; the max across repetitions is reported (RSS is a repeatable
ceiling, not a noisy quantity like wall time, so a max is more meaningful
than an average). Results print as a human-readable, column-aligned table
(time as ms/s ± stddev, memory and output size as KiB/MiB), one row per
binary and scenario.

## Usage

Pass two or more binaries. The first is the baseline (assumed to be
upstream): scenarios that use fork-only flags are skipped for it and run for
every later binary. Each argument may be given as `LABEL=PATH` to control
the row label.

```sh
cargo build --release
cargo build --release -p typst-bench

TMPDIR=/var/tmp cargo run --release -p typst-bench -- \
  stock=/path/to/upstream/typst \
  fork=target/release/typst
```

**Set `TMPDIR` to a disk-backed directory.** The harness writes its fixtures
and output through `TMPDIR`, and on most modern distributions `/tmp` is a
`tmpfs`, i.e. RAM. Benchmarking there charges the asset and the encoded
output to memory, and makes the page-cache eviction this fork does a no-op —
both of which are exactly what the memory numbers are supposed to measure.

Environment variables:

| variable | default | meaning |
| --- | --- | --- |
| `TYPST_BENCH_SIZE` | `7200x18000` | page and asset size in pixels |
| `TYPST_BENCH_RUNS` | `5` | timed repetitions per scenario |
| `TYPST_BENCH_WARMUP` | `1` | untimed warmup runs per scenario |

The default poster is intentionally large: it is a stress test, not a normal
document benchmark. Override `TYPST_BENCH_SIZE=2400x6000` for a quicker local
smoke run — but note that small fixtures barely exercise the bounded-memory
paths, since those only matter for large full-bleed assets, so the default
size (or your own worst case) is what to use for real numbers.

## Scenarios

| scenario | fixture | `--ppi` | what it isolates |
| --- | --- | --- | --- |
| `opaque` | opaque PNG background, no text | 72 | the simplest large-asset case, at native resolution |
| `alpha` | PNG background with an alpha channel | 72 | the blend-instead-of-overwrite path |
| `svg` | SVG background | 72 | direct SVG rendering vs. a full-size texture |
| `poster` | opaque PNG background + text/data on top | 72 | the shape real documents have |
| `poster-scaled` | same as `poster` | 71 | the resampling path (see below) |
| `constrained` | same as `poster`, `--max-memory 512` | 71 | how far the memory cap actually binds |
| `balanced` | same as `opaque`, `--png-compression balanced` | 72 | encode effort matched to the baseline's default |

Two details matter more than they look:

**Native vs. resampled.** A page `N` points wide at 72 ppi is exactly `N`
device pixels, so a full-bleed asset of the same pixel size lands at its
native resolution and is copied rather than resampled. Real posters almost
never line up that way, and the resampling path is substantially different
code, so `poster-scaled` compiles the same fixture at 71 ppi to measure it.
A benchmark that only ever renders at native resolution will not see that
path at all.

**Photographic fixture content.** `write_png` fills the asset with a smooth
gradient plus deterministic per-pixel noise, so it compresses roughly like a
photograph. A flat-color fixture compresses several hundred-fold, which makes
both inflating the source and deflating the output nearly free — precisely
the work this benchmark exists to measure.

The last row deserves the same caution: the fork's default
`--png-compression` is `fast`, which is itself cheaper than the `png` crate's
default (`balanced`) that the baseline uses, so part of the plain time gap is
the compression default rather than the rendering path. The `balanced` row
re-runs the same fixture at that same effort to separate the two.

Requirements: none beyond the workspace's usual `cargo build` (Linux only,
since it uses `wait4`/`rusage` directly). The harness does not modify the
repository and places generated files under a temporary directory.

The numbers are machine-specific. Compare binaries on the same host, with the
same fonts, `-j 1` (which the harness passes), and a quiet system. Peak RSS is
not the same as a serverless cgroup's exact accounting, but it is a useful
repeatable signal.
