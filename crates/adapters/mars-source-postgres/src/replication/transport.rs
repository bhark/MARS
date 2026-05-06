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
//! next subscribe (SPEC §8.3 / compiler `run` invariant).

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
/// pgwire-replication maintains its own buffer in front of this; we keep
/// ours small because the compiler drives a windowed batching loop and
/// therefore does not benefit from deep buffering here.
const BATCH_CHANNEL_CAPACITY: usize = 64;

/// pgwire-replication's internal worker buffer. Has to be sized to absorb
/// a burst of XLogData frames between commits without blocking the wire.
const WORKER_EVENT_BUFFER: usize = 8192;

/// Spawn the replication subscriber task and return the ack-aware subscription.
pub(crate) async fn run(
    cfg: Arc<PgConfig>,
    topology: Arc<ReplicationTopology>,
) -> Result<Box<dyn ChangeSubscription>, SourceError> {
    let repl_cfg = build_replication_config(&cfg)?;
    let client = ReplicationClient::connect(repl_cfg).await.map_err(|e| {
        SourceError::Backend(format!(
            "replication connect: {e} (slot={}, publication={})",
            cfg.slot, cfg.publication
        ))
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
    };
    let join = tokio::spawn(worker.run());

    Ok(Box::new(PgOutputSubscription {
        rx: batch_rx,
        applied_tx,
        cancel,
        join: Some(join),
    }))
}

struct PgOutputSubscription {
    rx: mpsc::Receiver<Result<ChangeBatch, SourceError>>,
    applied_tx: watch::Sender<u64>,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
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
        let lsn = Lsn::from_str(s).map_err(|e| SourceError::Backend(format!("ack: invalid LSN {s:?}: {e}")))?;
        // monotonic; watch::send always succeeds while a receiver lives.
        let _ = self.applied_tx.send(lsn.as_u64());
        Ok(())
    }
}

impl Drop for PgOutputSubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
        // detach the worker; `cancel` triggers a graceful CopyDone inside the
        // pgwire client's stop() call. abort would slam the socket shut and
        // potentially lose an in-flight feedback message.
        if let Some(handle) = self.join.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(async move {
                        let _ = handle.await;
                    });
                }
                Err(_) => handle.abort(),
            }
        }
    }
}

struct Worker {
    client: ReplicationClient,
    topology: Arc<ReplicationTopology>,
    batch_tx: mpsc::Sender<Result<ChangeBatch, SourceError>>,
    applied_rx: watch::Receiver<u64>,
    cancel: CancellationToken,
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
                        // sender dropped — subscription is gone. proceed to cancel
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
                            .send(Err(SourceError::Backend(format!("replication: {e}"))))
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
                self.batch_tx.send(Ok(batch)).await.map_err(|_| ())
            }
            ReplicationEvent::XLogData { data, .. } => {
                let msg = match pgoutput::decode(&data) {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = self
                            .batch_tx
                            .send(Err(SourceError::Backend(format!("pgoutput decode: {e}"))))
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
    let pg_cfg = tokio_postgres::Config::from_str(&cfg.dsn).map_err(|e| SourceError::Backend(format!("dsn: {e}")))?;

    let user = pg_cfg
        .get_user()
        .ok_or_else(|| SourceError::Backend("dsn: missing user".into()))?
        .to_string();
    let password = pg_cfg
        .get_password()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let database = pg_cfg
        .get_dbname()
        .ok_or_else(|| SourceError::Backend("dsn: missing dbname".into()))?
        .to_string();

    let host = pg_cfg
        .get_hosts()
        .iter()
        .find_map(|h| match h {
            tokio_postgres::config::Host::Tcp(s) => Some(s.clone()),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(p) => p.to_str().map(|s| s.to_string()),
        })
        .ok_or_else(|| SourceError::Backend("dsn: no usable host".into()))?;
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
            return Err(SourceError::Backend(format!(
                "dsn: unsupported sslmode for replication: {other:?}"
            )));
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
        // SPEC §8.3 ack semantics: this is what makes replay-on-reconnect work
        // without us needing to remember the cursor anywhere except the slot.
        start_lsn: Lsn::ZERO,
        stop_at_lsn: None,
        buffer_events: WORKER_EVENT_BUFFER,
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn cfg(dsn: &str) -> PgConfig {
        PgConfig {
            dsn: dsn.into(),
            publication: "p".into(),
            slot: "s".into(),
            ..Default::default()
        }
    }

    #[test]
    fn build_config_parses_uri_dsn() {
        let r = build_replication_config(&cfg("postgres://alice:secret@db.example:6543/forv")).unwrap();
        assert_eq!(r.host, "db.example");
        assert_eq!(r.port, 6543);
        assert_eq!(r.user, "alice");
        assert_eq!(r.password, "secret");
        assert_eq!(r.database, "forv");
        assert_eq!(r.slot, "s");
        assert_eq!(r.publication, "p");
    }

    #[test]
    fn build_config_maps_sslmode_disable() {
        let r = build_replication_config(&cfg("postgres://u:p@h/d?sslmode=disable")).unwrap();
        assert_eq!(r.tls.mode, PgwireSslMode::Disable);
    }

    #[test]
    fn build_config_maps_sslmode_require() {
        let r = build_replication_config(&cfg("postgres://u:p@h/d?sslmode=require")).unwrap();
        assert_eq!(r.tls.mode, PgwireSslMode::Require);
    }

    #[test]
    fn build_config_rejects_missing_user() {
        let err = build_replication_config(&cfg("postgres://h/d")).unwrap_err();
        match err {
            SourceError::Backend(m) => assert!(m.contains("user"), "msg = {m}"),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn build_config_rejects_missing_dbname() {
        let err = build_replication_config(&cfg("postgres://u:p@h/")).unwrap_err();
        match err {
            SourceError::Backend(m) => assert!(m.contains("dbname"), "msg = {m}"),
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
