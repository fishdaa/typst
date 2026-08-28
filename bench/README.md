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
| `poster-rt1` | same as `poster`, `--render-threads 1` | 72 | the sequential render-then-encode loop, as a reference point |
| `poster-rt8` | same as `poster`, `--render-threads 8` | 72 | whether fan-out past the default of 4 still pays |
| `poster-scaled-rt1` | same as `poster-scaled`, `--render-threads 1` | 71 | as `poster-rt1`, on the resampling path |
| `poster-scaled-rt8` | same as `poster-scaled`, `--render-threads 8` | 71 | as `poster-rt8`, on the resampling path |
| `constrained` | same as `poster`, `--max-memory 512` | 71 | how far the memory cap actually binds |
| `balanced` | same as `opaque`, `--png-compression balanced` | 72 | encode effort matched to the baseline's default |

The `-rt1`/`-rt8` rows exist because `--render-threads` defaults to a value
derived from the core count, so the plain rows measure the default rather than
any fixed width. `-rt1` disables both the row tiling and the render/encode
pipeline, which makes it the honest before-picture for those two changes, and
`-rt8` shows whether more threads than the default help. They are fork-only
flags, so these rows are skipped for the baseline binary.

Three details matter more than they look:

**Native vs. resampled.** A page `N` points wide at 72 ppi is exactly `N`
device pixels, so a full-bleed asset of the same pixel size lands at its
native resolution and is copied rather than resampled. Real posters almost
never line up that way, and the resampling path is substantially different
code, so `poster-scaled` compiles the same fixture at 71 ppi to measure it.
A benchmark that only ever renders at native resolution will not see that
path at all.

**Photographic fixture content.** `write_png` fills the asset with a smooth
gradient plus deterministic block-coherent noise, so it compresses roughly
like a photograph (about three-fold; the harness prints the ratio it achieved
for each fixture it writes). Both extremes mislead: a flat-color fixture
compresses several hundred-fold, which makes inflating the source and
deflating the output nearly free, while independent per-pixel noise is
essentially incompressible, which turns the same measurement into pure memory
bandwidth. Either one hides the work this benchmark exists to measure, so
check the printed ratio if you substitute your own asset.

**Thread count is part of the measurement.** Pass `-j 1` (the harness does) to
keep page-level parallelism out of it, so that what is being measured is the
per-page pipeline rather than several pages overlapping. Note that the two are
separate: `--jobs` controls how many pages export at once, `--render-threads`
how many threads work on one page.

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
