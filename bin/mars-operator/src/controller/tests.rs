#![allow(clippy::unwrap_used)]

use super::*;

// grace must be strictly less than duration; the LeaseManager rejects
// builds otherwise and we want to catch a misconfig at compile time.
const _: () = assert!(LEASE_GRACE_SECS < LEASE_DURATION_SECS);

#[tokio::test]
async fn wait_for_lease_loss_returns_on_transition_to_false() {
    let (tx, rx) = watch::channel(true);
    let waiter = tokio::spawn(wait_for_lease_loss(Some(rx)));
    tx.send(false).unwrap();
    waiter.await.unwrap();
}

#[tokio::test]
async fn wait_for_lease_loss_returns_on_channel_close() {
    let (tx, rx) = watch::channel(true);
    let waiter = tokio::spawn(wait_for_lease_loss(Some(rx)));
    drop(tx);
    waiter.await.unwrap();
}
