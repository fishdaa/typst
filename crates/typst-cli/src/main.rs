mod args;
mod compile;
mod completions;
mod deps;
mod download;
mod eval;
mod fonts;
mod greet;
mod info;
mod init;
mod packages;
mod query;
mod terminal;
#[cfg(feature = "self-update")]
mod update;
mod watch;
mod world;

use std::cell::Cell;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::LazyLock;

use clap::Parser;
use clap::error::ErrorKind;
use codespan_reporting::term;
use codespan_reporting::term::termcolor::WriteColor;
use ecow::eco_format;
use serde::Serialize;
use typst::diag::{HintedStrResult, StrResult};

use crate::args::{CliArguments, Command, SerializationFormat};

thread_local! {
    /// The CLI's exit code.
    static EXIT: Cell<ExitCode> = const { Cell::new(ExitCode::SUCCESS) };
}

/// The parsed command line arguments.
static ARGS: LazyLock<CliArguments> = LazyLock::new(|| {
    CliArguments::try_parse().unwrap_or_else(|error| {
        if error.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
            crate::greet::greet();
        }
        error.exit();
    })
});

/// Entry point.
fn main() -> ExitCode {
    // Handle SIGPIPE
    // https://stackoverflow.com/questions/65755853/simple-word-count-rust-program-outputs-valid-stdout-but-panicks-when-piped-to-he/65760807
    sigpipe::reset();

    limit_malloc_arenas();

    let res = dispatch();

    if let Err(msg) = res {
        set_failed();
        print_error(msg.message()).expect("failed to print error");
        for hint in msg.hints() {
            print_hint(hint).expect("failed to print hint");
        }
    }

    EXIT.with(|cell| cell.get())
}

/// Execute the requested command.
fn dispatch() -> HintedStrResult<()> {
    match &ARGS.command {
        Command::Compile(command) => crate::compile::compile(command)?,
        Command::Watch(command) => crate::watch::watch(command)?,
        Command::Init(command) => crate::init::init(command)?,
        Command::Query(command) => crate::query::query(command)?,
        Command::Eval(command) => crate::eval::eval(command)?,
        Command::Fonts(command) => crate::fonts::fonts(command),
        Command::Update(command) => crate::update::update(command)?,
        Command::Completions(command) => crate::completions::completions(command),
        Command::Info(command) => crate::info::info(command)?,
    }
    Ok(())
}

/// Limits glibc to a small, fixed number of malloc arenas instead of its
/// default of up to `8 * num_cpus`.
///
/// Each arena keeps its own free list, which isn't reclaimed just because
/// the memory in it was freed -- with one arena per thread (glibc's
/// default), a large transient allocation (e.g. a PNG export band or a
/// resampled image texture, see `compile::band_budget`) made on one of
/// rayon's worker threads can inflate that thread's arena and stay resident
/// long after the allocation is freed, even though the process as a whole
/// has plenty of free heap elsewhere. This matters most for long-lived,
/// warm-reused processes (e.g. a serverless runtime keeping the same
/// process across invocations), where that unreturned memory persists as
/// baseline RSS for unrelated later work. Capping the arena count trades a
/// little allocator contention under heavy parallelism for much less
/// memory fragmentation/retention. Must run before the rayon thread pool is
/// built (see `world::SystemWorld::new`) to apply from the first allocation
/// on every thread. Linux+glibc only: `mallopt`/`M_ARENA_MAX` aren't
/// available on musl or other platforms, and this is a memory-usage
/// optimization only, never required for correctness, so it's a silent
/// no-op elsewhere.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    // SAFETY: `mallopt` only tunes glibc's allocator behavior; it doesn't
    // affect Rust-level memory safety and is safe to call at any point,
    // including before any other allocation has happened.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn limit_malloc_arenas() {}

/// Ensure a failure exit code.
fn set_failed() {
    EXIT.with(|cell| cell.set(ExitCode::FAILURE));
}

/// Print an application-level error (independent from a source file).
fn print_error(msg: &str) -> io::Result<()> {
    let styles = term::Styles::default();

    let mut output = terminal::out();
    output.set_color(&styles.header_error)?;
    write!(output, "error")?;

    output.reset()?;
    writeln!(output, ": {msg}")
}

/// Print an application-level hint (independent from a source file).
fn print_hint(msg: &str) -> io::Result<()> {
    let styles = term::Styles::default();

    let mut output = terminal::out();
    output.set_color(&styles.header_help)?;
    write!(output, "hint")?;

    output.reset()?;
    writeln!(output, ": {msg}")
}

/// Serialize data to the output format and convert the error to an
/// [`EcoString`].
fn serialize(
    data: &impl Serialize,
    format: SerializationFormat,
    pretty: bool,
) -> StrResult<String> {
    match format {
        SerializationFormat::Json => {
            if pretty {
                serde_json::to_string_pretty(data).map_err(|e| eco_format!("{e}"))
            } else {
                serde_json::to_string(data).map_err(|e| eco_format!("{e}"))
            }
        }
        SerializationFormat::Yaml => {
            serde_yaml::to_string(data).map_err(|e| eco_format!("{e}"))
        }
    }
}

#[cfg(not(feature = "self-update"))]
mod update {
    use typst::diag::{StrResult, bail};

    use crate::args::UpdateCommand;

    pub fn update(_: &UpdateCommand) -> StrResult<()> {
        bail!(
            "self-updating is not enabled for this executable, \
             please update with the package manager or mechanism \
             used for initial installation",
        )
    }
}
