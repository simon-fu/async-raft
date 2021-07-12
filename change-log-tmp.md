0.6.2-alpha.1..0.6.2-alpha.4

**Change**:

- change: merge term and index to `xxx_log_id`: LogId in several public types:

    - Entry
    - InitialState
    - AppendEntriesRequest
    - RaftCore
    - CurrentSnapshotData
    - SnapshotUpdate::SnapshotComplete
    - InstallSnapshotRequest

- change: use snapshot-id to identify a snapshot stream

    A snapshot stream should be identified by some id, since the server end
    should not assume messages are arrived in the correct order.
    Without an id, two `install_snapshot` request belonging to different
    snapshot data may corrupt the snapshot data, explicitly or even worse,
    silently.

    - Add SnapshotId to identify a snapshot stream.

    - Add SnapshotSegmentId to identify a segment in a snapshot stream.

    - Add field `snapshot_id` to snapshot related data structures.

    - Add error `RaftError::SnapshotMismatch`.

    - `Storage::create_snapshot()` does not need to return and id.
      Since the receiving end only keeps one snapshot stream session at
      most.
      Instead, `Storage::do_log_compaction()` should build a unique id
      everytime it is called.

    - When the raft node receives an `install_snapshot` request, the id must
      match to continue.
      A request with a different id should be rejected.
      A new id with offset=0 indicates the sender has started a new stream.
      In this case, the old unfinished stream is dropped and cleaned.

    - Add test for `install_snapshot` API.

**Fix**:

- fix: leader should re-create and send snapshot when `threshold/2 < last_log_index - snapshot < threshold`

    The problem:

    If `last_log_index` advances `snapshot.applied_index` too many, i.e.:
    `threshold/2 < last_log_index - snapshot < threshold`
    (e.g., `10/2 < 16-10 < 20` in the test that reproduce this bug), the leader
    tries to re-create a new snapshot. But when
    `last_log_index < threshold`, it won't create, which result in a dead
    loop.

    Solution:

    In such case, force to create a snapshot without considering the
    threshold.

- fix: `client_read` has used wrong quorum=majority-1

**Feature**:

- feature: add metrics about leader

    In LeaderState it also report metrics about the replication to other node when report metrics.

    When switched to other state, LeaderState will be destroyed as long as
    the cached replication metrics.

    Other state report an `None` to raft core to override the previous
    metrics data.

    At some point the raft core, without knonwning the state, just report
    metrics with an `Update::Ignore`, to indicate that leave replication
    metrics intact.

- feature: report snapshot metrics to RaftMetrics::snapshot, which is a LogId: (term, index) that a snapshot includes
    - Add: `Wait.snapshot()` to watch snapshot changes.
    - Test: replace `sleep()` with `wait_for_snapshot()` to speed up tests.


**Test**:

- test: add test of small chunk snapshot transfer

- test: compaction test does not need to change membership

- test: dynamic_membership: use wait() instead of sleep to reduce test time

**Refactor**:

- dep: upgrade tokio from 1.7 to 1.8

- refactor: merge term and index into xxx_log_id: LogId

