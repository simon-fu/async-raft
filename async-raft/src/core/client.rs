use std::sync::Arc;

use anyhow::anyhow;
use futures::future::TryFutureExt;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use tokio::time::timeout;
use tokio::time::Duration;
use tracing::Instrument;

use crate::core::LeaderState;
use crate::core::State;
use crate::error::ClientReadError;
use crate::error::ClientWriteError;
use crate::error::RaftError;
use crate::error::RaftResult;
use crate::error::ResponseError;
use crate::quorum;
use crate::raft::AppendEntriesRequest;
use crate::raft::ClientReadResponseTx;
use crate::raft::ClientWriteRequest;
use crate::raft::ClientWriteResponse;
use crate::raft::ClientWriteResponseTx;
use crate::raft::Entry;
use crate::raft::EntryPayload;
use crate::raft::ResponseTx;
use crate::replication::RaftEvent;
use crate::AppData;
use crate::AppDataResponse;
use crate::LogId;
use crate::RaftNetwork;
use crate::RaftStorage;

/// A wrapper around a ClientRequest which has been transformed into an Entry, along with its response channel.
pub(super) struct ClientRequestEntry<D: AppData, R: AppDataResponse> {
    /// The Arc'd entry of the ClientRequest.
    ///
    /// This value is Arc'd so that it may be sent across thread boundaries for replication
    /// without having to clone the data payload itself.
    pub entry: Arc<Entry<D>>,
    /// The response channel for the request.
    pub tx: ClientOrInternalResponseTx<D, R>,
}

impl<D: AppData, R: AppDataResponse> ClientRequestEntry<D, R> {
    /// Create a new instance from the raw components of a client request.
    pub(crate) fn from_entry<T: Into<ClientOrInternalResponseTx<D, R>>>(entry: Entry<D>, tx: T) -> Self {
        Self {
            entry: Arc::new(entry),
            tx: tx.into(),
        }
    }
}

/// An enum type wrapping either a client response channel or an internal Raft response channel.
#[derive(derive_more::From)]
pub enum ClientOrInternalResponseTx<D: AppData, R: AppDataResponse> {
    Client(ClientWriteResponseTx<D, R>),
    Internal(Option<ResponseTx>),
}

