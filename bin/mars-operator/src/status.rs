//! CR status computation. Encodes the kubectl-rollout-status readiness check
//! so callers (the user, ArgoCD, the chart smoke test) see a faithful Ready
//! signal rather than the raw replica counts.

use k8s_openapi::api::apps::v1::Deployment;

use crate::crd::spec::{Condition, DefinitionObserved, DefinitionStatus, MarsServiceStatus};

pub(crate) fn now_rfc3339() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_rfc3339_utc(secs)
}

fn format_rfc3339_utc(secs: i64) -> String {
    // tiny self-contained formatter so we do not pull chrono just for status
    // timestamps. accurate to second; UTC; gregorian.
    let mut days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let h = (secs_of_day / 3600) as u32;
    let m = ((secs_of_day % 3600) / 60) as u32;
    let s = (secs_of_day % 60) as u32;

    let mut year: i64 = 1970;
    loop {
        let dy = if is_leap(year) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let mut month = 1u32;
    while month <= 12 {
        let dm = days_in_month(year, month) as i64;
        if days < dm {
            break;
        }
        days -= dm;
        month += 1;
    }
    let day = (days + 1) as u32;
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

pub(crate) fn condition(type_: &str, status: bool, reason: &str, message: &str) -> Condition {
    raw_condition(type_, if status { "True" } else { "False" }, reason, message)
}

/// kube convention is True / False / Unknown; emit Unknown when an upstream
/// prerequisite skipped the check (so consumers see explicit "not evaluated"
/// rather than a missing condition).
pub(crate) fn raw_condition(type_: &str, status: &str, reason: &str, message: &str) -> Condition {
    Condition {
        type_: type_.into(),
        status: status.into(),
        reason: reason.into(),
        message: message.into(),
        last_transition_time: now_rfc3339(),
    }
}

/// kubectl-rollout-status semantics: a Deployment is "available" only when
/// generation observed, updated and available replica counts match the spec,
/// and progressing-via-new-replica-set has settled.
pub(crate) fn deployment_ready(d: &Deployment) -> bool {
    let Some(status) = &d.status else {
        return false;
    };
    let spec_replicas = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let generation = d.metadata.generation.unwrap_or(0);

    if status.observed_generation.unwrap_or(0) < generation {
        return false;
    }
    if status.updated_replicas.unwrap_or(0) < spec_replicas {
        return false;
    }
    if status.available_replicas.unwrap_or(0) < spec_replicas {
        return false;
    }
    let available_ok = status
        .conditions
        .as_ref()
        .map(|c| c.iter().any(|cond| cond.type_ == "Available" && cond.status == "True"))
        .unwrap_or(false);
    if !available_ok {
        return false;
    }
    true
}

pub(crate) struct StatusInputs<'a> {
    pub(crate) observed_generation: i64,
    /// Outcome of resolving the cluster catalog + spec.sources for the new
    /// path; on the legacy path use [`Resolution::Legacy`].
    pub(crate) catalog: Resolution<'a>,
    /// Outcome of resolving + fetching + parsing the RenderDefinition for the
    /// new path; on the legacy path use [`Resolution::Legacy`].
    pub(crate) definition: Resolution<'a>,
    /// Identity of the most recently fetched RenderDefinition. `Some` only
    /// when DefinitionResolved=True on the new path.
    pub(crate) definition_observed: Option<ObservedDefinition<'a>>,
    pub(crate) config_valid: bool,
    pub(crate) config_message: &'a str,
    pub(crate) children_applied: bool,
    pub(crate) children_message: &'a str,
    pub(crate) compiler_ready: bool,
    pub(crate) runtime_ready: bool,
    pub(crate) degraded: Option<&'a str>,
    /// Postgres bootstrap state. `None` means the CR does not declare a
    /// `spec.bootstrap` block, in which case no `BootstrapReady` condition is
    /// emitted at all so cluster operators see exactly the conditions that
    /// apply to their setup.
    pub(crate) bootstrap: Option<BootstrapStatus<'a>>,
    /// Name of the resolved runtime-password Secret (BYO or operator-managed).
    /// Surfaced on status so consumers can locate it without recomputing the
    /// naming convention.
    pub(crate) runtime_credentials_secret: Option<&'a str>,
    /// Name of the operator-managed admin-credentials Secret holding the
    /// composed admin DSN. Populated only when the component-style
    /// `bootstrap.adminCredentialsRef` branch is in use; absent for BYO
    /// `adminSecretRef` and for disabled/missing bootstrap.
    pub(crate) bootstrap_admin_credentials_secret: Option<&'a str>,
}

/// Tri-state outcome of an upstream resolution step.
#[derive(Clone, Copy)]
pub(crate) enum Resolution<'a> {
    /// Legacy `spec.config` path - resolution does not apply.
    Legacy,
    /// New path, succeeded.
    Resolved,
    /// New path, blocked by a prior step (e.g. CatalogResolved=False).
    /// Surfaces as `Unknown` with reason `Skipped`.
    Skipped { blocked_by: &'a str },
    /// New path, failed with a typed reason. Surfaces as `False`.
    Failed { reason: ResolutionReason, message: &'a str },
}

/// Reason vocabulary for failed resolution conditions. Mirrors the typed
/// `OperatorError` variants in `error.rs` so reconcile maps each fork to
/// exactly one reason.
#[derive(Clone, Copy)]
pub(crate) enum ResolutionReason {
    // CatalogResolved=False
    ClusterNotFound,
    UnknownSourceId,
    InvalidCatalog,
    // DefinitionResolved=False
    ExactlyOneViolated,
    DefinitionResolveError,
    DefinitionFetchError,
    DefinitionDecodeError,
    // shared
    SpecInvalid,
    Internal,
}

impl ResolutionReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ClusterNotFound => "ClusterNotFound",
            Self::UnknownSourceId => "UnknownSourceId",
            Self::InvalidCatalog => "InvalidCatalog",
            Self::ExactlyOneViolated => "ExactlyOneViolated",
            Self::DefinitionResolveError => "DefinitionResolveError",
            Self::DefinitionFetchError => "DefinitionFetchError",
            Self::DefinitionDecodeError => "DefinitionDecodeError",
            Self::SpecInvalid => "SpecInvalid",
            Self::Internal => "Internal",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ObservedDefinition<'a> {
    pub(crate) adapter: &'a str,
    pub(crate) revision: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) struct BootstrapStatus<'a> {
    pub(crate) ready: bool,
    pub(crate) reason: BootstrapReason,
    pub(crate) message: &'a str,
}

