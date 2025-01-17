//! The Raft storage interface and data types.

use std::error::Error;
use std::fmt::Debug;
use std::ops::RangeBounds;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncRead;
use tokio::io::AsyncSeek;
use tokio::io::AsyncWrite;

use crate::raft::Entry;
use crate::raft::MembershipConfig;
use crate::raft_types::SnapshotId;
use crate::AppData;
use crate::AppDataResponse;
use crate::LogId;
use crate::NodeId;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SnapshotMeta {
    // Log entries upto which this snapshot includes, inclusive.
    pub last_log_id: LogId,

    /// The latest membership configuration covered by the snapshot.
    pub membership: MembershipConfig,

    /// To identify a snapshot when transferring.
    /// Caveat: even when two snapshot is built with the same `last_log_id`, they still could be different in bytes.
    pub snapshot_id: SnapshotId,
}

/// The data associated with the current snapshot.
pub struct Snapshot<S>
where S: AsyncRead + AsyncSeek + Send + Unpin + 'static
{
    /// metadata of a snapshot
    pub meta: SnapshotMeta,

    /// A read handle to the associated snapshot.
    pub snapshot: Box<S>,
}

/// A record holding the hard state of a Raft node.
///
/// This model derives serde's traits for easily (de)serializing this
/// model for storage & retrieval.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Default)]
pub struct HardState {
    /// The last recorded term observed by this system.
    pub current_term: u64,
    /// The ID of the node voted for in the `current_term`.
    pub voted_for: Option<NodeId>,
}

/// A struct used to represent the initial state which a Raft node needs when first starting.
#[derive(Clone, Debug)]
pub struct InitialState {
    /// The last entry.
    pub last_log_id: LogId,

    /// The LogId of the last log applied to the state machine.
    pub last_applied_log: LogId,
    /// The saved hard state of the node.
    pub hard_state: HardState,
    /// The latest cluster membership configuration found in the log, else a new initial
    /// membership config consisting only of this node's ID.
    pub membership: MembershipConfig,
}

impl InitialState {
    /// Create a new instance for a pristine Raft node.
    ///
    /// ### `id`
    /// The ID of the Raft node.
    pub fn new_initial(id: NodeId) -> Self {
        Self {
            last_log_id: LogId { term: 0, index: 0 },
            last_applied_log: LogId { term: 0, index: 0 },
            hard_state: HardState {
                current_term: 0,
                voted_for: None,
            },
            membership: MembershipConfig::new_initial(id),
        }
    }
}

