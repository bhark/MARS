//! CLI surface: long-running operator + `print-crd` / `print-clusterrole`
//! for chart drift checks.

use std::net::SocketAddr;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum LogFormat {
    Json,
    Text,
}

impl LogFormat {
    pub(crate) fn is_json(self) -> bool {
        matches!(self, LogFormat::Json)
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "mars-operator",
    version,
    about = "Kubernetes operator for MarsService resources."
)]
pub(crate) struct Cli {
    /// Bind address for /metrics.
    #[arg(long, env = "MARS_OPERATOR_METRICS_ADDR", default_value = "0.0.0.0:9090")]
    pub(crate) metrics_addr: SocketAddr,

    /// Bind address for /healthz and /readyz.
    #[arg(long, env = "MARS_OPERATOR_HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    pub(crate) health_addr: SocketAddr,

    /// Enable leader election. Required when running >1 replica.
    #[arg(
        long,
        env = "MARS_OPERATOR_LEADER_ELECT",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub(crate) leader_elect: bool,

    /// Namespace to coordinate leader-election Lease in.
    #[arg(long, env = "MARS_OPERATOR_NAMESPACE", default_value = "mars-system")]
    pub(crate) namespace: String,

    /// Log level (e.g. info, debug). Accepts standard `RUST_LOG`-style filters.
    #[arg(long, env = "MARS_OPERATOR_LOG_LEVEL", default_value = "info")]
    pub(crate) log_level: String,

    /// Log format.
    #[arg(long, env = "MARS_OPERATOR_LOG_FORMAT", value_enum, default_value_t = LogFormat::Json)]
    pub(crate) log_format: LogFormat,

    /// Field manager string for server-side apply.
    #[arg(long, env = "MARS_OPERATOR_FIELD_MANAGER", default_value = "mars-operator")]
    pub(crate) field_manager: String,

    /// Container repository the operator constructs runtime/compiler image
    /// references against. The tag is always the operator's own version, so
    /// operator vX.Y.Z runs mars vX.Y.Z. Override only for air-gap mirrors.
    #[arg(long, env = "MARS_RUNTIME_IMAGE_REPOSITORY", default_value = "ghcr.io/bhark/mars")]
    pub(crate) runtime_image_repository: String,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Print the MarsService CRD to stdout as YAML.
    PrintCrd,
    /// Print the operator ClusterRole (Helm template) to stdout as YAML.
    #[command(name = "print-clusterrole")]
    PrintClusterRole,
}
