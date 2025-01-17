//! Replication stream.

use std::io::SeekFrom;
use std::sync::Arc;

use futures::future::FutureExt;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeek;
use tokio::io::AsyncSeekExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
// use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio::time::timeout;
use tokio::time::Duration;
use tokio::time::Interval;
use tracing::Instrument;
use tracing::Span;

use crate::config::Config;
use crate::config::SnapshotPolicy;
use crate::error::RaftResult;
use crate::raft::AppendEntriesRequest;
use crate::raft::Entry;
use crate::raft::EntryPayload;
use crate::raft::InstallSnapshotRequest;
use crate::storage::Snapshot;
use crate::AppData;
use crate::AppDataResponse;
use crate::LogId;
use crate::MessageSummary;
use crate::NodeId;
use crate::RaftNetwork;
use crate::RaftStorage;

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicationMetrics {
    pub matched: LogId,
}

/// The public handle to a spawned replication stream.
pub(crate) struct ReplicationStream<D: AppData> {
    /// The spawn handle the `ReplicationCore` task.
    // pub handle: JoinHandle<()>,
    /// The channel used for communicating with the replication task.
    pub repl_tx: mpsc::UnboundedSender<(RaftEvent<D>, Span)>,
}

impl<D: AppData> ReplicationStream<D> {
    /// Create a new replication stream for the target peer.
    pub(crate) fn new<R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>>(
        id: NodeId,
        target: NodeId,
        term: u64,
        config: Arc<Config>,
        last_log: LogId,
        commit_index: u64,
        network: Arc<N>,
        storage: Arc<S>,
        replication_tx: mpsc::UnboundedSender<(ReplicaEvent<S::SnapshotData>, Span)>,
    ) -> Self {
        ReplicationCore::spawn(
            id,
            target,
            term,
            config,
            last_log,
            commit_index,
            network,
            storage,
            replication_tx,
        )
    }
}

/// A task responsible for sending replication events to a target follower in the Raft cluster.
///
/// NOTE: we do not stack replication requests to targets because this could result in
/// out-of-order delivery. We always buffer until we receive a success response, then send the
/// next payload from the buffer.
struct ReplicationCore<D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> {
    //////////////////////////////////////////////////////////////////////////
    // Static Fields /////////////////////////////////////////////////////////
    /// The ID of this Raft node.
    id: NodeId,
    /// The ID of the target Raft node which replication events are to be sent to.
    target: NodeId,
    /// The current term, which will never change during the lifetime of this task.
    term: u64,

    /// A channel for sending events to the Raft node.
    raft_core_tx: mpsc::UnboundedSender<(ReplicaEvent<S::SnapshotData>, Span)>,

    /// A channel for receiving events from the Raft node.
    repl_rx: mpsc::UnboundedReceiver<(RaftEvent<D>, Span)>,

    /// The `RaftNetwork` interface.
    network: Arc<N>,

    /// The `RaftStorage` interface.
    storage: Arc<S>,

    /// The Raft's runtime config.
    config: Arc<Config>,
    /// The configured max payload entries, simply as a usize.
    max_payload_entries: usize,
    marker_r: std::marker::PhantomData<R>,

    //////////////////////////////////////////////////////////////////////////
    // Dynamic Fields ////////////////////////////////////////////////////////
    /// The target state of this replication stream.
    target_state: TargetReplState,

    /// The index of the log entry to most recently be appended to the log by the leader.
    last_log_index: u64,
    /// The index of the highest log entry which is known to be committed in the cluster.
    commit_index: u64,