#[derive(Clone, Copy)]
pub(crate) enum BootstrapReason {
    Ready,
    ManualVerified,
    InProgress,
    Failed,
    ManualSetupIncomplete,
}

impl BootstrapReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::ManualVerified => "ManualVerified",
            Self::InProgress => "InProgress",
            Self::Failed => "Failed",
            Self::ManualSetupIncomplete => "ManualSetupIncomplete",
        }
    }
}

pub(crate) fn compute(inputs: StatusInputs<'_>) -> MarsServiceStatus {
    let mut conditions = Vec::new();
    conditions.push(resolution_condition("CatalogResolved", inputs.catalog));
    conditions.push(resolution_condition("DefinitionResolved", inputs.definition));
    conditions.push(condition(
        "ConfigValid",
        inputs.config_valid,
        if inputs.config_valid { "Validated" } else { "Invalid" },
        inputs.config_message,
    ));
    if let Some(bs) = inputs.bootstrap {
        conditions.push(condition("BootstrapReady", bs.ready, bs.reason.as_str(), bs.message));
    }
    conditions.push(condition(
        "ChildrenApplied",
        inputs.children_applied,
        if inputs.children_applied {
            "Applied"
        } else {
            "ApplyFailed"
        },
        inputs.children_message,
    ));
    conditions.push(condition(
        "CompilerAvailable",
        inputs.compiler_ready,
        if inputs.compiler_ready {
            "RolloutComplete"
        } else {
            "Rolling"
        },
        if inputs.compiler_ready {
            "compiler ready"
        } else {
            "compiler rolling out"
        },
    ));
    conditions.push(condition(
        "RuntimeAvailable",
        inputs.runtime_ready,
        if inputs.runtime_ready {
            "RolloutComplete"
        } else {
            "Rolling"
        },
        if inputs.runtime_ready {
            "runtime ready"
        } else {
            "runtime rolling out"
        },
    ));
    let degraded_msg = inputs.degraded.unwrap_or("");
    conditions.push(condition(
        "Degraded",
        inputs.degraded.is_some(),
        if inputs.degraded.is_some() {
            "Violation"
        } else {
            "Healthy"
        },
        degraded_msg,
    ));

    let bootstrap_blocking = matches!(inputs.bootstrap, Some(bs) if !bs.ready);
    let bootstrap_failed = matches!(inputs.bootstrap, Some(bs) if matches!(bs.reason, BootstrapReason::Failed));
    let phase = if !inputs.config_valid || bootstrap_failed {
        "Failed"
    } else if inputs.degraded.is_some() {
        "Degraded"
    } else if bootstrap_blocking || !inputs.children_applied {
        "Reconciling"
    } else if inputs.compiler_ready && inputs.runtime_ready {
        "Ready"
    } else {
        "Reconciling"
    };

    MarsServiceStatus {
        phase: Some(phase.into()),
        observed_generation: Some(inputs.observed_generation),
        conditions,
        runtime_credentials_secret: inputs.runtime_credentials_secret.map(str::to_string),
        bootstrap_admin_credentials_secret: inputs.bootstrap_admin_credentials_secret.map(str::to_string),
        definition: inputs.definition_observed.map(|o| DefinitionStatus {
            observed: DefinitionObserved {
                adapter: o.adapter.into(),
                revision: o.revision.into(),
            },
        }),
    }
}

fn resolution_condition(type_: &str, resolution: Resolution<'_>) -> Condition {
    match resolution {
        Resolution::Legacy => raw_condition(type_, "True", "LegacyPath", "legacy spec.config path"),
        Resolution::Resolved => raw_condition(type_, "True", "Resolved", "resolved"),
        Resolution::Skipped { blocked_by } => raw_condition(
            type_,
            "Unknown",
            "Skipped",
            &format!("skipped: blocked by {blocked_by}"),
        ),
        Resolution::Failed { reason, message } => raw_condition(type_, "False", reason.as_str(), message),
    }
}

#[cfg(test)]
mod tests;
