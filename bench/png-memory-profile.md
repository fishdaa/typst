# PNG memory profiling

Profiled the working tree based on `10554e64` on an AMD Ryzen 7 8845HS
(Linux x86-64). Both binaries use the workspace release profile. The baseline
includes the pre-existing render debug print; that print is preserved in the
patch. Measurements use disk-backed files in `target/png-profile`, 72 ppi,
`-j 1`, and explicit render thread counts. Each configuration runs three times;
RSS is the maximum and elapsed time is the mean. These are process RSS numbers,
not a hard cgroup memory bound. Synthetic fixtures use deterministic gradients
and block noise; RGBA fixtures have varying alpha, including transparent pixels.
They compress more easily than many photographs, so timings are workload-specific.

## Initial measurements (before grayscale streaming)

The source is 3600×9000 pixels (32.4 MP). Scaled outputs are 3200×8000;
fractional placement adds a 0.25 pt offset in both axes.

| Fixture | Threads | Before RSS (MiB) | After RSS (MiB) | Before (s) | After (s) |
|---|---:|---:|---:|---:|---:|
| rgb-native | 1 | 37.9 | 38.1 | 0.147 | 0.147 |
| rgb-native | 4 | 40.5 | 35.5 | 0.090 | 0.090 |
| rgb-scaled | 1 | 80.6 | 80.4 | 0.400 | 0.390 |
| rgb-scaled | 4 | 59.4 | 55.0 | 0.187 | 0.193 |
| rgba-scaled | 1 | 115.3 | 115.3 | 0.670 | 0.673 |
| rgba-scaled | 4 | 69.5 | 69.3 | 0.263 | 0.270 |
| rgba-fractional | 1 | 116.0 | 115.7 | 0.617 | 0.633 |
| rgba-fractional | 4 | 71.5 | 72.5 | 0.283 | 0.273 |
| gray-scaled | 1 | 225.9 | 226.3 | 0.747 | 0.747 |
| gray-scaled | 4 | 559.8 | 202.2 | 0.567 | 0.490 |

The largest measured benefit is preventing concurrent full-image conversions.
The affine texture reuse removes an allocation, but does not show a consistent
peak-RSS improvement in the fractional-placement case; resize-time allocations
still dominate. Small timing differences should not be treated as speedups.

Additional stress cases, four render threads:

| Fixture | Source size | Before RSS (MiB) | After RSS (MiB) | Before (s) | After (s) |
|---|---|---:|---:|---:|---:|
| rgba-rotated | 3600×9000 | 173.8 | 169.9 | 8.573 | 8.680 |
| gray-scaled | 7200×18000 | 2141.0 | 670.8 | 2.157 | 1.953 |
| rgba-fractional | 7200×18000 | 85.7 | 83.4 | 1.230 | 1.297 |
| rgb-scaled | 7200×18000 | 67.7 | 62.1 | 0.730 | 0.757 |

Large scaled outputs are 6400×16000. The rotated fixture uses a 90° rotation
at native scale. Its roughly 8.6-second runtime, versus subsecond unrotated
32.4 MP cases, highlights the cost of repeatedly walking full-height source
regions. This patch does not resolve that bottleneck.

## Changes

- Fixed a bounds panic (and incorrect channel interpretation before the panic)
  when compositing a masked, resized opaque PNG. The resize buffer can be RGB,
  so compositing must use its actual stride and supply an opaque alpha channel.
  Reproduced with a rounded clipping block and with a failing unit test before
  the fix; the test covers both enlargement and reduction, RGB and RGBA.
- Replaced concurrent memoized RGBA conversions with a per-raster `OnceLock`.
  On a cold cache, each render tile could allocate a whole converted image.
  All tiles now share one conversion; already decoded RGBA8 pixels are reused.
  A concurrent regression test verifies sharing and native RGBA buffer reuse.
- Changed the band handoff to a rendezvous channel. A channel capacity of one
  allowed three live canvases: encoding, queued, and rendering. Capacity zero
  enforces the two canvases assumed by the budget while still overlapping
  encoding with rendering.
- Reused the general affine path's resized RGBA allocation as its pixmap and
  released the source region immediately after resizing. This removes a
  redundant texture allocation without changing premultiplication arithmetic.

## Heap evidence

Valgrind Massif on the baseline's 3600×9000 grayscale fixture, four render
threads, measured 538.8 MiB peak heap. Of this, 518,400,000 bytes (494.4 MiB,
91.75%) came from one allocation stack: exactly four 3600×9000×4 RGBA buffers.
The release binary is stripped, so Massif reports addresses rather than Rust
function names. Attribution to concurrent `to_rgba8` calls follows from those
sizes, code inspection, and the before/after measurements.

The same Massif run after the changes measured 173.5 MiB peak heap and
129,600,000 bytes in that conversion allocation: one RGBA buffer instead of
four. This independently supports the RSS measurements.