    /// The index of the next log to send.
    ///
    /// This is initialized to leader's last log index + 1. Per the Raft protocol spec,
    /// this value may be decremented as new nodes enter the cluster and need to catch-up per the
    /// log consistency check.
    ///
    /// If a follower's log is inconsistent with the leader's, the AppendEntries consistency check
    /// will fail in the next AppendEntries RPC. After a rejection, the leader decrements
    /// `next_index` and retries the AppendEntries RPC. Eventually `next_index` will reach a point
    /// where the leader and follower logs match. When this happens, AppendEntries will succeed,
    /// which removes any conflicting entries in the follower's log and appends entries from the
    /// leader's log (if any). Once AppendEntries succeeds, the follower’s log is consistent with
    /// the leader's, and it will remain that way for the rest of the term.
    ///
    /// This Raft implementation also uses a _conflict optimization_ pattern for reducing the
    /// number of RPCs which need to be sent back and forth between a peer which is lagging
    /// behind. This is defined in §5.3.
    next_index: u64,
    /// The last know log to be successfully replicated on the target.
    ///
    /// This will be initialized to the leader's (last_log_term, last_log_index), and will be updated as
    /// replication proceeds.
    /// TODO(xp): initialize to last_log_index? should be a zero value?
    matched: LogId,

    /// A buffer of data to replicate to the target follower.
    ///
    /// The buffered payload here will be expanded as more replication commands come in from the
    /// Raft node. Data from this buffer will flow into the `outbound_buffer` in chunks.
    replication_buffer: Vec<Arc<Entry<D>>>,
    /// A buffer of data which is being sent to the follower.
    ///
    /// Data in this buffer comes directly from the `replication_buffer` in chunks, and will
    /// remain here until it is confirmed that the payload has been successfully received by the
    /// target node. This allows for retransmission of payloads in the face of transient errors.
    outbound_buffer: Vec<OutboundEntry<D>>,
    /// The heartbeat interval for ensuring that heartbeats are always delivered in a timely fashion.
    heartbeat: Interval,

    // TODO(xp): collect configs in one struct.
    /// The timeout duration for heartbeats.
    heartbeat_timeout: Duration,

    /// The timeout for sending snapshot segment.
    install_snapshot_timeout: Duration,
}

impl<D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> ReplicationCore<D, R, N, S> {
    /// Spawn a new replication task for the target node.
    pub(self) fn spawn(
        id: NodeId,
        target: NodeId,
        term: u64,
        config: Arc<Config>,
        last_log: LogId,
        commit_index: u64,
        network: Arc<N>,
        storage: Arc<S>,
        raft_core_tx: mpsc::UnboundedSender<(ReplicaEvent<S::SnapshotData>, Span)>,
    ) -> ReplicationStream<D> {
        // other component to ReplicationStream
        let (repl_tx, repl_rx) = mpsc::unbounded_channel();
        let heartbeat_timeout = Duration::from_millis(config.heartbeat_interval);
        let install_snapshot_timeout = Duration::from_millis(config.install_snapshot_timeout);

        let max_payload_entries = config.max_payload_entries as usize;
        let this = Self {
            id,
            target,
            term,
            network,
            storage,
            config,
            max_payload_entries,
            marker_r: std::marker::PhantomData,
            target_state: TargetReplState::Lagging,
            last_log_index: last_log.index,
            commit_index,
            next_index: last_log.index + 1,
            matched: last_log,
            raft_core_tx,
            repl_rx,
            heartbeat: interval(heartbeat_timeout),
            heartbeat_timeout,
            install_snapshot_timeout,
            replication_buffer: Vec::new(),
            outbound_buffer: Vec::new(),
        };

        let _handle = tokio::spawn(this.main().instrument(tracing::debug_span!("spawn")));

        ReplicationStream {
            // handle,
            repl_tx,
        }
    }

    #[tracing::instrument(level="trace", skip(self), fields(id=self.id, target=self.target, cluster=%self.config.cluster_name))]
    async fn main(mut self) {
        // Perform an initial heartbeat.
        self.send_append_entries().await;

        // Proceed to the replication stream's inner loop.
        loop {
            match &self.target_state {
                TargetReplState::LineRate => self.line_rate_loop().await,
                TargetReplState::Lagging => self.lagging_loop().await,
                TargetReplState::Snapshotting => SnapshottingState::new(&mut self).run().await,
                TargetReplState::Shutdown => return,
            }
        }
    }

