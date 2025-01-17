use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_raft::raft::VoteRequest;
use async_raft::Config;
use async_raft::LogId;
use async_raft::RaftNetwork;
use async_raft::ReplicationMetrics;
use async_raft::State;
use fixtures::RaftRouter;
use futures::stream::StreamExt;
use maplit::btreeset;
use maplit::hashmap;
#[allow(unused_imports)]
use pretty_assertions::assert_eq;
#[allow(unused_imports)]
use pretty_assertions::assert_ne;

#[macro_use]
mod fixtures;

/// Cluster leader_metrics test.
///
/// What does this test do?
///
/// - brings 5 nodes online: one leader and 4 non-voter.
/// - add 4 non-voter as follower.
/// - asserts that the leader was able to successfully commit logs and that the followers has successfully replicated
///   the payload.
/// - remove one folower: node-4
/// - asserts node-4 becomes non-voter and the leader stops sending logs to it.
///
/// RUST_LOG=async_raft,memstore,leader_metrics=trace cargo test -p async-raft --test leader_metrics
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn leader_metrics() -> Result<()> {
    let (_log_guard, ut_span) = init_ut!();
    let _ent = ut_span.enter();

    let span = tracing::debug_span!("leader_metrics");
    let _ent = span.enter();

    let timeout = Some(Duration::from_millis(1000));
    let all_members = btreeset![0, 1, 2, 3, 4];
    let left_members = btreeset![0, 1, 2, 3];

    // Setup test dependencies.
    let config = Arc::new(Config::build("test".into()).validate().expect("failed to build Raft config"));
    let router = Arc::new(RaftRouter::new(config.clone()));
    router.new_raft_node(0).await;

    // Assert all nodes are in non-voter state & have no entries.
    let mut want = 0;
    router.wait_for_log(&btreeset![0], want, timeout, "init").await?;
    router.wait_for_state(&btreeset![0], State::NonVoter, timeout, "init").await?;

    router.assert_pristine_cluster().await;

    tracing::info!("--- initializing cluster");

    router.initialize_from_single_node(0).await?;
    want += 1;

    router.wait_for_log(&btreeset![0], want, timeout, "init cluster").await?;
    router.assert_stable_cluster(Some(1), Some(want)).await;

    router
        .wait_for_metrics(
            &0,
            |x| {
                if let Some(ref q) = x.leader_metrics {
                    q.replication.is_empty()
                } else {
                    false
                }
            },
            timeout,
            "no replication with 1 node cluster",
        )
        .await?;

    // Sync some new nodes.
    router.new_raft_node(1).await;
    router.new_raft_node(2).await;
    router.new_raft_node(3).await;
    router.new_raft_node(4).await;

    tracing::info!("--- adding 4 new nodes to cluster");

    let mut new_nodes = futures::stream::FuturesUnordered::new();
    new_nodes.push(router.add_non_voter(0, 1));
    new_nodes.push(router.add_non_voter(0, 2));
    new_nodes.push(router.add_non_voter(0, 3));
    new_nodes.push(router.add_non_voter(0, 4));
    while let Some(inner) = new_nodes.next().await {
        inner?;
    }

    router.wait_for_log(&all_members, want, timeout, "add non-voter 1,2,3,4").await?;

    tracing::info!("--- changing cluster config to 012");

    router.change_membership(0, all_members.clone()).await?;
    want += 2; // 2 member-change logs

    router.wait_for_log(&all_members, want, timeout, "change members to 0,1,2,3,4").await?;

    router.assert_stable_cluster(Some(1), Some(want)).await; // Still in term 1, so leader is still node 0.

    let ww = ReplicationMetrics {
        matched: LogId { term: 1, index: want },
    };
    let want_repl = hashmap! { 1=>ww.clone(), 2=>ww.clone(), 3=>ww.clone(), 4=>ww.clone(), };
    router
        .wait_for_metrics(
            &0,
            |x| {
                if let Some(ref q) = x.leader_metrics {
                    q.replication == want_repl
                } else {
                    false
                }
            },
            timeout,
            "replication metrics to 4 nodes",
        )
        .await?;

    // Send some requests
    router.client_request_many(0, "client", 10).await;
    want += 10;

    tracing::info!("--- remove n{}", 4);

    router.change_membership(0, left_members.clone()).await?;
    want += 2; // two member-change logs

    router
        .wait_for_metrics(
            &4,
            |x| x.state == State::NonVoter,
            timeout,
            &format!("n{}.state -> {:?}", 4, State::NonVoter),
        )
        .await?;

    router.wait_for_log(&left_members, want, timeout, "removed node 4").await?;

    let ww = ReplicationMetrics {
        matched: LogId { term: 1, index: want },
    };
    let want_repl = hashmap! { 1=>ww.clone(), 2=>ww.clone(), 3=>ww.clone()};
    router
        .wait_for_metrics(
            &0,
            |x| {
                if let Some(ref q) = x.leader_metrics {
                    q.replication == want_repl
                } else {
                    false
                }
            },
            timeout,
            "replication metrics to 3 nodes",
        )
        .await?;

    tracing::info!("--- take leadership of node 0");

    router
        .send_vote(0, VoteRequest {
            term: 100,
            candidate_id: 100,
            last_log_index: 100,
            last_log_term: 10,
        })
        .await?;

    router.wait_for_state(&btreeset![0], State::Candidate, timeout, "node 0 to candidate").await?;

    router
        .wait_for_metrics(
            &0,
            |x| x.leader_metrics.is_none(),
            timeout,
            "node 0 should close all replication",
        )
        .await?;

    Ok(())
}
