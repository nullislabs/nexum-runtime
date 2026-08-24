//! Shared CLI surface for engine binaries, derived via clap.

use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

/// Parsed CLI surface.
///
/// `<bin> [<wasm-path> [<manifest-path>]] [--engine-config <path>] [--pretty-logs]`
/// `<bin> digest <artifact-path>`
///
/// Positional `<wasm-path>` synthesizes a one-module engine config.
/// Production deployments pass `--engine-config` and declare modules in
/// TOML.
///
/// `--pretty-logs` selects the human-readable tracing formatter; without
/// it the engine emits JSON log lines per the structured-logging contract.
///
/// A `digest` token is the subcommand wherever it sits, so a file of that
/// name has to be written `./digest`. `args_conflicts_with_subcommands`
/// stays off: in clap 4 it suppresses subcommand matching once any
/// top-level argument is seen, which would make `--pretty-logs digest
/// x.wasm` a launch of an artifact called `digest`.
#[derive(Parser, Debug, Default)]
#[command(
    about = "Run one or more Wasm Component modules under the engine supervisor",
    long_about = None,
    version,
)]
pub struct Cli {
    /// A subcommand answered in place of a launch, before any config load.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Optional positional path to a Wasm Component file. Synthesizes
    /// a one-module engine config when no `--engine-config` is given.
    pub wasm: Option<PathBuf>,

    /// Optional manifest path; defaults to a mandatory `component.toml`
    /// sibling of the wasm.
    pub manifest: Option<PathBuf>,

    /// Optional explicit path to the engine-wide `engine.toml` config.
    /// When omitted, the engine resolves the default search path
    /// documented in `engine_config::load_or_default`.
    #[arg(long = "engine-config")]
    pub engine_config: Option<PathBuf>,

    /// Use the human-readable tracing formatter instead of the
    /// default JSON formatter (structured-logging contract).
    #[arg(long = "pretty-logs")]
    pub pretty_logs: bool,

    /// Override `[engine] log_backfill_concurrency`, the chain-log
    /// poller's per-block `eth_getLogs` concurrency during backfill.
    #[arg(long = "log-backfill-concurrency")]
    pub log_backfill_concurrency: Option<usize>,
}

/// Work the launcher does instead of booting the engine.
#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Command {
    /// Print an artifact's `sha256:<hex>` content digest.
    Digest {
        /// The Wasm Component file to hash.
        path: PathBuf,
    },
}

impl Cli {
    /// Parse the process arguments under the binary's `name`, exiting on
    /// `--help`/`--version` or a usage error.
    #[must_use]
    pub fn parse_as(name: &'static str) -> Self {
        let matches = Self::command().name(name).get_matches();
        Self::from_arg_matches(&matches).unwrap_or_else(|err| err.exit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flags land on the parsed surface under a caller-supplied name.
    #[test]
    fn flags_parse_under_a_supplied_name() {
        let matches = Cli::command()
            .name("nexum")
            .try_get_matches_from([
                "nexum",
                "--engine-config",
                "engine.toml",
                "--pretty-logs",
                "--log-backfill-concurrency",
                "8",
            ])
            .expect("valid arguments parse");
        let cli = Cli::from_arg_matches(&matches).expect("matches destructure");
        assert_eq!(cli.engine_config, Some(PathBuf::from("engine.toml")));
        assert!(cli.pretty_logs);
        assert_eq!(cli.log_backfill_concurrency, Some(8));
        assert!(cli.wasm.is_none());
        assert!(cli.command.is_none());
    }

    fn parse<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<Cli, clap::Error> {
        let matches = Cli::command().name("nexum").try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    /// The subcommand claims its path and leaves the launch positionals empty.
    #[test]
    fn digest_takes_the_artifact_path() {
        let cli = parse(["nexum", "digest", "component.wasm"]).expect("subcommand parses");
        assert_eq!(
            cli.command,
            Some(Command::Digest {
                path: PathBuf::from("component.wasm"),
            }),
        );
        assert!(cli.wasm.is_none());
        assert!(cli.manifest.is_none());
    }

    /// The subcommand name wins over either positional, so an artifact or
    /// manifest called `digest` is reachable only through a qualified path.
    #[test]
    fn a_qualified_path_named_digest_stays_a_launch() {
        let cli = parse(["nexum", "./digest", "./digest"]).expect("positionals parse");
        assert_eq!(cli.wasm, Some(PathBuf::from("./digest")));
        assert_eq!(cli.manifest, Some(PathBuf::from("./digest")));
        assert!(cli.command.is_none());
    }

    /// A bare `digest` token is the subcommand wherever it sits, so it
    /// refuses for a missing path rather than landing in a positional slot
    /// and booting the engine against a file called `digest`.
    #[test]
    fn a_bare_digest_token_refuses_in_either_position() {
        for args in [
            vec!["nexum", "digest"],
            vec!["nexum", "component.wasm", "digest"],
        ] {
            let err = parse(args.clone()).expect_err("a missing path refuses");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{args:?}: {err}",
            );
        }
    }

    /// A launch flag ahead of the subcommand does not hide it.
    ///
    /// `args_conflicts_with_subcommands` reads like the fix for the launch
    /// and digest overlap, but in clap 4 it stops subcommand matching once
    /// any top-level argument is seen, which turns this line into a launch
    /// of an artifact called `digest`. The flag stays off; a launch flag
    /// that digest cannot honour is parsed and unused.
    #[test]
    fn a_leading_flag_does_not_hide_the_subcommand() {
        let cli = parse(["nexum", "--pretty-logs", "digest", "component.wasm"])
            .expect("subcommand parses");
        assert_eq!(
            cli.command,
            Some(Command::Digest {
                path: PathBuf::from("component.wasm"),
            }),
        );
        assert!(cli.wasm.is_none());
    }
}
