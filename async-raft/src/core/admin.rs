use std::collections::BTreeSet;
use std::collections::HashSet;

use crate::core::client::ClientRequestEntry;
use crate::core::ConsensusState;
use crate::core::LeaderState;
use crate::core::NonVoterReplicationState;
use crate::core::NonVoterState;
use crate::core::State;
use crate::core::UpdateCurrentLeader;
use crate::error::ChangeConfigError;
use crate::error::InitializeError;
use crate::raft::ClientWriteRequest;
use crate::raft::MembershipConfig;
use crate::raft::ResponseTx;
use crate::replication::RaftEvent;
use crate::AppData;
use crate::AppDataResponse;
use crate::NodeId;
use crate::RaftError;
use crate::RaftNetwork;
use crate::RaftStorage;

impl<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> NonVoterState<'a, D, R, N, S> {
    /// Handle the admin `init_with_config` command.
    #[tracing::instrument(level = "debug", skip(self))]
    pub(super) async fn handle_init_with_config(
        &mut self,
        mut members: BTreeSet<NodeId>,
    ) -> Result<(), InitializeError> {
        if self.core.last_log_id.index != 0 || self.core.current_term != 0 {
            tracing::error!({self.core.last_log_id.index, self.core.current_term}, "rejecting init_with_config request as last_log_index or current_term is 0");
            return Err(InitializeError::NotAllowed);
        }

        // Ensure given config contains this nodes ID as well.
        if !members.contains(&self.core.id) {
            members.insert(self.core.id);
        }

        // Build a new membership config from given init data & assign it as the new cluster
        // membership config in memory only.
        self.core.membership = MembershipConfig {
            members,
            members_after_consensus: None,
        };

        // Become a candidate and start campaigning for leadership. If this node is the only node
        // in the cluster, then become leader without holding an election. If members len == 1, we
        // know it is our ID due to the above code where we ensure our own ID is present.
        if self.core.membership.members.len() == 1 {
            self.core.current_term += 1;
            self.core.voted_for = Some(self.core.id);
            self.core.set_target_state(State::Leader);
            self.core.save_hard_state().await?;
        } else {
            self.core.set_target_state(State::Candidate);
        }

        Ok(())
    }
}