    /// Send an AppendEntries RPC to the target.
    ///
    /// This request will timeout if no response is received within the
    /// configured heartbeat interval.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn send_append_entries(&mut self) {
        // Attempt to fill the send buffer from the replication buffer.
        if self.outbound_buffer.is_empty() {
            let repl_len = self.replication_buffer.len();
            if repl_len > 0 {
                let chunk_size = if repl_len < self.max_payload_entries {
                    repl_len
                } else {
                    self.max_payload_entries
                };
                self.outbound_buffer.extend(self.replication_buffer.drain(..chunk_size).map(OutboundEntry::Arc));
            }
        }

        // Build the heartbeat frame to be sent to the follower.
        let payload = AppendEntriesRequest {
            term: self.term,
            leader_id: self.id,
            prev_log_id: self.matched,
            leader_commit: self.commit_index,
            entries: self.outbound_buffer.iter().map(|entry| entry.as_ref().clone()).collect(),
        };

        // Send the payload.
        tracing::debug!("start sending append_entries, timeout: {:?}", self.heartbeat_timeout);
        let res = match timeout(
            self.heartbeat_timeout,
            self.network.send_append_entries(self.target, payload),
        )
        .await
        {
            Ok(outer_res) => match outer_res {
                Ok(res) => res,
                Err(err) => {
                    tracing::warn!(error=%err, "error sending AppendEntries RPC to target");
                    return;
                }
            },
            Err(err) => {
                tracing::warn!(error=%err, "timeout while sending AppendEntries RPC to target");
                return;
            }
        };
        let last_log_id = self.outbound_buffer.last().map(|last| last.as_ref().log_id);

        // Once we've successfully sent a payload of entries, don't send them again.
        self.outbound_buffer.clear();

        tracing::debug!("append_entries last: {:?}", last_log_id);

        // Handle success conditions.
        if res.success {
            tracing::debug!("append entries succeeded to {:?}", last_log_id);

            // If this was a proper replication event (last index & term were provided), then update state.
            if let Some(log_id) = last_log_id {
                self.next_index = log_id.index + 1; // This should always be the next expected index.
                self.matched = log_id;
                let _ = self.raft_core_tx.send((
                    ReplicaEvent::UpdateMatchIndex {
                        target: self.target,
                        matched: log_id,
                    },
                    tracing::debug_span!("CH"),
                ));

                // If running at line rate, and our buffered outbound requests have accumulated too
                // much, we need to purge and transition to a lagging state. The target is not able to
                // replicate data fast enough.
                let is_lagging = self
                    .last_log_index
                    .checked_sub(self.matched.index)
                    .map(|diff| diff > self.config.replication_lag_threshold)
                    .unwrap_or(false);
                if is_lagging {
                    self.target_state = TargetReplState::Lagging;
                }
            }
            return;
        }

        // Replication was not successful, if a newer term has been returned, revert to follower.
        if res.term > self.term {
            tracing::debug!({ res.term }, "append entries failed, reverting to follower");
            let _ = self.raft_core_tx.send((
                ReplicaEvent::RevertToFollower {
                    target: self.target,
                    term: res.term,
                },
                tracing::debug_span!("CH"),
            ));
            self.target_state = TargetReplState::Shutdown;
            return;
        }

