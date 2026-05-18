//! CR status computation. Encodes the kubectl-rollout-status readiness check
//! so callers (the user, ArgoCD, the chart smoke test) see a faithful Ready
//! signal rather than the raw replica counts.

use k8s_openapi::api::apps::v1::Deployment;

use crate::crd::spec::{Condition, MarsServiceStatus};

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
    Condition {
        type_: type_.into(),
        status: if status { "True".into() } else { "False".into() },
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
    }
}
