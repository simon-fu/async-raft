use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use maplit::hashset;

use async_raft::{Config, State};
use fixtures::RaftRouter;

mod fixtures;

/// Cluster total_order_apply test.
///
/// What does this test do?
///
/// - brings 2 nodes online: one leader and one non-voter.
/// - write one log to the leader.
/// - asserts that when metrics.last_applied is upto date, the state machine should be upto date
///   too.
///
/// RUST_LOG=async_raft,memstore,total_order_apply=trace cargo test -p async-raft --test total_order_apply
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn total_order_apply() -> Result<()> {
    fixtures::init_tracing();

    // Setup test dependencies.
    let config = Arc::new(Config::build("test".into()).validate().expect("failed to build Raft config"));
    let router = Arc::new(RaftRouter::new(config.clone()));

    router.new_raft_node(0).await;
    router.new_raft_node(1).await;

    tracing::info!("--- initializing single node cluster");

    // Wait for node 0 to become leader.
    router.initialize_with(0, hashset![0]).await?;
    router
        .wait_for_metrics(&0u64, |x| x.state == State::Leader, Duration::from_micros(100), "n0.state -> Leader")
        .await?;

    tracing::info!("--- add one non-voter");
    router.add_non_voter(0, 1).await?;

    let (tx, rx) = tokio::sync::watch::channel(false);

    let sto = router.get_storage_handle(&1).await?;

    let mut prev = 0;
    let h = tokio::spawn(async move {
        loop {
            if rx.borrow().clone() == true {
                break;
            }

            let last;
            {
                let sm = sto.get_state_machine().await;
                last = sm.last_applied_log;
            }

            if last < prev {
                panic!("out of order apply");
            }
            prev = last;
        }

        ()
    });

    h.await?;

    let n = 1000_000;
    router.client_request_many(0, "foo", n).await;

    let want = n as u64;
    router
        .wait_for_metrics(
            &1u64,
            |x| x.last_applied >= want,
            Duration::from_millis(1000),
            &format!("n{}.last_applied -> {}", 1, want),
        )
        .await?;

    tx.send(true)?;

    Ok(())
}
