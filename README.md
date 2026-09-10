<h1 align="center">
  <img alt="Typst" src="https://user-images.githubusercontent.com/17899797/226108480-722b770e-6313-40d7-84f2-26bebb55a281.png">
</h1>

<p align="center">
  <a href="https://typst.app/docs/">
    <img alt="Documentation" src="https://img.shields.io/website?down_message=offline&label=docs&up_color=007aff&up_message=online&url=https%3A%2F%2Ftypst.app%2Fdocs"
  ></a>
  <a href="https://typst.app/">
    <img alt="Typst App" src="https://img.shields.io/website?down_message=offline&label=typst.app&up_color=239dad&up_message=online&url=https%3A%2F%2Ftypst.app"
  ></a>
  <a href="https://discord.gg/2uDybryKPe">
    <img alt="Discord Server" src="https://img.shields.io/discord/1054443721975922748?color=5865F2&label=discord&labelColor=555"
  ></a>
  <a href="https://github.com/typst/typst/blob/main/LICENSE">
    <img alt="Apache-2 License" src="https://img.shields.io/badge/license-Apache%202-brightgreen"
  ></a>
  <a href="https://typst.app/jobs/">
    <img alt="Jobs at Typst" src="https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Ftypst.app%2Fassets%2Fdata%2Fshields.json&query=%24.jobs.text&label=jobs&color=%23A561FF&cacheSeconds=1800"
  ></a>
</p>

Typst is a new markup-based typesetting system that is designed to be as powerful
as LaTeX while being much easier to learn and use. Typst has:

- Built-in markup for the most common formatting tasks
- Flexible functions for everything else
- A tightly integrated scripting system
- Math typesetting, bibliography management, and more
- Fast compile times thanks to incremental compilation
- Friendly error messages in case something goes wrong

## Why this fork exists

