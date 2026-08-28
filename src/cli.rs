//! Command-line surface for gitprism.
//!
//! Subcommands mirror the tool's design directly, not a general-purpose CLI
//! convention:
//!
//! - `setup`   — the one-time graft in decisions/0006.
//! - `sync`    — the recurring job in playbooks/0001; handles both sync
//!   directions in one run (decisions/0017: dest→source for every configured
//!   branch, then source→dest for every branch discovered on source).
//! - `resolve` — the conflict-resolution helper in decisions/0008.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ResolveDirection {
    /// Apply a pending destination commit onto source (the historical default).
    DestToSource,
    /// Apply a pending source commit onto destination.
    SourceToDest,
}

#[derive(Parser, Debug)]
#[command(name = "gitprism", version, about, long_about = None)]
pub struct Cli {
    /// Path to the gitprism config file.
    ///
    /// For every command except `setup`, this resolves against source's
    /// checked-out tree, since the config is versioned there. `setup` is the
    /// exception: it reads this same path off disk *before* source has a
    /// first commit to version it in.
    #[arg(short, long, global = true, default_value = crate::config::FILENAME)]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Graft source's initial commit onto dest's real tip.
    Setup,

    /// Sync both directions in one run.
    ///
    /// Safe to invoke redundantly: resume and no-op detection come from the
    /// trailer-based history scan, not from anything about how this command
    /// was triggered.
    Sync,

    /// Reproduce a dest<->source conflict and hand off to normal git
    /// conflict-resolution UX.
    Resolve {
        /// Which configured branch the conflict is on — the same identifier
        /// `sync`'s own conflict error prints.
        branch: String,

        /// Direction of the conflict to resolve. Defaults to dest-to-source
        /// for compatibility with the original resolve command.
        #[arg(long, value_enum, default_value_t = ResolveDirection::DestToSource)]
        direction: ResolveDirection,

        /// Finish a cherry-pick already started by a prior `gitprism resolve
        /// <branch>` run, once the human has resolved its conflicts and `git
        /// add`ed them. Explicit, matching git's own `rebase`/`cherry-pick`/
        /// `merge --continue` convention rather than having the bare command
        /// guess start-vs-resume from repo state.
        #[arg(long)]
        r#continue: bool,
    },

    /// Print the protected SHA-256 digest for the checked-out policy files.
    PolicyHash,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn policy_hash_is_a_read_only_subcommand() {
        let cli = Cli::try_parse_from(["gitprism", "policy-hash"]).unwrap();
        assert!(matches!(cli.command, Commands::PolicyHash));
    }

    #[test]
    fn policy_hash_accepts_the_global_config_path() {
        let cli =
            Cli::try_parse_from(["gitprism", "--config", "policy.toml", "policy-hash"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("policy.toml"));
    }

    #[test]
    fn resolve_direction_is_explicit_and_defaults_to_dest_to_source() {
        let cli = Cli::try_parse_from(["gitprism", "resolve", "main"]).unwrap();
        let Commands::Resolve { direction, .. } = cli.command else {
            panic!("expected resolve command");
        };
        assert_eq!(direction, ResolveDirection::DestToSource);

        let cli = Cli::try_parse_from([
            "gitprism",
            "resolve",
            "main",
            "--direction",
            "source-to-dest",
        ])
        .unwrap();
        let Commands::Resolve { direction, .. } = cli.command else {
            panic!("expected resolve command");
        };
        assert_eq!(direction, ResolveDirection::SourceToDest);
    }

    #[test]
    fn setup_is_a_recognized_subcommand_with_no_arguments_of_its_own() {
        let cli = Cli::try_parse_from(["gitprism", "setup"]).unwrap();
        assert!(matches!(cli.command, Commands::Setup));
    }

    #[test]
    fn sync_is_a_recognized_subcommand_with_no_arguments_of_its_own() {
        let cli = Cli::try_parse_from(["gitprism", "sync"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync));
    }

    #[test]
    fn resolve_dispatches_with_its_branch_direction_and_continue_flag() {
        let cli = Cli::try_parse_from([
            "gitprism",
            "resolve",
            "feature",
            "--direction",
            "source-to-dest",
            "--continue",
        ])
        .unwrap();
        let Commands::Resolve {
            branch,
            direction,
            r#continue,
        } = cli.command
        else {
            panic!("expected resolve command");
        };
        assert_eq!(branch, "feature");
        assert_eq!(direction, ResolveDirection::SourceToDest);
        assert!(r#continue);
    }

    #[test]
    fn resolve_continue_defaults_to_false() {
        let cli = Cli::try_parse_from(["gitprism", "resolve", "main"]).unwrap();
        let Commands::Resolve { r#continue, .. } = cli.command else {
            panic!("expected resolve command");
        };
        assert!(!r#continue);
    }

    #[test]
    fn resolve_continue_without_a_branch_is_rejected() {
        // `--continue` never substitutes for the required positional
        // `branch` argument — there's no reasonable default to resume
        // "whichever branch," so this must fail to parse rather than pick
        // one.
        let error = Cli::try_parse_from(["gitprism", "resolve", "--continue"])
            .expect_err("--continue must not make the branch argument optional");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn resolve_rejects_an_unknown_direction_value() {
        let error = Cli::try_parse_from(["gitprism", "resolve", "main", "--direction", "sideways"])
            .expect_err("an unrecognized --direction value must not silently pick a default");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        let error = Cli::try_parse_from(["gitprism", "frobnicate"])
            .expect_err("an unrecognized subcommand must not be silently accepted");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