impl<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> LeaderState<'a, D, R, N, S> {
    /// Add a new node to the cluster as a non-voter, bringing it up-to-speed, and then responding
    /// on the given channel.
    #[tracing::instrument(level = "trace", skip(self, tx))]
    pub(super) fn add_member(&mut self, target: NodeId, tx: ResponseTx) {
        // Ensure the node doesn't already exist in the current config, in the set of new nodes
        // alreading being synced, or in the nodes being removed.
        if self.core.membership.members.contains(&target)
            || self
                .core
                .membership
                .members_after_consensus
                .as_ref()
                .map(|new| new.contains(&target))
                .unwrap_or(false)
            || self.non_voters.contains_key(&target)
        {
            tracing::debug!("target node is already a cluster member or is being synced");
            let _ = tx.send(Err(ChangeConfigError::Noop.into()));
            return;
        }

        // Spawn a replication stream for the new member. Track state as a non-voter so that it
        // can be updated to be added to the cluster config once it has been brought up-to-date.
        let state = self.spawn_replication_stream(target);
        self.non_voters.insert(target, NonVoterReplicationState {
            state,
            is_ready_to_join: false,
            tx: Some(tx),
        });
    }

    #[tracing::instrument(level = "trace", skip(self, tx))]
    pub(super) async fn change_membership(&mut self, members: BTreeSet<NodeId>, tx: ResponseTx) {
        // Ensure cluster will have at least one node.
        if members.is_empty() {
            let _ = tx.send(Err(ChangeConfigError::InoperableConfig.into()));
            return;
        }

        // Only allow config updates when currently in a uniform consensus state.
        match &self.consensus_state {
            ConsensusState::Uniform => (),
            ConsensusState::NonVoterSync { .. } | ConsensusState::Joint { .. } => {
                let _ = tx.send(Err(ChangeConfigError::ConfigChangeInProgress.into()));
                return;
            }
        }

        // Check the proposed config for any new nodes. If ALL new nodes already have replication
        // streams AND are ready to join, then we can immediately proceed with entering joint
        // consensus. Else, new nodes need to first be brought up-to-speed.
        //
        // Here, all we do is check to see which nodes still need to be synced, which determines
        // if we can proceed.
        let mut awaiting = HashSet::new();
        for new_node in members.difference(&self.core.membership.members) {
            match self.non_voters.get(&new_node) {
                // Node is ready to join.
                Some(node) if node.is_ready_to_join => continue,
                // Node has repl stream, but is not yet ready to join.
                Some(_) => (),
                // Node does not yet have a repl stream, spawn one.
                None => {
                    // Spawn a replication stream for the new member. Track state as a non-voter so that it
                    // can be updated to be added to the cluster config once it has been brought up-to-date.
                    let state = self.spawn_replication_stream(*new_node);
                    self.non_voters.insert(*new_node, NonVoterReplicationState {
                        state,
                        is_ready_to_join: false,
                        tx: None,
                    });
                }
            }
            awaiting.insert(*new_node);
        }
        // If there are new nodes which need to sync, then we need to wait until they are synced.
        // Once they've finished, this routine will be called again to progress further.
        if !awaiting.is_empty() {
            self.consensus_state = ConsensusState::NonVoterSync { awaiting, members, tx };
            return;
        }

        // Enter into joint consensus if we are not awaiting any new nodes.
        if !members.contains(&self.core.id) {
            self.is_stepping_down = true;
        }
        self.consensus_state = ConsensusState::Joint { is_committed: false };
        self.core.membership.members_after_consensus = Some(members.clone());

        // Create final_config first, the joint config may be committed at once if the cluster has only 1 node
        // and changes core.membership.
        let final_config = MembershipConfig {
            members: members.clone(),
            members_after_consensus: None,
        };

        let joint_config = self.core.membership.clone();

        let res = self.append_membership_log(joint_config, None).await;
        if let Err(e) = res {
            tracing::error!("append joint log error: {:?}", e);
        }

        let res = self.append_membership_log(final_config, Some(tx)).await;
        if let Err(e) = res {
            tracing::error!("append final log error: {:?}", e);
        }
    }

    #[tracing::instrument(level = "trace", skip(self, resp_tx), fields(id=self.core.id))]
    pub async fn append_membership_log(
        &mut self,
        mem: MembershipConfig,
        resp_tx: Option<ResponseTx>,
    ) -> Result<(), RaftError> {
        let payload = ClientWriteRequest::<D>::new_config(mem);
        let res = self.append_payload_to_log(payload.entry).await;
        let entry = match res {
            Ok(entry) => entry,
            Err(err) => {
                let err_str = err.to_string();
                if let Some(tx) = resp_tx {
                    let send_res = tx.send(Err(err.into()));
                    if let Err(e) = send_res {
                        tracing::error!("send response res error: {:?}", e);
                    }
                }
                return Err(RaftError::RaftStorage(anyhow::anyhow!(err_str)));
            }
        };

        let cr_entry = ClientRequestEntry::from_entry(entry, resp_tx);
        self.replicate_client_request(cr_entry).await;

        Ok(())
    }

    /// Handle the commitment of a joint consensus cluster configuration.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(super) fn handle_joint_consensus_committed(&mut self) {
        if let ConsensusState::Joint { is_committed, .. } = &mut self.consensus_state {
            *is_committed = true; // Mark as committed.
        }
        // Only proceed to finalize this joint consensus if there are no remaining nodes being synced.
        if self.consensus_state.is_joint_consensus_safe_to_finalize() {
            self.update_replication_state();
            self.finalize_joint_consensus();
        }
    }

    /// When the joint membership is committed(not the uniform membership),
    /// a new added node turns from a NonVoter to a Follower.
    /// Thus we need to move replication state from `non_voters` to `nodes`.
    ///
    /// There are two place in this code base where `nodes` are changed:
    /// - When a leader is established it adds all node_id found in `membership` to `nodes`.
    /// - When membership change is committed, i.e., a joint membership or a uniform membership.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(super) fn update_replication_state(&mut self) {
        tracing::debug!("update_replication_state");

        let new_node_ids = self
            .core
            .membership
            .all_nodes()
            .into_iter()
            .filter(|elem| elem != &self.core.id)
            .collect::<BTreeSet<_>>();

        let old_node_ids = self.core.membership.members.clone();
        let node_ids_to_add = new_node_ids.difference(&old_node_ids);

        // move replication state from non_voters to nodes.
        for node_id in node_ids_to_add {
            if !self.non_voters.contains_key(node_id) {
                // Just a probe for bug
                panic!(
                    "joint membership contains node_id:{} not in non_voters:{:?}",
                    node_id,
                    self.non_voters.keys().collect::<Vec<_>>()
                );
            }

            if self.nodes.contains_key(node_id) {
                // Just a probe for bug
                panic!(
                    "joint membership contains an existent node_id:{} in nodes:{:?}",
                    node_id,
                    self.nodes.keys().collect::<Vec<_>>()
                );
            }

            let non_voter_state = self.non_voters.remove(node_id).unwrap();
            self.nodes.insert(*node_id, non_voter_state.state);
        }
    }

    /// Finalize the committed joint consensus.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(super) fn finalize_joint_consensus(&mut self) {
        // Only proceed if it is safe to do so.
        if !self.consensus_state.is_joint_consensus_safe_to_finalize() {
            tracing::error!("attempted to finalize joint consensus when it was not safe to do so");
            return;
        }

        // Cut the cluster config over to the new membership config.
        if let Some(new_members) = self.core.membership.members_after_consensus.take() {
            self.core.membership.members = new_members;
        }
        self.consensus_state = ConsensusState::Uniform;

        // NOTE WELL: this implementation uses replication streams (src/replication/**) to replicate
        // entries. Nodes which do not exist in the new config will still have an active replication
        // stream until the current leader determines that they have replicated the config entry which
        // removes them from the cluster. At that point in time, the node will revert to non-voter state.
        //
        // HOWEVER, if an election takes place, the new leader will not have the old nodes in its config
        // and the old nodes may not revert to non-voter state using the above mechanism. That is fine.
        // The Raft spec accounts for this using the 3rd safety measure of cluster configuration changes
        // described at the very end of §6. This measure is already implemented and in place.
    }

    /// Handle the commitment of a uniform consensus cluster configuration.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(super) fn handle_uniform_consensus_committed(&mut self, index: u64) {
        // Step down if needed.
        if self.is_stepping_down {
            tracing::debug!("raft node is stepping down");
            self.core.set_target_state(State::NonVoter);
            self.core.update_current_leader(UpdateCurrentLeader::Unknown);
            return;
        }

        // Remove any replication streams which have replicated this config & which are no longer
        // cluster members. All other replication streams which are no longer cluster members, but
        // which have not yet replicated this config will be marked for removal.
        let membership = &self.core.membership;
        let nodes_to_remove: Vec<_> = self
            .nodes
            .iter_mut()
            .filter(|(id, _)| !membership.contains(id))
            .filter_map(|(idx, replstate)| {
                if replstate.matched.index >= index {
                    Some(*idx)
                } else {
                    replstate.remove_after_commit = Some(index);
                    None
                }
            })
            .collect();

        let follower_ids: Vec<u64> = self.nodes.keys().cloned().collect();
        let non_voter_ids: Vec<u64> = self.non_voters.keys().cloned().collect();
        tracing::debug!("nodes: {:?}", follower_ids);
        tracing::debug!("non_voters: {:?}", non_voter_ids);
        tracing::debug!("membership: {:?}", self.core.membership);
        tracing::debug!("nodes_to_remove: {:?}", nodes_to_remove);

        for target in nodes_to_remove {
            tracing::debug!(target, "removing target node from replication pool");
            if let Some(node) = self.nodes.remove(&target) {
                let _ = node.replstream.repl_tx.send((RaftEvent::Terminate, tracing::debug_span!("CH")));

                // remove metrics entry
                self.leader_metrics.replication.remove(&target);
            }
        }
        self.leader_report_metrics();
    }
}