        // Replication was not successful, handle conflict optimization record, else decrement `next_index`.
        if let Some(conflict) = res.conflict_opt {
            tracing::debug!(?conflict, res.term, "append entries failed, handling conflict opt");

            // If the returned conflict opt index is greater than last_log_index, then this is a
            // logical error, and no action should be taken. This represents a replication failure.
            if conflict.log_id.index > self.last_log_index {
                return;
            }
            self.next_index = conflict.log_id.index + 1;
            self.matched = conflict.log_id;

            // If conflict index is 0, we will not be able to fetch that index from storage because
            // it will never exist. So instead, we just return, and accept the conflict data.
            if conflict.log_id.index == 0 {
                self.target_state = TargetReplState::Lagging;
                let _ = self.raft_core_tx.send((
                    ReplicaEvent::UpdateMatchIndex {
                        target: self.target,
                        matched: self.matched,
                    },
                    tracing::debug_span!("CH"),
                ));
                return;
            }

            // Fetch the entry at conflict index and use the term specified there.
            let ent = self.storage.try_get_log_entry(conflict.log_id.index).await;
            let ent = match ent {
                Ok(x) => x,
                Err(err) => {
                    tracing::error!(error=?err, "error fetching log entry due to returned AppendEntries RPC conflict_opt");
                    let _ = self.raft_core_tx.send((ReplicaEvent::Shutdown, tracing::debug_span!("CH")));
                    self.target_state = TargetReplState::Shutdown;
                    return;
                }
            };

            let ent_term = ent.map(|entry| entry.log_id.term);
            match ent_term {
                Some(term) => {
                    self.matched.term = term; // If we have the specified log, ensure we use its term.
                }
                None => {
                    // This condition would only ever be reached if the log has been removed due to
                    // log compaction (barring critical storage failure), so transition to snapshotting.
                    self.target_state = TargetReplState::Snapshotting;
                    let _ = self.raft_core_tx.send((
                        ReplicaEvent::UpdateMatchIndex {
                            target: self.target,
                            matched: self.matched,
                        },
                        tracing::debug_span!("CH"),
                    ));
                    return;
                }
            };

            // Check snapshot policy and handle conflict as needed.
            let _ = self.raft_core_tx.send((
                ReplicaEvent::UpdateMatchIndex {
                    target: self.target,
                    matched: self.matched,
                },
                tracing::debug_span!("CH"),
            ));
            match &self.config.snapshot_policy {
                SnapshotPolicy::LogsSinceLast(threshold) => {
                    let diff = self.last_log_index - conflict.log_id.index; // NOTE WELL: underflow is guarded against above.
                    if &diff >= threshold {
                        // Follower is far behind and needs to receive an InstallSnapshot RPC.
                        self.target_state = TargetReplState::Snapshotting;
                        return;
                    }
                    // Follower is behind, but not too far behind to receive an InstallSnapshot RPC.
                    self.target_state = TargetReplState::Lagging;
                    return;
                }
            }
        }
    }

    /// Perform a check to see if this replication stream is lagging behind far enough that a
    /// snapshot is warranted.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(self) fn needs_snapshot(&self) -> bool {
        match &self.config.snapshot_policy {
            SnapshotPolicy::LogsSinceLast(threshold) => {
                let needs_snap =
                    self.commit_index.checked_sub(self.matched.index).map(|diff| diff >= *threshold).unwrap_or(false);
                if needs_snap {
                    tracing::trace!("snapshot needed");
                    true
                } else {
                    tracing::trace!("snapshot not needed");
                    false
                }
            }
        }
    }

    /// Fully drain the channel coming in from the Raft node.
    pub(self) fn drain_raft_rx(&mut self, first: RaftEvent<D>, span: Span) {
        let mut event_opt = Some((first, span));
        let mut iters = 0;
        loop {
            // Just ensure we don't get stuck draining a REALLY hot replication feed.
            if iters > self.config.max_payload_entries {
                return;
            }

            // Unpack the event opt, else return if we don't have one to process.
            let (event, span) = match event_opt.take() {
                Some(event) => event,
                None => return,
            };

            let _ent = span.enter();

            // Process the event.
            match event {
                RaftEvent::UpdateCommitIndex { commit_index } => {
                    self.commit_index = commit_index;
                }

                RaftEvent::Replicate { entry, commit_index } => {
                    self.commit_index = commit_index;
                    self.last_log_index = entry.log_id.index;
                    if self.target_state == TargetReplState::LineRate {
                        self.replication_buffer.push(entry);
                    }
                }

                RaftEvent::Terminate => {
                    self.target_state = TargetReplState::Shutdown;
                    return;
                }
            }

            // Attempt to unpack the next event for the next loop iteration.
            if let Some(event_span) = self.repl_rx.recv().now_or_never() {
                event_opt = event_span;
            }
            iters += 1;
        }
    }
}

/// A type which wraps two possible forms of an outbound entry for replication.
enum OutboundEntry<D: AppData> {
    /// An entry owned by an Arc, hot off the replication stream from the Raft leader.
    Arc(Arc<Entry<D>>),
    /// An entry which was fetched directly from storage.
    Raw(Entry<D>),
}

