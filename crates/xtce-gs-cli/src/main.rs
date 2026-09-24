//! `xtce-gs` — a ground station for CCSDS telemetry described by XTCE.
//!
//! Thin on purpose: parse, dispatch, and turn a failure into a message and an exit code.
//! Everything that decides anything is in [`args`] — which turns flags into an
//! [`xtce_gs_engine::SessionConfig`] — or in [`run`], which carries one out.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// The doc comments on the argument types are clap's `--help` text, not rustdoc. Backticks
// around a container name would be shown to the operator literally, so they do not belong
// there — and `doc_markdown` wants them.
#![allow(clippy::doc_markdown)]
// A frame loss printed as a percentage is a `u64` counter over a `u64` counter rendered to one
// decimal place, and a byte offset in a file is a `u64` an index has to reach. Both are
// nowhere near where the conversion stops being exact, and flagging them one at a time would
// mean an `#[allow]` on most lines of the two report functions.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::process::ExitCode;

use clap::Parser;

mod args;
mod run;

use args::{Cli, Command};

/// What the command line can refuse to do.
///
/// Four variants, all of them `#[from]`: this program's own failures are already somebody
/// else's errors, and a fifth variant for "bad arguments" would duplicate what clap has
/// already printed and exited over.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The interface would not open, or the session behind it would not start.
    #[error("{0}")]
    Gui(#[from] xtce_gs_gui::GuiError),

    /// The session would not start: the definition, the configuration, or a limits file.
    #[error("{0}")]
    Engine(#[from] xtce_gs_engine::EngineError),

    /// The source or the framing cannot be used.
    #[error("{0}")]
    Link(#[from] xtce_gs_link::LinkError),

    /// A file said no, or the reactor would not build.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn main() -> ExitCode {
    match dispatch(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error);
            ExitCode::FAILURE
        }
    }
}

/// Prints a failure and everything under it, once each.
///
/// Every variant of every error enum in this workspace is `#[error("{0}")]` over a `#[from]`
/// field, so the outermost message and its source's message are usually the same string. A
/// chain printed without this prints it three times — `CliError`, `EngineError`, then
/// `LinkError` — and an operator reading three identical lines learns to read none of them.
/// Only a cause that *says something new* is printed.
fn report(error: &dyn std::error::Error) {
    let mut previous = error.to_string();
    eprintln!("error: {previous}");
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if message != previous {
            eprintln!("  caused by: {message}");
            previous = message;
        }
        source = cause.source();
    }
}

/// Turns one parsed command line into one thing done.
///
/// The conversions happen *here* rather than inside [`run`], so that a configuration nothing
/// can carry out is refused before a window, a socket or a file is opened — and the operator
/// reads the reason on the terminal they typed into.
fn dispatch(cli: Cli) -> Result<(), CliError> {
    let verbose = cli.verbose;
    match cli.command {
        Command::Run(args) => {
            let config = args.to_session_config()?;
            if args.headless {
                run::headless(config, verbose)
            } else {
                run::gui(config)
            }
        }

        Command::Replay(args) => {
            let config = args.to_session_config()?;
            if args.headless {
                run::headless(config, verbose)
            } else {
                run::gui(config)
            }
        }

        Command::Export(args) => {
            let config = args.to_session_config()?;
            run::export(&config, args.output.as_deref(), args.limit, verbose)
        }

        Command::Probe(args) => {
            let source = args.to_source_spec()?;
            let pipeline = args.to_pipeline_config()?;
            run::probe(&source, &pipeline, args.limit, args.sample, verbose)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        // clap's own consistency check: duplicate flags, a positional after an optional one,
        // a `default_value` that does not parse. It is a compile-time mistake that only
        // shows up at run time, which is exactly what a test is for.
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn a_chain_of_identical_messages_is_printed_once() {
        // Not a test of `report`'s output — it writes to stderr — but of the predicate it
        // turns on: the outer message and its cause are the same string for every wrapper in
        // this workspace, which is why `report` compares them.
        let inner = xtce_gs_link::LinkError::Config("frame_length is 0".to_owned());
        let outer = CliError::Engine(xtce_gs_engine::EngineError::Link(inner));
        let source = std::error::Error::source(&outer);
        assert_eq!(
            source.map(ToString::to_string),
            Some(outer.to_string()),
            "the wrappers no longer forward their message; `report` can stop deduplicating"
        );
    }
}
