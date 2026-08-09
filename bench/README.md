# Large-asset benchmark

This is a small, deterministic, Linux-oriented harness for comparing an
upstream Typst binary with this fork. It is a plain Rust binary in the
workspace (no shell script, Python, or external benchmarking tool) that
generates fixtures, runs each scenario for a warmup plus several timed
repetitions, and reports mean/stddev wall time. Peak resident set size is
read straight from the kernel via `wait4`'s `rusage` on each repetition;
the max across repetitions is reported (RSS is a repeatable ceiling, not a
noisy quantity like wall time, so a max is more meaningful than an average).
Results print as a human-readable, column-aligned table (time as ms/s ±
stddev, memory and output size as KiB/MiB), with one row per scenario.

Build two release binaries from pinned source revisions. The benchmark in the
top-level README used upstream commit `a51e0280`; build that baseline in a
separate checkout, then run:

```sh
cargo build --release
cargo run --release -p typst-bench -- \
  /path/to/upstream/typst target/release/typst
```

The default poster is intentionally large: it is a stress test, not a normal
document benchmark. Override `TYPST_BENCH_SIZE=2400x6000` for a smaller local
smoke run — note that small fixtures don't exercise the bounded-memory paths
at all, since those only kick in for large full-bleed assets, so the default
size (or your own worst case) is what to use for real numbers. The scenarios
are opaque PNG, alpha PNG, and SVG; `constrained` also runs the fork with
`--max-memory 512`; `high-compression` re-runs the fork's `opaque` fixture
at `--png-compression high` to isolate the rendering-path memory/time win
from the fork's faster default compression level (upstream has no such flag
and presumably compresses harder by default, so `opaque`'s time gap includes
both effects — `high-compression` roughly matches upstream's output size to
separate them). `TYPST_BENCH_RUNS` (default `5`) and `TYPST_BENCH_WARMUP`
(default `1`) control the repetition and warmup counts.

Requirements: none beyond the workspace's usual `cargo build` (Linux only,
since it uses `wait4`/`rusage` directly). The harness does not modify the
repository and places generated files under a temporary directory.

The numbers are machine-specific. Compare binaries on the same host, with the
same fonts, `-j 1`, and a quiet system. Peak RSS is not the same as the
serverless cgroup's exact accounting, but it is a useful repeatable signal.