This is a production-focused fork of [Typst](https://github.com/typst/typst).
The service that motivated it runs Typst in serverless workers with a strict
memory limit, while compiling documents that contain large raster and SVG
assets. Upstream's normal image path is excellent for ordinary documents, but
a large full-bleed asset can make several full-size copies live at once: the
decoded source, a render texture, the page canvas, and the encoded output.
That is enough to make an otherwise valid document exceed a small worker's
RAM limit.

The fork therefore concentrates on reducing peak memory, and on the wall-clock
cost of the paths it introduces, rather than changing Typst's language or
document output. Small assets keep the established
paths, where they are already fast; large assets get bounded-memory paths
instead — see the scenario table below for exactly which path applies where
and why. The full change set is in the commit history (`git log
upstream/main..HEAD`); the intended end state is to upstream it in focused,
reviewable pieces and merge the fork back into the main Typst repository.

The additional CLI controls are:

```sh
# Bound PNG export memory (MiB); lower values trade speed for RAM.
typst compile --max-memory 512 --ppi 72 poster.typ poster.png

# Choose a faster PNG compression effort for large exports.
typst compile --png-compression fastest poster.typ poster.png

# Choose how many threads render one page (default: cores, capped at 4).
typst compile --render-threads 4 poster.typ poster.png
```

`--render-threads` splits each horizontal band into that many row tiles,
rendered in parallel into disjoint rows of the band's own buffer, while a
further thread compresses the previous band — so rendering and PNG encoding
overlap instead of alternating. It is bounded by the encoder, which has to stay
sequential, so the default stops at four threads; `--render-threads 1` restores
the strictly sequential loop. Note that this is orthogonal to `--jobs`, which
controls how many *pages* are exported at once.

`--max-memory` is a PNG-export budget for the render/encode portion of a
compile, not a hard cgroup guarantee: document layout, fonts, and process
overhead still need room. Source assets are outside the budget but no longer
need to be inside it — a memory-mapped image is released back to the OS as it
is read, so a large background PNG does not stay resident for the whole
export. For serverless deployment, leave headroom between the flag and the
worker's actual limit.

## Large-asset benchmarks

The numbers below come from [`bench/`](bench/) at its default size: a
7200x18000-point page (130 megapixels of output) whose background is a PNG of
the same pixel size, compressing about three-fold as a photograph would
(118 MiB on disk, 371 MiB decoded). `stock` is upstream `a51e0280` built from
a clean worktree; both are release builds, measured on the same host back to
back with `-j 1`, one warmup and three timed repetitions
(`TYPST_BENCH_RUNS=3`), writing through a disk-backed `TMPDIR`. Peak RSS is
process resident memory. Results vary with OS, allocator, fonts, and
filesystem — regenerate them with the command below rather than trusting
these across machines.

| Scenario | Time (stock) | Time (fork) | Peak RSS (stock) | Peak RSS (fork) |
| --- | ---: | ---: | ---: | ---: |
| Opaque PNG background | 11.37 s | **1.51 s** | 2127 MiB | **45 MiB** |
| Alpha PNG background | 11.65 s | **1.60 s** | 2275 MiB | **47 MiB** |
| SVG background | 1.94 s | **0.23 s** | 1510 MiB | **42 MiB** |
| Poster (background + text/data) | 11.50 s | **1.54 s** | 2149 MiB | **56 MiB** |
| Poster, background resampled | 25.20 s | **1.73 s** | 3310 MiB | **77 MiB** |

The fork column was re-measured for the most recent round; the stock column is
carried over from the run that produced it, since building upstream again does
not change it. Both were measured on this host with the same harness.

Peak memory is the point of the exercise: the fork exports a 130-megapixel
poster in tens of mebibytes rather than gigabytes, and — importantly for a
container with a fixed limit — that figure no longer scales with the size of
the asset or the page.

One row needs its own explanation: *poster, background resampled* renders at
a PPI that does not line the
background up 1:1 with the output grid, which is the normal case for a real
poster and a substantially different code path. It is the most expensive
scenario for both binaries, and the one where the fork's advantage is
largest.

Output size is the one axis where the fork's default looks worse, because
that default is deliberately `--png-compression fast`:

| Scenario | Stock | Fork |
| --- | ---: | ---: |
| Opaque PNG background (fork default, `fast`) | 68.0 MiB | 118.4 MiB |
| Opaque PNG background at `--png-compression balanced` | 68.0 MiB | **62.0 MiB** |

`balanced` is the effort upstream uses, so the second row is the honest
like-for-like comparison: at matched effort the fork produces a 9% *smaller*
file (it writes three channels instead of four for an opaque page) in
8.97 s versus 11.37 s, still using 45 MiB against 2127 MiB. The
`fast` default trades file size for speed on purpose; `--png-compression`
exists to choose otherwise.

Finally, `--max-memory` behaves as a dial rather than a cliff: the same
resampled poster at `--max-memory 512` takes 1.88 s and peaks at 379 MiB,
against 1.73 s and 77 MiB with the flag left off (whose built-in budget is
tighter than 512 MiB for this page). Tightening it keeps working: on a
3000x9000-point poster the peak lands at 75%, 85% and 63% of a 128, 256 and
512 MiB cap respectively — the flag spends more of a large budget than it used
to, and stays inside it.

### What the most recent round changed

This round is about CPU rather than memory: the export was using about one
core regardless of how many the machine had. The fork's own before/after, same
fixtures and host, comparing the previous fork commit with this one:

| Scenario | Time | Peak RSS | Output |
| --- | ---: | ---: | ---: |
| Opaque PNG background | 2.17 s → **1.51 s** (−31%) | 43.6 → 44.8 MiB | unchanged |
| Alpha PNG background | 2.77 s → **1.60 s** (−42%) | 45.4 → 46.8 MiB | unchanged |
| SVG background | 0.31 s → **0.23 s** (−26%) | 41.7 → 41.8 MiB | unchanged |
| Poster (background + text/data) | 2.25 s → **1.54 s** (−32%) | 55.1 → 56.1 MiB | unchanged |
| Poster, background resampled | 3.15 s → **1.73 s** (−45%) | 93.6 → **77.3 MiB** (−17%) | unchanged |
| Opaque background at `balanced` | 9.67 s → **8.97 s** (−7%) | 45.1 MiB | unchanged |

`--render-threads` is what moves those numbers, and it saturates early,
because PNG compression is the other half of the work and cannot be
parallelized at this compression level:

| `--render-threads` | Poster | Poster, resampled |
| --- | ---: | ---: |
| 1 (the previous behavior) | 2.25 s | 3.08 s |
| 4 (the default) | **1.54 s** | **1.73 s** |
| 8 | 1.65 s | 1.81 s |

Eight threads are already slower than four: each extra tile re-walks the page
frame, and past the point where compression becomes the limit that walk is all
the extra thread contributes. The default therefore stops at four.

The four changes behind those numbers:

- **A band is rendered by several threads at once.** Each horizontal band is
  split into row tiles, and because a pixmap's rows are contiguous, each tile
  is a mutable view over a disjoint slice of the band's own buffer — so tiles
  render straight into the final pixels with no per-tile buffer and no
  compositing pass to merge them.
- **Rendering and encoding overlap.** They used to alternate: render a band,
  compress it, render the next. A renderer thread now works one band ahead of
  the encoder, which is worth most where compression is expensive.
- **Concurrent tiles share one PNG decoder.** Tiles reach an image's row
  cursor in an arbitrary order, and a PNG can only be decompressed forwards,
  so the naive result is a decoder restart — and a full re-inflate — per tile.
  The cursor now retains a window as tall as the band, sized from the band
  itself, so the rows are decoded exactly once and handed out in whatever
  order the tiles ask for them.
- **No alpha round-trip when resampling an opaque image.** Resampling
  premultiplies by alpha and divides it back out afterwards so transparent
  pixels don't bleed. At a uniform alpha of 255 both passes are exact
  identities, so they are skipped — two fewer passes over the region, and the
  buffers they needed are what account for the resampled poster's memory
  dropping as well.

Two caveats worth stating plainly:

- Peak memory is up by 1–2 MiB on the native paths, because two bands are now
  alive at once. With no `--max-memory` given, the default band size is
  divided by how many band-sized buffers the pipeline keeps alive, which is
  what keeps that difference to a couple of mebibytes rather than a couple of
  bands' worth.
- Rendered output is unchanged for pages drawn at native resolution, and the
  reference-image suite passes unmodified. Where a background has to be
  resampled, tiling subdivides the resample, which shifts sub-pixel rounding
  at tile seams. Measured against an unbanded render of the poster fixture,
  the previous commit already differed at 3 rows out of 8875 (banding does the
  same thing at band seams); with tiling it is 1–2 rows. So this is an
  existing artifact moving, not a new one — but it does mean a resampled page
  is not byte-identical to the previous commit. `--render-threads 1` is.

### Which path applies where

The fork's behavior is deliberately scenario-dependent: the established paths
are kept wherever they are already the right choice, and bounded-memory paths
kick in for the cases that would otherwise dominate peak memory.

| Scenario | Stock path | Fork path | Benefit |
| --- | --- | --- | --- |
| Opaque, native-resolution raster | Full texture plus compositing | Row-at-a-time blit straight into the canvas | No texture, no whole-band copy |
| Alpha raster at native resolution | General image compositing | Alpha-aware direct blend when safe; fallback otherwise | Avoids conversion copies |
| Transformed raster | Resample the whole placed image | Resample only the visible crop, three channels when opaque | Transient buffers follow the band |
| Overlapping band reads of one image | n/a (single full decode) | One decoder plus a bounded tail of decoded rows | Decode stays linear in image height |
| Large SVG | Rasterize a full placed texture | Render directly into the destination canvas | Memory follows the destination band |
| Small SVG | Texture path | Existing texture path | Keeps the fast ordinary case |
| Large PNG export | Full page canvas and encoded output | Streamed horizontal bands and output | Peak memory independent of page height |
| Rendering a band | One thread, then encode, then repeat | Band split into row tiles rendered in parallel | Uses the cores a worker is paying for |
| Encoding a band | Alternates with rendering | Overlapped with rendering the next band | Compression stops being dead time |
| Resampling an opaque raster | Premultiply, convolve, un-premultiply | Convolve directly | Two fewer passes over the region |
| Opaque page PNG export | Four channels | Three channels | A quarter less to filter, compress, and store |
| Large PNG load | Full decode up front to validate | Chunk structure and CRC check | No redundant decompression pass |
| Large source file | Heap-backed file bytes | Memory-mapped, released as it is read | Peak memory independent of asset size |

### Reproducing

The harness takes binaries as arguments, so a comparison is always against a
pinned commit rather than an accidentally different local build. Build the
baseline in a separate checkout, then:

```sh
cargo build --release
TMPDIR=/var/tmp cargo run --release -p typst-bench -- \
  stock=/path/to/upstream/target/release/typst \
  fork=target/release/typst
```

Any number of binaries can be passed; the first is treated as the baseline
and skips fork-only flags. **Point `TMPDIR` at a disk-backed directory** —
`/tmp` is a `tmpfs` on most distributions, which charges the asset and the
encoded output to RAM and makes the page-cache eviction a no-op, i.e. it
distorts exactly what is being measured. See the harness
[README](bench/README.md) for the scenario list, fixture design, and
environment variables.

The implementation-level budget checks are ordinary Rust tests and do not
require the large fixture:

```sh
cargo test -p typst-cli --bin typst band_budget
```

This repository contains the Typst compiler and its CLI, which is everything you
need to compile Typst documents locally. For the best writing experience,
consider signing up to our [collaborative online editor][app] for free.

## Example
A [gentle introduction][tutorial] to Typst is available in our documentation.
However, if you want to see the power of Typst encapsulated in one image, here
it is:
<p align="center">
 <img alt="Example" width="900" src="https://user-images.githubusercontent.com/17899797/228031796-ced0e452-fcee-4ae9-92da-b9287764ff25.png">
</p>


Let's dissect what's going on:

- We use _set rules_ to configure element properties like the size of pages or
  the numbering of headings. By setting the page height to `auto`, it scales to
  fit the content. Set rules accommodate the most common configurations. If you
  need full control, you can also use [show rules][show] to completely redefine
  the appearance of an element.

- We insert a heading with the `= Heading` syntax. One equals sign creates a top
  level heading, two create a subheading and so on. Typst has more lightweight
  markup like this; see the [syntax] reference for a full list.

- [Mathematical equations][math] are enclosed in dollar signs. By adding extra
  spaces around the contents of an equation, we can put it into a separate block.
  Multi-letter identifiers are interpreted as Typst definitions and functions
  unless put into quotes. This way, we don't need backslashes for things like
  `floor` and `sqrt`. And `phi.alt` applies the `alt` modifier to the `phi` to
  select a particular symbol variant.

- Now, we get to some [scripting]. To input code into a Typst document, we can
  write a hash followed by an expression. We define two variables and a
  recursive function to compute the n-th fibonacci number. Then, we display the
  results in a center-aligned table. The table function takes its cells
  row-by-row. Therefore, we first pass the formulas `$F_1$` to `$F_8$` and then
  the computed fibonacci numbers. We apply the spreading operator (`..`) to both
  because they are arrays and we want to pass the arrays' items as individual
  arguments.

<details>
  <summary>Text version of the code example.</summary>

  ```typst
  #set page(width: 10cm, height: auto)
  #set heading(numbering: "1.")

  = Fibonacci sequence
  The Fibonacci sequence is defined through the
  recurrence relation $F_n = F_(n-1) + F_(n-2)$.
  It can also be expressed in _closed form:_

  $ F_n = round(1 / sqrt(5) phi.alt^n), quad
    phi.alt = (1 + sqrt(5)) / 2 $

  #let count = 8
  #let nums = range(1, count + 1)
  #let fib(n) = (
    if n <= 2 { 1 }
    else { fib(n - 1) + fib(n - 2) }
  )

  The first #count numbers of the sequence are:

  #align(center, table(
    columns: count,
    ..nums.map(n => $F_#n$),
    ..nums.map(n => str(fib(n))),
  ))
  ```
</details>

## Installation
Typst's CLI is available from different sources:

- You can get sources and pre-built binaries for the latest release of Typst
  from the [releases page][releases]. Download the archive for your platform and
  place it in a directory that is in your `PATH`. To stay up to date with future
  releases, you can simply run `typst update`.

- You can install Typst through different package managers. Note that the
  versions in the package managers might lag behind the latest release.
  - Linux:
      - View [Typst on Repology][repology]
      - View [Typst's Snap][snap]
  - macOS: `brew install typst`
  - Windows: `winget install --id Typst.Typst`

- If you have a [Rust][rust] toolchain installed, you can install
  - the latest released Typst version with
    `cargo install --locked typst-cli`
  - a development version with
    `cargo install --git https://github.com/typst/typst --locked typst-cli`

- Nix users can
  - use the `typst` package with `nix-shell -p typst`
  - build and run the [Typst flake](https://github.com/typst/typst-flake) with
    `nix run github:typst/typst-flake -- --version`.

- Docker users can run a prebuilt image with
  `docker run ghcr.io/typst/typst:latest --help`.

## Usage
Once you have installed Typst, you can use it like this:
```sh
# Creates `file.pdf` in working directory.
typst compile file.typ

# Creates a PDF file at the desired path.
typst compile path/to/source.typ path/to/output.pdf
```

You can also watch source files and automatically recompile on changes. This is
faster than compiling from scratch each time because Typst has incremental
compilation.
```sh
# Watches source files and recompiles on changes.
typst watch file.typ
```

Typst further allows you to add custom font paths for your project and list all
of the fonts it discovered:
```sh
# Adds additional directories to search for fonts.
typst compile --font-path path/to/fonts file.typ

# Lists all of the discovered fonts in the system and the given directory.
typst fonts --font-path path/to/fonts

# Or via environment variable (Linux syntax).
TYPST_FONT_PATHS=path/to/fonts typst fonts
```

For other CLI subcommands and options, see below:
```sh
# Prints available subcommands and options.
typst help

# Prints detailed usage of a subcommand.
typst help watch
```

If you prefer an integrated IDE-like experience with autocompletion and instant 
preview, you can also check out our [free web app][app]. Alternatively, there is 
a community-created language server called 
[Tinymist](https://myriad-dreamin.github.io/tinymist/) which is integrated into 
various editor extensions.

## Community
The main places where the community gathers are our [Forum][forum] and our
[Discord server][discord]. The Forum is a great place to ask questions, help
others, and share cool things you created with Typst. The Discord server is more
suitable for quicker questions, discussions about contributing, or just to chat.
We'd be happy to see you there!

[Typst Universe][universe] is where the community shares templates and packages.
If you want to share your own creations, you can submit them to our
[package repository][packages].

If you had a bad experience in our community, please [reach out to us][contact].

## Contributing
We love to see contributions from the community. If you experience bugs, feel
free to open an issue. If you would like to implement a new feature or bug fix,
please follow the steps outlined in the [contribution guide][contributing].

To build Typst yourself, first ensure that you have the
[latest stable Rust][rust] installed. Then, clone this repository and build the
CLI with the following commands:

```sh
git clone https://github.com/typst/typst
cd typst
cargo build --release
```

The optimized binary will be stored in `target/release/`.

Another good way to contribute is by [sharing packages][packages] with the
community.

## Pronunciation and Spelling
IPA: /taɪpst/. "Ty" like in **Ty**pesetting and "pst" like in Hi**pst**er. When
writing about Typst, capitalize its name as a proper noun, with a capital "T".

## Design Principles
All of Typst has been designed with three key goals in mind: Power,
simplicity, and performance. We think it's time for a system that matches the
power of LaTeX, is easy to learn and use, all while being fast enough to realize
instant preview. To achieve these goals, we follow three core design principles:

- **Simplicity through Consistency:**
  If you know how to do one thing in Typst, you should be able to transfer that
  knowledge to other things. If there are multiple ways to do the same thing,
  one of them should be at a different level of abstraction than the other. E.g.
  it's okay that `= Introduction` and `#heading[Introduction]` do the same thing
  because the former is just syntax sugar for the latter.

- **Power through Composability:**
  There are two ways to make something flexible: Have a knob for everything or
  have a few knobs that you can combine in many ways. Typst is designed with the
  second way in mind. We provide systems that you can compose in ways we've
  never even thought of. TeX is also in the second category, but it's a bit
  low-level and therefore people use LaTeX instead. But there, we don't really
  have that much composability. Instead, there's a package for everything
  (`\usepackage{knob}`).

- **Performance through Incrementality:**
  All Typst language features must accommodate for incremental compilation.
  Luckily we have [`comemo`], a system for incremental compilation which does
  most of the hard work in the background.

## Acknowledgements

We'd like to thank everyone who is supporting Typst's development, be it via
[GitHub sponsors] or elsewhere. In particular, special thanks[^1] go to:

- [Posit](https://posit.co/blog/posit-and-typst/) for financing a full-time
  compiler engineer
- [NLnet](https://nlnet.nl/) for supporting work on Typst via multiple grants
  through the [NGI Zero Core](https://nlnet.nl/core) fund:
  - Work on [HTML export](https://nlnet.nl/project/Typst-HTML/)
  - Work on [PDF accessibility](https://nlnet.nl/project/Typst-Accessibility/)
- [Science & Startups](https://www.science-startups.berlin/) for having financed
  Typst development from January through June 2023 via the Berlin Startup
  Scholarship
- [Zerodha](https://zerodha.tech/blog/1-5-million-pdfs-in-25-minutes/) for their
  generous one-time sponsorship

[^1]: This list only includes contributions for our open-source work that exceed
    or are expected to exceed €10K.

[docs]: https://typst.app/docs/
[app]: https://typst.app/
[discord]: https://discord.gg/2uDybryKPe
[forum]: https://forum.typst.app/
[universe]: https://typst.app/universe/
[tutorial]: https://typst.app/docs/tutorial/
[show]: https://typst.app/docs/reference/styling/#show-rules
[math]: https://typst.app/docs/reference/math/
[syntax]: https://typst.app/docs/reference/syntax/
[scripting]: https://typst.app/docs/reference/scripting/
[rust]: https://rustup.rs/
[releases]: https://github.com/typst/typst/releases/
[repology]: https://repology.org/project/typst/versions
[contact]: https://typst.app/contact
[architecture]: https://github.com/typst/typst/blob/main/docs/dev/architecture.md
[contributing]: https://github.com/typst/typst/blob/main/CONTRIBUTING.md
[packages]: https://github.com/typst/packages/
[`comemo`]: https://github.com/typst/comemo/
[snap]: https://snapcraft.io/typst
[GitHub sponsors]: https://github.com/sponsors/typst/
