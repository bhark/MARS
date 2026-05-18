//! clap-facing CLI surface for the `mars` binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "mars",
    version,
    about = "MARS - Map Artifact Rendering Service",
    long_about = None,
    // top-level args (--mode, --config) are mutually exclusive with the
    // tooling subcommands. clap enforces this at parse time so renames or
    // new subcommands can't drift away from the constraint.
    args_conflicts_with_subcommands = true,
)]
pub(crate) struct Cli {
    /// Service operation mode. Required for service operation; mutually
    /// exclusive with subcommands.
    #[arg(long, value_enum)]
    pub(crate) mode: Option<Mode>,

    /// Path to the service configuration file.
    #[arg(long, default_value = "/etc/mars/mars.yaml")]
    pub(crate) config: PathBuf,

    /// Operational tooling. Mutually exclusive with `--mode`.
    #[command(subcommand)]
    pub(crate) tool: Option<Tool>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum Mode {
    /// Serve WMS / WMTS / health / metrics. Stateless. Multiple replicas allowed.
    Runtime,
    /// Subscribe to the source change feed, build artifacts, publish manifests.
    /// Singleton per service.
    Compiler,
    /// Both compiler and runtime in one process. Dev / tiny deployments only.
    AllInOne,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Tool {
    /// Validate a configuration file: parse YAML and run cross-cutting checks.
    Validate {
        /// Path to the configuration file.
        path: PathBuf,
    },
    /// Inspect a `.mars` artifact: footer, sections, hashes, bbox, schema.
    Inspect {
        /// Path to the artifact file.
        path: PathBuf,
    },
    /// Perform an HTTP health check against a URL.
    /// Exits 0 on 2xx, 1 otherwise. Used by container health probes.
    Healthcheck {
        /// URL to GET.
        #[arg(long)]
        url: String,
    },
    /// Idempotently provision the postgres catalog objects MARS needs (role,
    /// grants, publication, slot). Reads names + schemas from the config file.
    Setup {
        /// Path to the configuration file.
        #[arg(long)]
        config: PathBuf,
        /// libpq DSN for an admin connection (CREATE ROLE / CREATE PUBLICATION
        /// / pg_create_logical_replication_slot privileges).
        #[arg(long, env = "MARS_ADMIN_DSN")]
        admin_dsn: String,
        /// Password to set on the runtime role.
        #[arg(long, env = "MARS_RUNTIME_PASSWORD")]
        runtime_password: String,
        /// Print the SQL that would be executed and exit without connecting.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Inverse of `setup`. Each drop is opt-in.
    Teardown {
        /// Path to the configuration file.
        #[arg(long)]
        config: PathBuf,
        #[arg(long, env = "MARS_ADMIN_DSN")]
        admin_dsn: String,
        /// Drop the replication slot.
        #[arg(long, default_value_t = false)]
        drop_slot: bool,
        /// Drop the publication.
        #[arg(long, default_value_t = false)]
        drop_publication: bool,
        /// Drop the runtime role.
        #[arg(long, default_value_t = false)]
        drop_role: bool,
        /// Print the SQL that would be executed and exit without connecting.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}