/// A trait defining the interface for a Raft storage system.
///
/// See the [storage chapter of the guide](https://async-raft.github.io/async-raft/storage.html)
/// for details and discussion on this trait and how to implement it.
#[async_trait]
pub trait RaftStorage<D, R>: Send + Sync + 'static
where
    D: AppData,
    R: AppDataResponse,
{
    /// The storage engine's associated type used for exposing a snapshot for reading & writing.
    ///
    /// See the [storage chapter of the guide](https://async-raft.github.io/async-raft/storage.html)
    /// for details on where and how this is used.
    type SnapshotData: AsyncRead + AsyncWrite + AsyncSeek + Send + Unpin + 'static;

    /// The error type used to indicate to Raft that shutdown is needed when calling the
    /// `apply_entry_to_state_machine` method.
    ///
    /// This error type is only considered for the `apply_entry_to_state_machine` method as it is
    /// the only method which is allowed to return errors normally as part of application logic.
    ///
    /// For all other methods of this trait, returning an error will cause Raft to shutdown.
    type ShutdownError: Error + Send + Sync + 'static;

    /// Set if to turn on defensive check to unexpected input.
    /// E.g. discontinuous log appending.
    /// The default impl returns `false` to indicate it does impl any defensive check.
    async fn defensive(&self, _d: bool) -> bool {
        false
    }

    /// Get the latest membership config found in the log.
    ///
    /// This must always be implemented as a reverse search through the log to find the most
    /// recent membership config to be appended to the log.
    ///
    /// If a snapshot pointer is encountered, then the membership config embedded in that snapshot
    /// pointer should be used.
    ///
    /// If the system is pristine, then it should return the value of calling
    /// `MembershipConfig::new_initial(node_id)`. It is required that the storage engine persist
    /// the node's ID so that it is consistent across restarts.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn get_membership_config(&self) -> Result<MembershipConfig>;

    /// Get Raft's state information from storage.
    ///
    /// When the Raft node is first started, it will call this interface on the storage system to
    /// fetch the last known state from stable storage. If no such entry exists due to being the
    /// first time the node has come online, then `InitialState::new_initial` should be used.
    ///
    /// **Pro tip:** the storage impl may need to look in a few different places to accurately
    /// respond to this request: the last entry in the log for `last_log_index` & `last_log_term`;
    /// the node's hard state record; and the index of the last log applied to the state machine.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn get_initial_state(&self) -> Result<InitialState>;

    /// Save Raft's hard-state.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn save_hard_state(&self, hs: &HardState) -> Result<()>;

    /// Get a series of log entries from storage.
    ///
    /// The start value is inclusive in the search and the stop value is non-inclusive: `[start, stop)`.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn get_log_entries<RNG: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RNG,
    ) -> Result<Vec<Entry<D>>>;

    /// Try to get an log entry.
    /// It does not return an error if in defensive mode and the log entry at `log_index` is not found.
    async fn try_get_log_entry(&self, log_index: u64) -> Result<Option<Entry<D>>>;

    /// Returns the last known log id.
    /// It could be the id of the last entry in log, or the last applied id that is saved in state machine.
    ///
    /// When there is no log or state machine, it returns (0,0)
    ///
    /// Caveat: an impl must hold the log-state-machine consistency or must deal with the inconsistency when accessing
    /// it:
    ///
    /// I.e.: if `logs.last().log_id.index > last_applied.index`, `logs.last().log_id > last_applied` must hold. E.g.,
    /// `logs.last() == {term:1, index:2}` and `last_applied == {term:2, index:1}` is inconsistent:
    ///
    /// Log `{term:1, index:2}` can not be committed and should definitely be removed. The simplest way to achieve
    /// consistency is to remove such inconsistent logs after a successful `append_entries` or `install_snapshot`
    /// request.
    ///
    /// TODO(xp) test it
    /// TODO(xp) defensive test about consistency
    async fn get_last_log_id(&self) -> Result<LogId>;

    /// Delete all logs in a `range`.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn delete_logs_from<RNG: RangeBounds<u64> + Clone + Debug + Send + Sync>(&self, range: RNG) -> Result<()>;

    /// Append a payload of entries to the log.
    ///
    /// Though the entries will always be presented in order, each entry's index should be used to
    /// determine its location to be written in the log.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn append_to_log(&self, entries: &[&Entry<D>]) -> Result<()>;

    /// Apply the given payload of entries to the state machine.
    ///
    /// The Raft protocol guarantees that only logs which have been _committed_, that is, logs which
    /// have been replicated to a majority of the cluster, will be applied to the state machine.
    ///
    /// This is where the business logic of interacting with your application's state machine
    /// should live. This is 100% application specific. Perhaps this is where an application
    /// specific transaction is being started, or perhaps committed. This may be where a key/value
    /// is being stored.
    ///
    /// An impl should do:
    /// - Deal with the EntryPayload::Normal() log, which is business logic log.
    /// - Deal with EntryPayload::ConfigChange
    /// - A EntryPayload::SnapshotPointer log should never be seen.
    ///
    /// TODO(xp): choose one of the following policy:
    /// Error handling for this method is note worthy. If an error is returned from a call to this
    /// method, the error will be inspected, and if the error is an instance of
    /// `RaftStorage::ShutdownError`, then Raft will go into shutdown in order to preserve the
    /// safety of the data and avoid corruption. Any other errors will be propagated back up to the
    /// `Raft.client_write` call point.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn apply_to_state_machine(&self, entries: &[&Entry<D>]) -> Result<Vec<R>>;

    /// Perform log compaction, returning a handle to the generated snapshot.
    ///
    /// ### implementation guide
    /// When performing log compaction, the compaction can only cover the breadth of the log up to
    /// the last applied log and under write load this value may change quickly. As such, the
    /// storage implementation should export/checkpoint/snapshot its state machine, and then use
    /// the value of that export's last applied log as the metadata indicating the breadth of the
    /// log covered by the snapshot.
    ///
    /// Errors returned from this method will be logged and retried.
    async fn do_log_compaction(&self) -> Result<Snapshot<Self::SnapshotData>>;

    /// Create a new blank snapshot, returning a writable handle to the snapshot object.
    ///
    /// Raft will use this handle to receive snapshot data.
    ///
    /// ### implementation guide
    /// See the [storage chapter of the guide](https://async-raft.github.io/async-raft/storage.html)
    /// for details on log compaction / snapshotting.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn begin_receiving_snapshot(&self) -> Result<Box<Self::SnapshotData>>;

    /// Finalize the installation of a snapshot which has finished streaming from the cluster leader.
    ///
    /// Delete all entries in the log through `meta.last_log_id.index`.
    ///
    /// Write a new snapshot pointer to the log at the given `meta.last_log_id.index`. The snapshot pointer should be
    /// constructed via the `Entry::new_snapshot_pointer` constructor and the other parameters
    /// provided to this method.
    ///
    /// All other snapshots should be deleted at this point.
    ///
    /// ### snapshot
    /// A snapshot created from an earlier call to `created_snapshot` which provided the snapshot.
    /// By the time ownership of the snapshot object is returned here, its
    /// `AsyncWriteExt.shutdown()` method will have been called, so no additional writes should be
    /// made to the snapshot.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn finalize_snapshot_installation(
        &self,
        meta: &SnapshotMeta,
        snapshot: Box<Self::SnapshotData>,
    ) -> Result<()>;

    /// Get a readable handle to the current snapshot, along with its metadata.
    ///
    /// ### implementation algorithm
    /// Implementing this method should be straightforward. Check the configured snapshot
    /// directory for any snapshot files. A proper implementation will only ever have one
    /// active snapshot, though another may exist while it is being created. As such, it is
    /// recommended to use a file naming pattern which will allow for easily distinguishing between
    /// the current live snapshot, and any new snapshot which is being created.
    ///
    /// A proper snapshot implementation will store the term, index and membership config as part
    /// of the snapshot, which should be decoded for creating this method's response data.
    ///
    /// Errors returned from this method will cause Raft to go into shutdown.
    async fn get_current_snapshot(&self) -> Result<Option<Snapshot<Self::SnapshotData>>>;
}

/// APIs for debugging a store.
#[async_trait]
pub trait RaftStorageDebug<SM> {
    /// Get a handle to the state machine for testing purposes.
    async fn get_state_machine(&self) -> SM;

    /// Get the current hard state for testing purposes.
    async fn read_hard_state(&self) -> Option<HardState>;
}
