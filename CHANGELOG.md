changelog
=========
This changelog follows the patterns described here: https://keepachangelog.com/en/1.0.0/.

## [unreleased]

### fixed

- Fixed [122](https://github.com/async-raft/async-raft/pull/122) a conflict is expected even when appending empty enties.

    `append_entries` should get a response with non-none ConflictOpt even if the entries in the message is empty.
    Otherwise if no conflict is found the leader will never be able to sync logs to a new added
    NonVoter, until a next log, is proposed on the leader.

- Fixed [117](https://github.com/async-raft/async-raft/pull/117) `last_applied` should be updated only when logs actually applied.

## async-raft 0.6.1
### fixed
- Fixed [#105](https://github.com/async-raft/async-raft/issues/105) where function `set_target_state` missing `else` condition.
- Fixed [#106](https://github.com/async-raft/async-raft/issues/106) which ensures that counting of replicas to determine a new commit value only considers entries replicated as part of the current term.
- Fixed a bug where NonVoter nodes could be restarted and come back as voting members.

## async-raft 0.6.0
The big news for this release is that we are now based on Tokio 1.0! Big shoutout to @xu-cheng for doing all of the heavy lifting for the Tokio 1.0 update, along with many other changes which are part of this release.

It is important to note that 0.6.0 does include two breaking changes from 0.5: the new `RaftStorage::ShutdownError` associated type, and Tokio 1.0. Both of these changes are purely code related, and it is not expected that they will negatively impact running systems.

### changed
- Updated to Tokio 1.0!
- **BREAKING:** this introduces a `RaftStorage::ShutdownError` associated type. This allows for the Raft system to differentiate between fatal storage errors which should cause the system to shutdown vs errors which should be propagated back to the client for application specific error handling. These changes only apply to the `RaftStorage::apply_entry_to_state_machine` method.
- A small change to Raft startup semantics. When a node comes online and successfully recovers state (the node was already part of a cluster), the node will start with a 30 second election timeout, ensuring that it does not disrupt a running cluster.
- [#89](https://github.com/async-raft/async-raft/pull/89) removes the `Debug` bounds requirement on the `AppData` & `AppDataResponse` types.
- The `Raft` type can now be cloned. The clone is very cheap and helps to facilitate async workflows while feeding client requests and Raft RPCs into the Raft instance.
- The `Raft.shutdown` interface has been changed slightly. Instead of returning a `JoinHandle`, the method is now async and simply returns a result.
- The `ClientWriteError::ForwardToLeader` error variant has been modified slightly. It now exposes the data (generic type `D` of the type) of the original client request directly. This ensures that the data can actually be used for forwarding, if that is what the parent app wants to do.
- Implemented [#12](https://github.com/async-raft/async-raft/issues/12). This is a pretty old issue and a pretty solid optimization. The previous implementation of this algorithm would go to storage (typically disk) for every process of replicating entries to the state machine. Now, we are caching entries as they come in from the leader, and using only the cache as the source of data. There are a few simple measures needed to ensure this is correct, as the leader entry replication protocol takes care of most of the work for us in this case.
- Updated / clarified the interface for log compaction. See the guide or the updated `do_log_compaction` method docs for more details.

### added
- [#97](https://github.com/async-raft/async-raft/issues/97) adds the new `Raft.current_leader` method. This is a convenience method which builds upon the Raft metrics system to quickly and easily identify the current cluster leader.

### fixed
- Fixed [#98](https://github.com/async-raft/async-raft/issues/98) where heartbeats were being passed along into the log consistency check algorithm. This had the potential to cause a Raft node to go into shutdown under some circumstances.
- Fixed a bug where the timestamp of the last received heartbeat from a leader was not being stored, resulting in degraded cluster stability under some circumstances.

## memstore 0.2.0
### changed
- Updated async-raft dependency to `0.6.0` & updated storage interface as needed.

### fixed
- Fixed [#76](https://github.com/async-raft/async-raft/issues/76) by moving the process of replicating log entries to the state machine off of the main task. This ensures that the process never blocks the main task. This also includes a few nice optimizations mentioned below.

## 0.5.5
### changed
- Added `#[derive(Serialize, Deserialize)]` to `RaftMetrics`, `State`.

## 0.5.4
### fixed
- Fixed [#82](https://github.com/async-raft/async-raft/issues/82) where client reads were not behaving correctly for single node clusters. Single node integration tests have been updated to ensure this functionality is working as needed.

## 0.5.3
### fixed
- Fixed [#79](https://github.com/async-raft/async-raft/issues/79) ... for real this time! Add an integration test to prove it.

## 0.5.2
### fixed
- Fixed [#79](https://github.com/async-raft/async-raft/issues/79). The Raft core state machine was not being properly updated in response to shutdown requests. That has been addressed and shutdowns are now behaving as expected.

## 0.5.1
### changed
- `ChangeConfigError::NodeNotLeader` now returns the ID of the current cluster leader if known.
- Fix off-by-one error in `get_log_entries` during the replication process.
- Added `#[derive(Serialize, Deserialize)]` to `Config`, `ConfigBuilder` & `SnapshotPolicy`.

## 0.5.0
### changed
The only thing which hasn't changed is that this crate is still an implementation of the Raft protocol. Pretty much everything else has changed.

- Everything is built directly on Tokio now.
- The guide has been updated.
- Docs have been updated.
- The `Raft` type is now the primary API of this crate, and is a simple struct with a few public methods.
- Lots of fixes to the implementation of the protocol, ranging from subtle issues in joint consensus to non-voter syncing.

## 0.4.4
- Implemented `Error` for `config::ConfigError`

## 0.4.3
Added a few convenience derivations.

- Derive `Eq` on `messages::MembershipConfig`.
- Derive `Eq` on `metrics::State`.
- Derive `PartialEq` & `Eq` on `metrics::RaftMetrics`.
- Update development dependencies.
- Fixed bug [#41](https://github.com/railgun-rs/actix-raft/issues/41) where nodes were not starting a new election timeout task after comign down from leader state. Thanks @lionesswardrobe for the report!

## 0.4.2
A few QOL improvements.

- Fixed an issue where the value for `current_leader` was not being set to `None` when becoming a candidate. This isn't really a *bug* per se, as no functionality depended on this value as far as Raft is concerned, but it is an issue that impacts the metrics system. This value is now being updated properly.
- Made the `messages::ClientPayload::new_base` constructor `pub(crate)` instead of `pub`, which is what the intention was originally, but I was apparently tired `:)`.
- Implemented [#25](https://github.com/railgun-rs/actix-raft/issues/25). Implementing Display+Error for the admin error types.

## 0.4.1
A few bug fixes.

- Fixed an issue where a node in a single-node Raft was not resuming as leader after a crash.
- Fixed an issue where hard state was not being saved after a node becomes leader in a single-node Raft.
- Fixed an issue where the client request pipeline (a `Stream` with the `actix::StreamFinish`) was being closed after an error was returned during processing of client requests (which should not cause the stream to close). This was unexpected and undocumented behavior, very simple fix though.

## 0.4.0
This changeset introduces a new `AppDataResponse` type which represents a concrete data type which must be sent back from the `RaftStorage` impl from the `ApplyEntryToStateMachine` handler. This provides a more direct path for returning application level data from the storage impl. Often times this is needed for responding to client requests in a timely / efficient manner.

- `AppDataResponse` type has been added (see above).
- A few handlers have been updated in the `RaftStorage` type. The handlers are now separated based on where they are invoked from the Raft node. The three changed handlers are:
  - `AppendEntryToLog`: this is the same. It is the initial step of handling client requests to apply an entry to the log. This is still where application level errors may be safely returned to the client.
  - `ReplicateToLog`: this is for replicating entries to the log. This is part of the replication process.
  - `ApplyEntryToStateMachine`: this is for applying an entry to the state machine as the final part of a client request. This is where the new `AddDataResponse` type must be returned.
  - `ReplicateToStateMachine`: this is for replicating entries to the state machine. This is part of the replication process.

## 0.3.1
Overhauled the election timeout mechanism. This uses an interval job instead of juggling a rescheduling processes. Seems to offer quite a lot more stability. Along with the interval job, we are using std::time::Instants for performing the comparisons against the last received heartbeat.

## 0.3.0
Another backwards incompatible change to the `RaftStorage` trait. It is now using associated types to better express the needed trait constraints. These changes were the final bit of work needed to get the entire actix-raft system to work with a Synchronous `RaftStorage` impl. Async impls continue to work as they have, the `RaftStorage` impl block will need to be updated to use the associated types though. The recommend pattern is as follows:

```rust
impl RaftStorage<..., ...> for MyStorage {
    type Actor = Self;
    type Context = Context<Self>; // Or SyncContext<Self>;
}
```

My hope is that this will be the last backwards incompatible change needed before a 1.0 release. This crate is still young though, so we will see.

## 0.2.0
- Made a few backwards incompatible changes to the `RaftStorage` trait. Overwrite its third type parameter with `actix::SyncContext<Self>` to enable sync storage.
- Also removed the `RaftStorage::new` constructor, as it is a bit restrictive. Just added some docs instead describing what is needed.

## 0.1.3
- Added a few addition top-level exports for convenience.

## 0.1.2
- Changes to the README for docs.rs.

## 0.1.1
- Changes to the README for docs.rs.

## 0.1.0
- Initial release!