impl<D: AppData> AsRef<Entry<D>> for OutboundEntry<D> {
    fn as_ref(&self) -> &Entry<D> {
        match self {
            Self::Arc(inner) => inner.as_ref(),
            Self::Raw(inner) => inner,
        }
    }
}

//////////////////////////////////////////////////////////////////////////////////////////////////

/// The state of the replication stream.
#[derive(Eq, PartialEq)]
enum TargetReplState {
    /// The replication stream is running at line rate.
    LineRate,
    /// The replication stream is lagging behind.
    Lagging,
    /// The replication stream is streaming a snapshot over to the target node.
    Snapshotting,
    /// The replication stream is shutting down.
    Shutdown,
}

/// An event from the Raft node.
pub(crate) enum RaftEvent<D: AppData> {
    Replicate {
        /// The new entry which needs to be replicated.
        ///
        /// This entry will always be the most recent entry to have been appended to the log, so its
        /// index is the new last_log_index value.
        entry: Arc<Entry<D>>,
        /// The index of the highest log entry which is known to be committed in the cluster.
        commit_index: u64,
    },
    /// A message from Raft indicating a new commit index value.
    UpdateCommitIndex {
        /// The index of the highest log entry which is known to be committed in the cluster.
        commit_index: u64,
    },
    Terminate,
}

/// An event coming from a replication stream.
pub(crate) enum ReplicaEvent<S>
where S: AsyncRead + AsyncSeek + Send + Unpin + 'static
{
    /// An event representing an update to the replication rate of a replication stream.
    RateUpdate {
        /// The ID of the Raft node to which this event relates.
        target: NodeId,
        /// A flag indicating if the corresponding target node is replicating at line rate.
        ///
        /// When replicating at line rate, the replication stream will receive log entries to
        /// replicate as soon as they are ready. When not running at line rate, the Raft node will
        /// only send over metadata without entries to replicate.
        is_line_rate: bool,
    },
    /// An event from a replication stream which updates the target node's match index.
    UpdateMatchIndex {
        /// The ID of the target node for which the match index is to be updated.
        target: NodeId,
        /// The log of the most recent log known to have been successfully replicated on the target.
        matched: LogId,
    },
    /// An event indicating that the Raft node needs to revert to follower state.
    RevertToFollower {
        /// The ID of the target node from which the new term was observed.
        target: NodeId,
        /// The new term observed.
        term: u64,
    },
    /// An event from a replication stream requesting snapshot info.
    NeedsSnapshot {
        /// The ID of the target node from which the event was sent.
        target: NodeId,
        /// The response channel for delivering the snapshot data.
        tx: oneshot::Sender<Snapshot<S>>,
    },
    /// Some critical error has taken place, and Raft needs to shutdown.
    Shutdown,
}

impl<S: AsyncRead + AsyncSeek + Send + Unpin + 'static> MessageSummary for ReplicaEvent<S> {
    fn summary(&self) -> String {
        match self {
            ReplicaEvent::RateUpdate {
                ref target,
                is_line_rate,
            } => {
                format!("RateUpdate: target: {}, is_line_rate: {}", target, is_line_rate)
            }
            ReplicaEvent::UpdateMatchIndex {
                ref target,
                ref matched,
            } => {
                format!("UpdateMatchIndex: target: {}, matched: {}", target, matched)
            }
            ReplicaEvent::RevertToFollower { ref target, ref term } => {
                format!("RevertToFollower: target: {}, term: {}", target, term)
            }
            ReplicaEvent::NeedsSnapshot { ref target, .. } => {
                format!("NeedsSnapshot: target: {}", target)
            }
            ReplicaEvent::Shutdown => "Shutdown".to_string(),
        }
    }
}

