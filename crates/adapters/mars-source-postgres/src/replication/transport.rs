//! pgoutput replication transport.
//!
//! Owns the network I/O for one logical-replication subscription.
//! `pgwire-replication` does the wire-protocol heavy lifting (startup with
//! `replication=database`, SCRAM/MD5 auth, TLS upgrade, CopyBoth framing,
//! periodic standby status updates). We layer the MARS-owned pgoutput
//! decoder + translator on top: each `XLogData` payload becomes zero or
//! more `ChangeEvent`s, and a pgoutput `Commit` flushes the per-transaction
//! buffer as one `ChangeBatch` carrying the commit's `end_lsn` as
//! `source_version` (`X/Y` PostgreSQL hex).
//!
//! Ack flow: `acknowledge(source_version)` parses the LSN and pushes it
//! into a `watch` channel. The worker forwards it to
//! `ReplicationClient::update_applied_lsn`, which stores the value in an
//! atomic; the library's own loop sends a standby status update at most
//! every `status_interval` (or immediately when the server requests a
//! reply). Crash between `next_batch` and `acknowledge` simply leaves
//! `confirmed_flush_lsn` un-advanced, which replays the window on the
//! next subscribe (compiler `run` invariant).

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use mars_source::{ChangeBatch, ChangeEvent, ChangeSubscription, SourceError};
use pgwire_replication::{
    Lsn, ReplicationClient, ReplicationConfig, ReplicationEvent, SslMode as PgwireSslMode, TlsConfig,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::translate::{Translated, translate};
use super::{RelationCache, ReplicationTopology, pgoutput};
use crate::PgConfig;

/// Per-commit batch buffer size handed to the consumer-side `mpsc`.
/// pgwire-replication maintains its own buffer in front of this; keep
/// ours small because the compiler drives a windowed batching loop and
/// therefore does not benefit from deep buffering here.
const BATCH_CHANNEL_CAPACITY: usize = 64;

/// pgwire-replication's internal worker buffer. Has to be sized to absorb
/// a burst of XLogData frames between commits without blocking the wire.
const WORKER_EVENT_BUFFER: usize = 8192;

/// How often the library flushes the latest applied/flush LSN to the server
/// as a standby status update. The library default (10s) is fine for batch
/// CDC but is too lazy for our ack semantics - between ack and disconnect
/// we must have flushed the cursor or the next subscription will replay an
/// already-persisted window. One second balances quick advancement with
/// keepalive overhead.
const STATUS_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long the library parks in a socket read before waking up to send
/// a periodic status update. Must be <= STATUS_FLUSH_INTERVAL or the
/// worker may sleep through several intervals when the stream is idle and
/// no acks would be flushed in time for a graceful shutdown.
const IDLE_WAKEUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Upper bound on how long a committed batch may sit in the worker waiting
/// for the consumer (compiler) to drain. Past this window the slot would
/// otherwise stall and pg WAL would accumulate without bound. Hitting it
/// fails the subscription so the compiler resets rather than wedging.
pub(crate) const DEFAULT_BATCH_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Spawn the replication subscriber task and return the ack-aware subscription.
pub(crate) async fn run(
    cfg: Arc<PgConfig>,
    topology: Arc<ReplicationTopology>,
) -> Result<Box<dyn ChangeSubscription>, SourceError> {
    let repl_cfg = build_replication_config(&cfg)?;
    let client = ReplicationClient::connect(repl_cfg).await.map_err(|e| {
        SourceError::backend_msg(
            "replication connect",
            format!("{e} (slot={}, publication={})", cfg.slot, cfg.publication),
        )
    })?;

    let (batch_tx, batch_rx) = mpsc::channel::<Result<ChangeBatch, SourceError>>(BATCH_CHANNEL_CAPACITY);
    let (applied_tx, applied_rx) = watch::channel::<u64>(0);
    let cancel = CancellationToken::new();

    let worker = Worker {
        client,
        topology,
        batch_tx,
        applied_rx,
        cancel: cancel.clone(),
        batch_send_timeout: cfg.batch_send_timeout.unwrap_or(DEFAULT_BATCH_SEND_TIMEOUT),
    };
    let join = tokio::spawn(worker.run());

    Ok(Box::new(PgOutputSubscription {
        rx: batch_rx,
        applied_tx,
        cancel,
        join: Some(join),
        slot: cfg.slot.clone(),
        shutdown_called: false,
    }))
}

struct PgOutputSubscription {
    rx: mpsc::Receiver<Result<ChangeBatch, SourceError>>,
    applied_tx: watch::Sender<u64>,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    slot: String,
    shutdown_called: bool,
}

#[async_trait]
impl ChangeSubscription for PgOutputSubscription {
    async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>> {
        self.rx.recv().await
    }

    async fn acknowledge(&mut self, source_version: Option<&str>) -> Result<(), SourceError> {
        let Some(s) = source_version else {
            return Ok(());
        };
        let lsn = Lsn::from_str(s).map_err(|e| SourceError::backend_msg("ack", format!("invalid LSN {s:?}: {e}")))?;
        // watch::send fails only when no receiver remains - i.e. the worker has
        // exited. silently dropping that ack would leave the caller believing
        // the LSN is durable while the slot stays pinned at the old position.
        self.applied_tx
            .send(lsn.as_u64())
            .map_err(|_| SourceError::backend_msg("ack", "replication worker has exited"))?;
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), SourceError> {
        // cancel triggers a graceful CopyDone inside the pgwire client's stop()
        // call; awaiting the worker guarantees the final feedback message is
        // on the wire before we return.
        self.cancel.cancel();
        // mark before the await: callers reached the graceful path, so drop
        // should stay silent even if the join below surfaces a worker error.
        self.shutdown_called = true;
        if let Some(handle) = self.join.take()
            && let Err(e) = handle.await
            && !e.is_cancelled()
        {
            return Err(SourceError::backend_msg("replication worker join", e.to_string()));
        }
        Ok(())
    }
}

impl Drop for PgOutputSubscription {
    fn drop(&mut self) {
        // fallback for callers that did not invoke `shutdown` first: cancel and
        // abort. ack durability is only guaranteed on the explicit shutdown
        // path; aborting here may drop an in-flight feedback message.
        if !self.shutdown_called {
            tracing::warn!(
                slot = %self.slot,
                "pgoutput subscription dropped without shutdown; in-flight ack may be lost \
                 and the next subscribe could replay an already-persisted window",
            );
        }
        self.cancel.cancel();
        if let Some(handle) = self.join.take() {
            handle.abort();
        }
    }
}

struct Worker {
    client: ReplicationClient,
    topology: Arc<ReplicationTopology>,
    batch_tx: mpsc::Sender<Result<ChangeBatch, SourceError>>,
    applied_rx: watch::Receiver<u64>,
    cancel: CancellationToken,
    batch_send_timeout: std::time::Duration,
}

impl Worker {
    async fn run(mut self) {
        let mut cache = RelationCache::default();
        let mut pending: Vec<ChangeEvent> = Vec::new();

        loop {
            tokio::select! {
                biased;

                _ = self.cancel.cancelled() => {
                    self.client.stop();
                    break;
                }

                changed = self.applied_rx.changed() => {
                    if changed.is_err() {
                        // sender dropped - subscription is gone. proceed to cancel
                        // path on next iteration.
                        continue;
                    }
                    let v = *self.applied_rx.borrow_and_update();
                    if v != 0 {
                        self.client.update_applied_lsn(Lsn::from_u64(v));
                    }
                }

                ev = self.client.recv() => match ev {
                    Ok(None) => break,
                    Err(e) => {
                        let _ = self
                            .batch_tx
                            .send(Err(SourceError::backend_msg("replication", e.to_string())))
                            .await;
                        break;
                    }
                    Ok(Some(event)) => {
                        if self
                            .handle_event(event, &mut cache, &mut pending)
                            .await
                            .is_err()
                        {
                            // batch_tx closed or fatal translation error already
                            // reported on the channel; either way the loop exits.
                            break;
                        }
                    }
                },
            }
        }
    }

    async fn handle_event(
        &mut self,
        event: ReplicationEvent,
        cache: &mut RelationCache,
        pending: &mut Vec<ChangeEvent>,
    ) -> Result<(), ()> {
        match event {
            ReplicationEvent::Begin { .. } => {
                pending.clear();
                Ok(())
            }
            ReplicationEvent::Commit { end_lsn, .. } => {
                let events = std::mem::take(pending);
                let batch = ChangeBatch {
                    events,
                    source_version: Some(end_lsn.to_string()),
                };
                // bounded send: if the consumer stalls past the budget we abort the
                // subscription rather than block, which would pin the slot and let
                // pg WAL grow without bound.
                match tokio::time::timeout(self.batch_send_timeout, self.batch_tx.send(Ok(batch))).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err(()),
                    Err(_) => {
                        tracing::error!(
                            timeout_secs = self.batch_send_timeout.as_secs(),
                            "replication: batch send stalled; aborting subscription"
                        );
                        let _ = self
                            .batch_tx
                            .send(Err(SourceError::backend_msg(
                                "batch send stalled",
                                format!("past {:?}; consumer not draining", self.batch_send_timeout),
                            )))
                            .await;
                        Err(())
                    }
                }
            }
            ReplicationEvent::XLogData { data, .. } => {
                let msg = match pgoutput::decode(&data) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = self
                            .batch_tx
                            .send(Err(SourceError::backend_msg("pgoutput decode", e.to_string())))
                            .await;
                        return Err(());
                    }
                };
                match translate(msg, cache, &self.topology) {
                    Ok(Translated(es)) => {
                        pending.extend(es);
                        Ok(())
                    }
                    Err(e) => {
                        let _ = self.batch_tx.send(Err(e)).await;
                        Err(())
                    }
                }
            }
            // server heartbeats: pgwire-replication already replied if needed.
            ReplicationEvent::KeepAlive { .. } => Ok(()),
            // unused: logical_emit_message + bounded-replay stop sentinel.
            ReplicationEvent::Message { .. } | ReplicationEvent::StoppedAt { .. } => Ok(()),
        }
    }
}

