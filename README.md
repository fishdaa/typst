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

The fork therefore concentrates on reducing peak memory rather than changing
Typst's language or document output. Small assets keep the established
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
```

`--max-memory` is a PNG-export budget for the render/encode portion of a
compile, not a hard cgroup guarantee: document layout, fonts, and process
overhead still need room. For serverless deployment, leave headroom between
the flag and the worker's actual limit.

## Large-asset benchmarks

The numbers below were measured with [`bench/`](bench/) (default
7200×18000-pixel poster) comparing this checkout against upstream `a51e0280`
built from a clean worktree, both release builds, on the same host back to
back. Peak RSS is process resident memory; results vary with OS, allocator,
fonts, and filesystem cache — regenerate with the command further down
rather than trusting these across machines.

**Time**

| Scenario | Stock | Fork |
| --- | ---: | ---: |
| Opaque PNG export | 2.43 s | 849 ms |
| Alpha PNG export | 2.57 s | 711 ms |
| SVG export | 2.02 s | 324 ms |

**Peak RSS**

| Scenario | Stock | Fork |
| --- | ---: | ---: |
| Opaque PNG export | 1874 MiB | 51 MiB |
| Alpha PNG export | 1998 MiB | 35 MiB |
| SVG export | 1510 MiB | 42 MiB |

**Output size**

| Scenario | Stock | Fork |
| --- | ---: | ---: |
| Opaque PNG export | 536 KiB | 2.4 MiB |
| Alpha PNG export | 536 KiB | 2.4 MiB |
| SVG export | 613 KiB | 2.5 MiB |

Output size is the odd one out: the fork's PNG is larger, not smaller — see
below.

Two fork-only variants isolate specific effects, both on the opaque fixture:

| Variant | Time | Peak RSS | Output size |
| --- | ---: | ---: | ---: |
| `--max-memory 512` | 872 ms | 168 MiB | 2.4 MiB |
| `--png-compression high` (matches stock's ~536 KiB output size) | 1.45 s | 51 MiB | 527 KiB |

The last row matters: the fork's default `--png-compression` is `fast`,
which is itself faster than stock's (fixed, harder) compression — some of
the plain time gap above is that default, not only the rendering-path
rewrite. At matched output size, the fork is still ~1.7× faster and uses
~37× less memory, so the memory result holds independent of the compression
default.

The fork's behavior is deliberately scenario-dependent:

| Scenario | Stock path | Fork path | Expected benefit |
| --- | --- | --- | --- |
| Opaque, native-resolution raster | Full texture plus compositing | Direct blit into the destination | Avoids a full-size texture |
| Alpha raster at native resolution | General image compositing | Alpha-aware direct path when safe; fallback otherwise | Avoids unnecessary conversion copies |
| Transformed raster | General resampling path | Specialized blitting/resampling paths | Smaller transient buffers |
| Large SVG | Rasterize a full placed texture | Render directly into the destination canvas | Memory follows the destination/band |
| Small SVG | Texture path | Existing texture path | Keeps the fast ordinary case |
| Large PNG export | Full page canvas and encoded output | Streamed horizontal bands and output | Peak memory is independent of page height |
| Large source file | Heap-backed file bytes | Memory-mapped file bytes | Lets the OS reclaim clean pages under pressure |

Run the reproducible comparison in [`bench/`](bench/). It generates
deterministic fixtures, measures wall time and peak RSS for the same
scenarios with an upstream binary and this fork, and prints a human-readable
comparison table. The harness intentionally takes binaries as arguments so a
comparison can be made against any pinned upstream commit, not an
accidentally different local build:

```sh
cargo build --release
cargo run --release -p typst-bench -- \
  /path/to/upstream/target/release/typst \
  target/release/typst
```

The upstream binary is run without fork-only flags; the constrained fork case
uses `--max-memory 512`. See the harness README for fixture sizes, system
requirements, and how to pin `upstream/main` before building the baseline.

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