impl<D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> ReplicationCore<D, R, N, S> {
    #[tracing::instrument(level = "trace", skip(self), fields(state = "line-rate"))]
    pub async fn line_rate_loop(&mut self) {
        let event = ReplicaEvent::RateUpdate {
            target: self.target,
            is_line_rate: true,
        };
        let _ = self.raft_core_tx.send((event, tracing::debug_span!("CH")));
        loop {
            if self.target_state != TargetReplState::LineRate {
                return;
            }

            // We always prioritize draining our buffers first.
            let next_buf_index = self
                .outbound_buffer
                .first()
                .map(|entry| entry.as_ref().log_id.index)
                .or_else(|| self.replication_buffer.first().map(|entry| entry.log_id.index));

            // When converting to `LaggingState`, `outbound_buffer` and `replication_buffer` is cleared,
            // in which there may be uncommitted logs.
            // Thus when converting back to `LineRateState`, when these two buffers are empty, we
            // need to resend all uncommitted logs.
            // Otherwise these logs have no chance to be replicated, unless a new log is written.
            let index = match next_buf_index {
                Some(i) => i,
                None => self.last_log_index + 1,
            };

            // Ensure that our buffered data matches up with `next_index`. When transitioning to
            // line rate, it is always possible that new data has been sent for replication but has
            // skipped this replication stream during transition. In such cases, a single update from
            // storage will put this stream back on track.
            if self.next_index != index {
                self.frontload_outbound_buffer(self.next_index, index).await;
                if self.target_state != TargetReplState::LineRate {
                    return;
                }

                self.send_append_entries().await;
                continue;
            }

            let span = tracing::debug_span!("CHrx:LineRate");
            let _en = span.enter();

            tokio::select! {
                _ = self.heartbeat.tick() => self.send_append_entries().await,

                event_span = self.repl_rx.recv() => {
                    match event_span {
                        Some((event, span)) => self.drain_raft_rx(event, span),
                        None => self.target_state = TargetReplState::Shutdown,
                    }
                }
            }
        }
    }

    /// Ensure there are no gaps in the outbound buffer due to transition from lagging.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn frontload_outbound_buffer(&mut self, start: u64, stop: u64) {
        let entries = match self.storage.get_log_entries(start..stop).await {
            Ok(entries) => entries,
            Err(err) => {
                tracing::error!(error=%err, "error while frontloading outbound buffer");
                let _ = self.raft_core_tx.send((ReplicaEvent::Shutdown, tracing::debug_span!("CH")));
                return;
            }
        };

        for entry in entries.iter() {
            if let EntryPayload::PurgedMarker = entry.payload {
                self.target_state = TargetReplState::Snapshotting;
                return;
            }
        }