impl<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> LeaderState<'a, D, R, N, S> {
    /// Commit the initial entry which new leaders are obligated to create when first coming to power, per §8.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(super) async fn commit_initial_leader_entry(&mut self) -> RaftResult<()> {
        // If the cluster has just formed, and the current index is 0, then commit the current
        // config, else a blank payload.
        let last_index = self.core.last_log_id.index;

        let req: ClientWriteRequest<D> = if last_index == 0 {
            ClientWriteRequest::new_config(self.core.membership.clone())
        } else {
            // Complete a partial member-change:
            //
            // Raft appends two consecutive membership change logs: the joint config and the final config,
            // to impl a membership change.
            //
            // It is possible only the first one, the joint config log is written in to storage or replicated.
            // Thus if a new leader sees only the first one, it needs to append the final config log to let
            // the change-membership operation to finish.

            let last_logs =
                self.core.storage.get_log_entries(last_index..=last_index).await.map_err(RaftError::RaftStorage)?;
            let last_log = &last_logs[0];

            let req = match last_log.payload {
                EntryPayload::ConfigChange(ref mem) => {
                    if mem.membership.members_after_consensus.is_some() {
                        let final_config = mem.membership.to_final_config();
                        Some(ClientWriteRequest::new_config(final_config))
                    } else {
                        None
                    }
                }
                _ => None,
            };

            req.unwrap_or_else(ClientWriteRequest::new_blank_payload)
        };

        // Commit the initial payload to the cluster.
        let entry = self.append_payload_to_log(req.entry).await?;
        self.core.last_log_id.term = self.core.current_term; // This only ever needs to be updated once per term.

        let cr_entry = ClientRequestEntry::from_entry(entry, None);
        self.replicate_client_request(cr_entry).await;

        Ok(())
    }

    /// Handle client read requests.
    ///
    /// Spawn requests to all members of the cluster, include members being added in joint
    /// consensus. Each request will have a timeout, and we respond once we have a majority
    /// agreement from each config group. Most of the time, we will have a single uniform
    /// config group.
    ///
    /// From the spec (§8):
    /// Second, a leader must check whether it has been deposed before processing a read-only
    /// request (its information may be stale if a more recent leader has been elected). Raft
    /// handles this by having the leader exchange heartbeat messages with a majority of the
    /// cluster before responding to read-only requests.
    #[tracing::instrument(level = "trace", skip(self, tx))]
    pub(super) async fn handle_client_read_request(&mut self, tx: ClientReadResponseTx) {
        // Setup sentinel values to track when we've received majority confirmation of leadership.
        let mut c0_confirmed = 0usize;
        // Will never be zero, as we don't allow it when proposing config changes.
        let len_members = self.core.membership.members.len();

        let c0_needed = quorum::majority_of(len_members);

        let mut c1_confirmed = 0usize;
        let mut c1_needed = 0usize;
        if let Some(joint_members) = &self.core.membership.members_after_consensus {
            let len = joint_members.len(); // Will never be zero, as we don't allow it when proposing config changes.
            c1_needed = quorum::majority_of(len);
        }

        // Increment confirmations for self, including post-joint-consensus config if applicable.
        c0_confirmed += 1;
        let is_in_post_join_consensus_config = self
            .core
            .membership
            .members_after_consensus
            .as_ref()
            .map(|members| members.contains(&self.core.id))
            .unwrap_or(false);
        if is_in_post_join_consensus_config {
            c1_confirmed += 1;
        }

        // If we already have all needed confirmations — which would be the case for single node
        // clusters — then respond.
        if c0_confirmed >= c0_needed && c1_confirmed >= c1_needed {
            let _ = tx.send(Ok(()));
            return;
        }

        // Spawn parallel requests, all with the standard timeout for heartbeats.
        let mut pending = FuturesUnordered::new();
        for (id, node) in self.nodes.iter() {
            let rpc = AppendEntriesRequest {
                term: self.core.current_term,
                leader_id: self.core.id,
                prev_log_id: node.matched,
                entries: vec![],
                leader_commit: self.core.commit_index,
            };
            let target = *id;
            let network = self.core.network.clone();
            let ttl = Duration::from_millis(self.core.config.heartbeat_interval);
            let task = tokio::spawn(
                async move {
                    match timeout(ttl, network.send_append_entries(target, rpc)).await {
                        Ok(Ok(data)) => Ok((target, data)),
                        Ok(Err(err)) => Err((target, err)),
                        Err(_timeout) => Err((target, anyhow!("timeout waiting for leadership confirmation"))),
                    }
                }
                .instrument(tracing::debug_span!("spawn")),
            )
            .map_err(move |err| (*id, err));
            pending.push(task);
        }

        // Handle responses as they return.
        while let Some(res) = pending.next().await {
            // TODO(xp): if receives error about a higher term, it should stop at once?
            let (target, data) = match res {
                Ok(Ok(res)) => res,
                Ok(Err((target, err))) => {
                    tracing::error!(target, error=%err, "timeout while confirming leadership for read request");
                    continue;
                }
                Err((target, err)) => {
                    tracing::error!(target, "{}", err);
                    continue;
                }
            };

            // If we receive a response with a greater term, then revert to follower and abort this request.
            if data.term != self.core.current_term {
                self.core.update_current_term(data.term, None);
                self.core.set_target_state(State::Follower);
            }

            // If the term is the same, then it means we are still the leader.
            if self.core.membership.members.contains(&target) {
                c0_confirmed += 1;
            }
            if self
                .core
                .membership
                .members_after_consensus
                .as_ref()
                .map(|members| members.contains(&target))
                .unwrap_or(false)
            {
                c1_confirmed += 1;
            }
            if c0_confirmed >= c0_needed && c1_confirmed >= c1_needed {
                let _ = tx.send(Ok(()));
                return;
            }
        }

        // If we've hit this location, then we've failed to gather needed confirmations due to
        // request failures.
        let _ = tx.send(Err(ClientReadError::RaftError(RaftError::RaftNetwork(anyhow!(
            "too many requests failed, could not confirm leadership"
        )))));
    }

    /// Handle client write requests.
    #[tracing::instrument(level = "trace", skip(self, rpc, tx))]
    pub(super) async fn handle_client_write_request(
        &mut self,
        rpc: ClientWriteRequest<D>,
        tx: ClientWriteResponseTx<D, R>,
    ) {
        let entry = match self.append_payload_to_log(rpc.entry).await {
            Ok(entry) => ClientRequestEntry::from_entry(entry, tx),
            Err(err) => {
                let _ = tx.send(Err(ClientWriteError::RaftError(err)));
                return;
            }
        };
        self.replicate_client_request(entry).await;
    }

    /// Transform the given payload into an entry, assign an index and term, and append the entry to the log.
    #[tracing::instrument(level = "trace", skip(self, payload))]
    pub(super) async fn append_payload_to_log(&mut self, payload: EntryPayload<D>) -> RaftResult<Entry<D>> {
        let entry = Entry {
            log_id: LogId {
                index: self.core.last_log_id.index + 1,
                term: self.core.current_term,
            },
            payload,
        };
        self.core
            .storage
            .append_to_log(&[&entry])
            .await
            .map_err(|err| self.core.map_fatal_storage_error(err))?;
        self.core.last_log_id.index = entry.log_id.index;

        self.leader_report_metrics();

        Ok(entry)
    }

    /// Begin the process of replicating the given client request.
    ///
    /// NOTE WELL: this routine does not wait for the request to actually finish replication, it
    /// merely beings the process. Once the request is committed to the cluster, its response will
    /// be generated asynchronously.
    #[tracing::instrument(level = "trace", skip(self, req))]
    pub(super) async fn replicate_client_request(&mut self, req: ClientRequestEntry<D, R>) {
        // Replicate the request if there are other cluster members. The client response will be
        // returned elsewhere after the entry has been committed to the cluster.
        let entry_arc = req.entry.clone();

        if self.nodes.is_empty() && self.non_voters.is_empty() {
            // Else, there are no voting nodes for replication, so the payload is now committed.
            self.core.commit_index = entry_arc.log_id.index;
            self.leader_report_metrics();
            self.client_request_post_commit(req).await;
            return;
        }

        self.awaiting_committed.push(req);

        if !self.nodes.is_empty() {
            for node in self.nodes.values() {
                let _ = node.replstream.repl_tx.send((
                    RaftEvent::Replicate {
                        entry: entry_arc.clone(),
                        commit_index: self.core.commit_index,
                    },
                    tracing::debug_span!("CH"),
                ));
            }
        }

        if !self.non_voters.is_empty() {
            // Replicate to non-voters.
            for node in self.non_voters.values() {
                let _ = node.state.replstream.repl_tx.send((
                    RaftEvent::Replicate {
                        entry: entry_arc.clone(),
                        commit_index: self.core.commit_index,
                    },
                    tracing::debug_span!("CH"),
                ));
            }
        }
    }

    /// Handle the post-commit logic for a client request.
    #[tracing::instrument(level = "trace", skip(self, req))]
    pub(super) async fn client_request_post_commit(&mut self, req: ClientRequestEntry<D, R>) {
        let entry = &req.entry;

        match req.tx {
            ClientOrInternalResponseTx::Client(tx) => {
                match &entry.payload {
                    EntryPayload::Normal(_) => match self.apply_entry_to_state_machine(&entry).await {
                        Ok(data) => {
                            let _ = tx.send(Ok(ClientWriteResponse {
                                index: req.entry.log_id.index,
                                data,
                            }));
                        }
                        Err(err) => {
                            let _ = tx.send(Err(ClientWriteError::RaftError(err)));
                        }
                    },
                    _ => {
                        // Why is this a bug, and why are we shutting down? This is because we can not easily
                        // encode these constraints in the type system, and client requests should be the only
                        // log entry types for which a `ClientOrInternalResponseTx::Client` type is used. This
                        // error should never be hit unless we've done a poor job in code review.
                        tracing::error!("critical error in Raft, this is a programming bug, please open an issue");
                        self.core.set_target_state(State::Shutdown);
                    }
                }
            }
            ClientOrInternalResponseTx::Internal(tx) => {
                self.handle_special_log(entry);

                // TODO(xp): copied from above, need refactor.
                let res = self.apply_entry_to_state_machine(&entry).await;
                let res = match res {
                    Ok(_data) => Ok(entry.log_id.index),
                    Err(err) => {
                        tracing::error!("res of applying to state machine: {:?}", err);
                        Err(err)
                    }
                };

                // TODO(xp): if there is error, shall we go on?

                self.core.last_applied = entry.log_id;
                self.leader_report_metrics();

                match tx {
                    None => {
                        tracing::debug!("no response tx to send res");
                    }

                    Some(tx) => {
                        let send_res = tx.send(res.map_err(ResponseError::from));
                        tracing::debug!("send internal response through tx, res: {:?}", send_res);
                    }
                }
            }
        }

        // Trigger log compaction if needed.
        self.core.trigger_log_compaction_if_needed(false);
    }

    pub fn handle_special_log(&mut self, entry: &Arc<Entry<D>>) {
        match &entry.payload {
            EntryPayload::ConfigChange(ref mem) => {
                let m = &mem.membership;
                if m.is_in_joint_consensus() {
                    self.handle_joint_consensus_committed();
                } else {
                    self.handle_uniform_consensus_committed(entry.log_id.index);
                }
            }
            EntryPayload::Blank => {}
            EntryPayload::Normal(_) => {}
            EntryPayload::PurgedMarker => {}
        }
    }

    /// Apply the given log entry to the state machine.
    #[tracing::instrument(level = "trace", skip(self, entry))]
    pub(super) async fn apply_entry_to_state_machine(&mut self, entry: &Entry<D>) -> RaftResult<R> {
        // First, we just ensure that we apply any outstanding up to, but not including, the index
        // of the given entry. We need to be able to return the data response from applying this
        // entry to the state machine.
        //
        // Note that this would only ever happen if a node had unapplied logs from before becoming leader.

        let log_id = &entry.log_id;
        let index = log_id.index;

        let expected_next_index = self.core.last_applied.index + 1;
        if index != expected_next_index {
            let entries = self
                .core
                .storage
                .get_log_entries(expected_next_index..index)
                .await
                .map_err(|err| self.core.map_fatal_storage_error(err))?;

            if let Some(entry) = entries.last() {
                self.core.last_applied = entry.log_id;
            }

            let data_entries: Vec<_> = entries.iter().collect();
            if !data_entries.is_empty() {
                self.core
                    .storage
                    .apply_to_state_machine(&data_entries)
                    .await
                    .map_err(|err| self.core.map_fatal_storage_error(err))?;
            }
        }

        // Before we can safely apply this entry to the state machine, we need to ensure there is
        // no pending task to replicate entries to the state machine. This is edge case, and would only
        // happen once very early in a new leader's term.
        if !self.core.replicate_to_sm_handle.is_empty() {
            if let Some(Ok(replicate_to_sm_result)) = self.core.replicate_to_sm_handle.next().await {
                self.core.handle_replicate_to_sm_result(replicate_to_sm_result)?;
            }
        }
        // Apply this entry to the state machine and return its data response.
        let res = self.core.storage.apply_to_state_machine(&[entry]).await.map_err(|err| {
            if err.downcast_ref::<S::ShutdownError>().is_some() {
                // If this is an instance of the storage impl's shutdown error, then trigger shutdown.
                self.core.map_fatal_storage_error(err)
            } else {
                // Else, we propagate normally.
                RaftError::RaftStorage(err)
            }
        });

        self.core.last_applied = *log_id;
        self.leader_report_metrics();
        let res = res?;

        // TODO(xp) merge this function to replication_to_state_machine?

        Ok(res.into_iter().next().unwrap())
    }
}