## Validation

- `cargo test -p typst-library -p typst-render -p typst-cli --lib --bins --offline`:
  all 53 tests passed.
- The masked resize regression was run before the fix and failed with an
  out-of-bounds slice panic; it passes after the fix.
- All 14 generated before/after output pairs have identical decoded pixels,
  including large grayscale, RGB, fractional RGBA, and rotated RGBA cases.
- The rounded-clip RGB document now compiles and matches the baseline rendering
  of an equivalent opaque RGBA image, including partially covered clip edges.
- Release build, `cargo fmt --all -- --check`, and `git diff --check` passed.

## Remaining opportunities and limits

- Interlaced and EXIF-oriented PNGs still bypass row streaming.
  The shared conversion prevents concurrent duplicates but still retains a
  full decoded image and, if conversion is needed, one full RGBA8 image.
  Grayscale streaming has now been implemented; see the follow-up below.
- Rotated images can map an output band to most or all source rows. The
  row-range decoder currently returns the full source width even when only
  a narrow horizontal crop is visible. Cropping columns during row conversion
  would reduce those region buffers; the PNG still has to decode sequentially.
- Gradient/pattern pages still explicitly disable banding in `encode_bands`.
  Their full-page canvas can exceed the requested memory budget. Fixing the
  underlying paint-coordinate precision issue is separate from PNG decoding.
- The new converted-image cache lives as long as its `RasterImage`, whereas the
  previous conversion cache could be evicted independently by comemo. This
  trades potentially longer retention in watch/long-lived API workloads for
  predictable single-allocation initialization. Watch-mode eviction behavior
  was not profiled in this run.

## Local reproduction artifacts

`target/png-profile/` contains the before/after binaries, deterministic fixture
script (`fixtures.py`), measurement scripts (`profile.py`, `extra.py`), generated
Typst sources and PNGs, raw JSON measurements, and Massif output. The `large/`
subdirectory contains the 7200×18000 fixtures. These large temporary artifacts
are intentionally outside version control and will be removed by `cargo clean`.

```sh
python3 target/png-profile/fixtures.py
python3 target/png-profile/profile.py before
python3 target/png-profile/profile.py after
python3 target/png-profile/extra.py
valgrind --tool=massif --time-unit=B --max-snapshots=40 \
  --massif-out-file=target/png-profile/massif-gray-before.out \
  target/png-profile/before compile target/png-profile/gray-scaled.typ \
  target/png-profile/massif-gray.png --ppi 72 --render-threads 4 -j 1
```

## Follow-up: remove full-image grayscale buffers

The initial 670.8 MiB result still retained a 129,600,000-byte grayscale
image and a 518,400,000-byte RGBA conversion. Non-interlaced grayscale PNGs
now use the same bounded row cursor as RGB/RGBA images. Opaque grayscale
can provide RGB rows for resizing; grayscale alpha keeps its alpha channel.
Low-bit-depth samples and tRNS transparency are expanded by the PNG decoder;
16-bit samples use the existing exact rounding conversion. Retained rows
remain in their compact source layout.

Same fixtures and measurement method, three runs per configuration. These
runs use the default band budget, with no `--max-memory` override:

| Source size | Render threads | Peak RSS (MiB) | Mean time (s) |
|---|---:|---:|---:|
| 3600×9000 | 1 | 78.7 | 0.397 |
| 3600×9000 | 4 | 49.8 | 0.187 |
| 7200×18000 | 1 | 82.6 | 1.473 |
| 7200×18000 | 4 | 55.5 | 0.730 |

The large four-thread case falls from 670.8 MiB to 55.5 MiB (91.7% lower)
and from 1.953 s to 0.730 s. All four outputs (two sizes × two thread counts)
are pixel-identical to the previous version. These are local CLI process RSS
measurements, not Lambda execution measurements; runtime overhead and other
assets in a real document are additional.

The PDF consumer now keeps streamed grayscale data as luma rather than
mistakenly treating replicated RGB as the native color space. Tests cover
luma and alpha bytes and ICC eligibility for both 8-bit and 16-bit sources.
PNG tests cover 1/2/4/8-bit grayscale, transparent samples, 16-bit grayscale
with and without alpha, and overlapping requests. They also verify that
streaming does not initialize the full-image decode cache.

Validation: all 57 tests passed with
`cargo test -p typst-library -p typst-render -p typst-cli -p typst-pdf --lib --bins --offline`.
Release build, formatting, and diff checks passed. The final executable is
`target/release/typst`; the measurement copy is `target/png-profile/stream`.
Raw measurements are in `target/png-profile/stream.json`, with decoded-pixel
comparison results in `stream-profile.log`. Reproduce with
`python3 target/png-profile/stream-profile.py` using the existing fixtures.