        // Prepend.
        self.outbound_buffer.reverse();
        self.outbound_buffer.extend(entries.into_iter().rev().map(OutboundEntry::Raw));
        self.outbound_buffer.reverse();
    }

    #[tracing::instrument(level = "trace", skip(self), fields(state = "lagging"))]
    pub async fn lagging_loop(&mut self) {
        let event = ReplicaEvent::RateUpdate {
            target: self.target,
            is_line_rate: false,
        };
        let _ = self.raft_core_tx.send((event, tracing::debug_span!("CH")));
        self.replication_buffer.clear();
        self.outbound_buffer.clear();
        loop {
            if self.target_state != TargetReplState::Lagging {
                return;
            }
            // If this stream is far enough behind, then transition to snapshotting state.
            if self.needs_snapshot() {
                self.target_state = TargetReplState::Snapshotting;
                return;
            }

            // Prep entries from storage and send them off for replication.
            if self.is_up_to_speed() {
                self.target_state = TargetReplState::LineRate;
                return;
            }
            self.prep_outbound_buffer_from_storage().await;
            self.send_append_entries().await;
            if self.is_up_to_speed() {
                self.target_state = TargetReplState::LineRate;
                return;
            }

            // Check raft channel to ensure we are staying up-to-date, then loop.
            if let Some(Some((event, span))) = self.repl_rx.recv().now_or_never() {
                self.drain_raft_rx(event, span);
            }
        }
    }

    /// Check if this replication stream is now up-to-speed.
    #[tracing::instrument(level="trace", skip(self), fields(self.core.next_index, self.core.commit_index))]
    fn is_up_to_speed(&self) -> bool {
        self.next_index > self.commit_index
    }

    /// Prep the outbound buffer with the next payload of entries to append.
    #[tracing::instrument(level = "trace", skip(self))]
    async fn prep_outbound_buffer_from_storage(&mut self) {
        // If the send buffer is empty, we need to fill it.
        if self.outbound_buffer.is_empty() {
            // Determine an appropriate stop index for the storage fetch operation. Avoid underflow.
            //
            // Logs in storage:
            // 0 ... next_index ... commit_index ... last_uncommitted_index

            // Underflow is guarded against in the `is_up_to_speed` check in the outer loop.
            let distance_behind = self.commit_index - self.next_index;

            let is_within_payload_distance = distance_behind <= self.config.max_payload_entries;

            let stop_idx = if is_within_payload_distance {
                // If we have caught up to the line index, then that means we will be running at
                // line rate after this payload is successfully replicated.
                self.target_state = TargetReplState::LineRate; // Will continue in lagging state until the outer loop cycles.
                self.commit_index + 1 // +1 to ensure stop value is included.
            } else {
                self.next_index + self.config.max_payload_entries + 1 // +1 to ensure stop value is
                                                                      // included.
            };

            // Bringing the target up-to-date by fetching the largest possible payload of entries
            // from storage within permitted configuration & ensure no snapshot pointer was returned.
            let entries = match self.storage.get_log_entries(self.next_index..stop_idx).await {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::error!(error=%err, "error fetching logs from storage");
                    let _ = self.raft_core_tx.send((ReplicaEvent::Shutdown, tracing::debug_span!("CH")));
                    self.target_state = TargetReplState::Shutdown;
                    return;
                }
            };

            for entry in entries.iter() {
                if let EntryPayload::PurgedMarker = entry.payload {
                    self.target_state = TargetReplState::Snapshotting;
                    return;
                }
            }

            self.outbound_buffer.extend(entries.into_iter().map(OutboundEntry::Raw));
        }
    }
}

//////////////////////////////////////////////////////////////////////////////////////////////////

/// Snapshotting specific state.
struct SnapshottingState<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> {
    /// An exclusive handle to the replication core.
    replication_core: &'a mut ReplicationCore<D, R, N, S>,
    snapshot: Option<Snapshot<S::SnapshotData>>,
    snapshot_fetch_rx: Option<oneshot::Receiver<Snapshot<S::SnapshotData>>>,
}

