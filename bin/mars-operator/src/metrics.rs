//! Operator-side metrics and health endpoints. Distinct from
//! `mars_observability::Metrics` (which is the data-plane facade) - the
//! operator reports its own reconcile counters, latencies, and the size of
//! its watched object set.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus::{Encoder, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder};

pub(crate) const RECONCILE_TOTAL: &str = "mars_operator_reconcile_total";
pub(crate) const RECONCILE_DURATION: &str = "mars_operator_reconcile_duration_seconds";
pub(crate) const RECONCILE_ERRORS: &str = "mars_operator_reconcile_errors_total";

#[derive(Clone)]
pub(crate) struct Metrics {
    pub(crate) registry: Arc<Registry>,
    pub(crate) reconcile_total: IntCounterVec,
    pub(crate) reconcile_errors: IntCounterVec,
    pub(crate) reconcile_duration: HistogramVec,
}

impl Metrics {
    pub(crate) fn new() -> Result<Self, prometheus::Error> {
        let registry = Arc::new(Registry::new());
        let reconcile_total = IntCounterVec::new(
            Opts::new(RECONCILE_TOTAL, "Reconcile attempts per outcome"),
            &["outcome"],
        )?;
        let reconcile_errors = IntCounterVec::new(
            Opts::new(RECONCILE_ERRORS, "Reconcile failures by error kind"),
            &["kind"],
        )?;
        let reconcile_duration = HistogramVec::new(
            HistogramOpts::new(RECONCILE_DURATION, "Reconcile duration")
                .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]),
            &["outcome"],
        )?;
        registry.register(Box::new(reconcile_total.clone()))?;
        registry.register(Box::new(reconcile_errors.clone()))?;
        registry.register(Box::new(reconcile_duration.clone()))?;
        Ok(Self {
            registry,
            reconcile_total,
            reconcile_errors,
            reconcile_duration,
        })
    }

    pub(crate) fn record(&self, outcome: &str, duration: Duration) {
        self.reconcile_total.with_label_values(&[outcome]).inc();
        self.reconcile_duration
            .with_label_values(&[outcome])
            .observe(duration.as_secs_f64());
    }

    pub(crate) fn record_error(&self, kind: &str) {
        self.reconcile_errors.with_label_values(&[kind]).inc();
    }
}

/// Spawn the /metrics + /healthz/readyz servers. Returns a JoinHandle the
/// caller can park; the runtime owns the listener.
pub(crate) async fn serve(metrics: Metrics, metrics_addr: SocketAddr, health_addr: SocketAddr) -> anyhow::Result<()> {
    let metrics_app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let health_app: Router = Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route("/readyz", get(|| async { (StatusCode::OK, "ok") }));

    let metrics_listener = tokio::net::TcpListener::bind(metrics_addr).await?;
    let health_listener = tokio::net::TcpListener::bind(health_addr).await?;

    let m = tokio::spawn(async move {
        let _ = axum::serve(metrics_listener, metrics_app).await;
    });
    let h = tokio::spawn(async move {
        let _ = axum::serve(health_listener, health_app).await;
    });

    let _ = tokio::try_join!(m, h);
    Ok(())
}

async fn metrics_handler(State(state): State<Metrics>) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    let metric_families = state.registry.gather();
    if let Err(e) = encoder.encode(&metric_families, &mut buf) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("encode error: {e}"));
    }
    let body = String::from_utf8(buf).unwrap_or_default();
    (StatusCode::OK, body)
}