/// Translate a libpq-shaped DSN plus our slot/publication into the shape
/// `pgwire-replication` expects. Multi-host DSNs use the first host; SSL
/// mode is the libpq subset (Disable/Prefer/Require). VerifyCa/VerifyFull
/// are not expressible via libpq DSN today.
fn build_replication_config(cfg: &PgConfig) -> Result<ReplicationConfig, SourceError> {
    let pg_cfg = tokio_postgres::Config::from_str(&cfg.dsn).map_err(|e| SourceError::backend("dsn", e))?;

    let user = pg_cfg
        .get_user()
        .ok_or_else(|| SourceError::backend_msg("dsn", "missing user"))?
        .to_string();
    let password = pg_cfg
        .get_password()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let database = pg_cfg
        .get_dbname()
        .ok_or_else(|| SourceError::backend_msg("dsn", "missing dbname"))?
        .to_string();

    let host = pg_cfg
        .get_hosts()
        .iter()
        .find_map(|h| match h {
            tokio_postgres::config::Host::Tcp(s) => Some(s.clone()),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(p) => p.to_str().map(|s| s.to_string()),
        })
        .ok_or_else(|| SourceError::backend_msg("dsn", "no usable host"))?;
    let port = pg_cfg.get_ports().first().copied().unwrap_or(5432);

    let tls = match pg_cfg.get_ssl_mode() {
        tokio_postgres::config::SslMode::Disable => TlsConfig {
            mode: PgwireSslMode::Disable,
            ..Default::default()
        },
        tokio_postgres::config::SslMode::Prefer => TlsConfig {
            mode: PgwireSslMode::Prefer,
            ..Default::default()
        },
        tokio_postgres::config::SslMode::Require => TlsConfig {
            mode: PgwireSslMode::Require,
            ..Default::default()
        },
        // tokio-postgres only parses Disable/Prefer/Require from a libpq
        // DSN today. anything stronger means the SslMode enum grew a new
        // variant we should explicitly map rather than silently downgrade.
        other => {
            return Err(SourceError::backend_msg(
                "dsn",
                format!("unsupported sslmode for replication: {other:?}"),
            ));
        }
    };

    Ok(ReplicationConfig {
        host,
        port,
        user,
        password,
        database,
        tls,
        slot: cfg.slot.clone(),
        publication: cfg.publication.clone(),
        // Lsn(0) tells the server to resume from the slot's confirmed_flush_lsn.
        // ack semantics: this is what makes replay-on-reconnect work
        // without needing to remember the cursor anywhere except the slot.
        start_lsn: Lsn::ZERO,
        stop_at_lsn: None,
        buffer_events: WORKER_EVENT_BUFFER,
        status_interval: STATUS_FLUSH_INTERVAL,
        idle_wakeup_interval: IDLE_WAKEUP_INTERVAL,
    })
}

#[cfg(test)]
mod tests;