impl<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> SnapshottingState<'a, D, R, N, S> {
    /// Create a new instance.
    pub fn new(replication_core: &'a mut ReplicationCore<D, R, N, S>) -> Self {
        Self {
            replication_core,
            snapshot: None,
            snapshot_fetch_rx: None,
        }
    }

    #[tracing::instrument(level = "trace", skip(self), fields(state = "snapshotting"))]
    pub async fn run(mut self) {
        let event = ReplicaEvent::RateUpdate {
            target: self.replication_core.target,
            is_line_rate: false,
        };
        let _ = self.replication_core.raft_core_tx.send((event, tracing::debug_span!("CH")));
        self.replication_core.replication_buffer.clear();
        self.replication_core.outbound_buffer.clear();

        loop {
            if self.replication_core.target_state != TargetReplState::Snapshotting {
                return;
            }

            // If we don't have any of the components we need, fetch the current snapshot.
            if self.snapshot.is_none() && self.snapshot_fetch_rx.is_none() {
                let (tx, rx) = oneshot::channel();
                let _ = self.replication_core.raft_core_tx.send((
                    ReplicaEvent::NeedsSnapshot {
                        target: self.replication_core.target,
                        tx,
                    },
                    tracing::debug_span!("CH"),
                ));
                self.snapshot_fetch_rx = Some(rx);
            }

            // If we are waiting for a snapshot response from the storage layer, then wait for
            // it and send heartbeats in the meantime.
            if let Some(snapshot_fetch_rx) = self.snapshot_fetch_rx.take() {
                self.wait_for_snapshot(snapshot_fetch_rx).await;
                continue;
            }

            // If we have a snapshot to work with, then stream it.
            if let Some(snapshot) = self.snapshot.take() {
                if let Err(err) = self.stream_snapshot(snapshot).await {
                    tracing::warn!(error=%err, "error streaming snapshot to target");
                }
                continue;
            }
        }
    }

    /// Wait for a response from the storage layer for the current snapshot.
    ///
    /// If an error comes up during processing, this routine should simple be called again after
    /// issuing a new request to the storage layer.
    #[tracing::instrument(level = "trace", skip(self, rx))]
    async fn wait_for_snapshot(&mut self, mut rx: oneshot::Receiver<Snapshot<S::SnapshotData>>) {
        loop {
            let span = tracing::debug_span!("FFF:wait_for_snapshot");
            let _ent = span.enter();

            tokio::select! {
                _ = self.replication_core.heartbeat.tick() => self.replication_core.send_append_entries().await,

                event_span = self.replication_core.repl_rx.recv() =>  {
                    match event_span {

                        Some((event, span)) => self.replication_core.drain_raft_rx(event, span),
                        None => {
                            self.replication_core.target_state = TargetReplState::Shutdown;
                            return;
                        }
                    }
                },

                res = &mut rx => {
                    match res {
                        Ok(snapshot) => {
                            self.snapshot = Some(snapshot);
                            return;
                        }
                        Err(_) => return, // Channels may close for various acceptable reasons.
                    }
                },
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self, snapshot))]
    async fn stream_snapshot(&mut self, mut snapshot: Snapshot<S::SnapshotData>) -> RaftResult<()> {
        let end = snapshot.snapshot.seek(SeekFrom::End(0)).await?;

        let mut offset = 0;

        self.replication_core.next_index = snapshot.meta.last_log_id.index + 1;
        self.replication_core.matched = snapshot.meta.last_log_id;
        let mut buf = Vec::with_capacity(self.replication_core.config.snapshot_max_chunk_size as usize);

        loop {
            // Build the RPC.
            snapshot.snapshot.seek(SeekFrom::Start(offset)).await?;
            let n_read = snapshot.snapshot.read_buf(&mut buf).await?;

            let done = (offset + n_read as u64) == end; // If bytes read == 0, then we're done.
            let req = InstallSnapshotRequest {
                term: self.replication_core.term,
                leader_id: self.replication_core.id,
                meta: snapshot.meta.clone(),
                offset,
                data: Vec::from(&buf[..n_read]),
                done,
            };
            buf.clear();

            // Send the RPC over to the target.
            tracing::debug!(
                snapshot_size = req.data.len(),
                req.offset,
                end,
                req.done,
                "sending snapshot chunk"
            );

            let res = timeout(
                self.replication_core.install_snapshot_timeout,
                self.replication_core.network.send_install_snapshot(self.replication_core.target, req),
            )
            .await;

            let res = match res {
                Ok(outer_res) => match outer_res {
                    Ok(res) => res,
                    Err(err) => {
                        tracing::warn!(error=%err, "error sending InstallSnapshot RPC to target");
                        continue;
                    }
                },
                Err(err) => {
                    tracing::warn!(error=%err, "timeout while sending InstallSnapshot RPC to target");
                    continue;
                }
            };

            // Handle response conditions.
            if res.term > self.replication_core.term {
                let _ = self.replication_core.raft_core_tx.send((
                    ReplicaEvent::RevertToFollower {
                        target: self.replication_core.target,
                        term: res.term,
                    },
                    tracing::debug_span!("CH"),
                ));
                self.replication_core.target_state = TargetReplState::Shutdown;
                return Ok(());
            }

            // If we just sent the final chunk of the snapshot, then transition to lagging state.
            if done {
                self.replication_core.target_state = TargetReplState::Lagging;
                return Ok(());
            }

            // Everything is good, so update offset for sending the next chunk.
            offset += n_read as u64;

            // Check raft channel to ensure we are staying up-to-date, then loop.
            if let Some(Some((event, span))) = self.replication_core.repl_rx.recv().now_or_never() {
                self.replication_core.drain_raft_rx(event, span);
            }
        }
    }
}
