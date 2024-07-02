// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem::{replace, size_of};
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use prost::Message;
use risingwave_common::catalog::TableId;
use risingwave_common::util::epoch::INVALID_EPOCH;
use risingwave_pb::hummock::group_delta::DeltaType;
use risingwave_pb::hummock::hummock_version::Levels as PbLevels;
use risingwave_pb::hummock::hummock_version_delta::{ChangeLogDelta, GroupDeltas as PbGroupDeltas};
use risingwave_pb::hummock::{
    CompactionConfig, HummockVersion as PbHummockVersion,
    HummockVersionDelta as PbHummockVersionDelta, SstableInfo, StateTableInfo as PbStateTableInfo,
    StateTableInfo, StateTableInfoDelta,
};
use tracing::warn;

use crate::change_log::TableChangeLog;
use crate::compaction_group::hummock_version_ext::build_initial_compaction_group_levels;
use crate::compaction_group::StaticCompactionGroupId;
use crate::table_watermark::TableWatermarks;
use crate::{CompactionGroupId, HummockSstableObjectId, HummockVersionId, FIRST_VERSION_ID};

#[derive(Debug, Clone, PartialEq)]
pub struct HummockVersionStateTableInfo {
    state_table_info: HashMap<TableId, PbStateTableInfo>,

    // in memory index
    compaction_group_member_tables: HashMap<CompactionGroupId, BTreeSet<TableId>>,
}

impl HummockVersionStateTableInfo {
    pub fn empty() -> Self {
        Self {
            state_table_info: HashMap::new(),
            compaction_group_member_tables: HashMap::new(),
        }
    }

    fn build_compaction_group_member_tables(
        state_table_info: &HashMap<TableId, PbStateTableInfo>,
    ) -> HashMap<CompactionGroupId, BTreeSet<TableId>> {
        let mut ret: HashMap<_, BTreeSet<_>> = HashMap::new();
        for (table_id, info) in state_table_info {
            assert!(ret
                .entry(info.compaction_group_id)
                .or_default()
                .insert(*table_id));
        }
        ret
    }

    pub fn build_table_compaction_group_id(&self) -> HashMap<TableId, CompactionGroupId> {
        self.state_table_info
            .iter()
            .map(|(table_id, info)| (*table_id, info.compaction_group_id))
            .collect()
    }

    pub fn from_protobuf(state_table_info: &HashMap<u32, PbStateTableInfo>) -> Self {
        let state_table_info = state_table_info
            .iter()
            .map(|(table_id, info)| (TableId::new(*table_id), info.clone()))
            .collect();
        let compaction_group_member_tables =
            Self::build_compaction_group_member_tables(&state_table_info);
        Self {
            state_table_info,
            compaction_group_member_tables,
        }
    }

    pub fn to_protobuf(&self) -> HashMap<u32, PbStateTableInfo> {
        self.state_table_info
            .iter()
            .map(|(table_id, info)| (table_id.table_id, info.clone()))
            .collect()
    }

    pub fn apply_delta(
        &mut self,
        delta: &HashMap<TableId, StateTableInfoDelta>,
        removed_table_id: &HashSet<TableId>,
    ) -> HashMap<TableId, Option<StateTableInfo>> {
        let mut changed_table = HashMap::new();
        fn remove_table_from_compaction_group(
            compaction_group_member_tables: &mut HashMap<CompactionGroupId, BTreeSet<TableId>>,
            compaction_group_id: CompactionGroupId,
            table_id: TableId,
        ) {
            let member_tables = compaction_group_member_tables
                .get_mut(&compaction_group_id)
                .expect("should exist");
            assert!(member_tables.remove(&table_id));
            if member_tables.is_empty() {
                assert!(compaction_group_member_tables
                    .remove(&compaction_group_id)
                    .is_some());
            }
        }
        for table_id in removed_table_id {
            if let Some(prev_info) = self.state_table_info.remove(table_id) {
                remove_table_from_compaction_group(
                    &mut self.compaction_group_member_tables,
                    prev_info.compaction_group_id,
                    *table_id,
                );
                assert!(changed_table.insert(*table_id, Some(prev_info)).is_none());
            } else {
                warn!(
                    table_id = table_id.table_id,
                    "table to remove does not exist"
                );
            }
        }
        for (table_id, delta) in delta {
            if removed_table_id.contains(table_id) {
                continue;
            }
            let new_info = StateTableInfo {
                committed_epoch: delta.committed_epoch,
                safe_epoch: delta.safe_epoch,
                compaction_group_id: delta.compaction_group_id,
            };
            match self.state_table_info.entry(*table_id) {
                Entry::Occupied(mut entry) => {
                    let prev_info = entry.get_mut();
                    assert!(
                        new_info.safe_epoch >= prev_info.safe_epoch
                            && new_info.committed_epoch >= prev_info.committed_epoch,
                        "state table info regress. table id: {}, prev_info: {:?}, new_info: {:?}",
                        table_id.table_id,
                        prev_info,
                        new_info
                    );
                    if prev_info.compaction_group_id != new_info.compaction_group_id {
                        // table moved to another compaction group
                        remove_table_from_compaction_group(
                            &mut self.compaction_group_member_tables,
                            prev_info.compaction_group_id,
                            *table_id,
                        );
                        assert!(self
                            .compaction_group_member_tables
                            .entry(new_info.compaction_group_id)
                            .or_default()
                            .insert(*table_id));
                    }
                    let prev_info = replace(prev_info, new_info);
                    changed_table.insert(*table_id, Some(prev_info));
                }
                Entry::Vacant(entry) => {
                    assert!(self
                        .compaction_group_member_tables
                        .entry(new_info.compaction_group_id)
                        .or_default()
                        .insert(*table_id));
                    entry.insert(new_info);
                    changed_table.insert(*table_id, None);
                }
            }
        }
        debug_assert_eq!(
            self.compaction_group_member_tables,
            Self::build_compaction_group_member_tables(&self.state_table_info)
        );
        changed_table
    }

    pub fn info(&self) -> &HashMap<TableId, StateTableInfo> {
        &self.state_table_info
    }

    pub fn compaction_group_member_table_ids(
        &self,
        compaction_group_id: CompactionGroupId,
    ) -> &BTreeSet<TableId> {
        static EMPTY_SET: LazyLock<BTreeSet<TableId>> = LazyLock::new(BTreeSet::new);
        self.compaction_group_member_tables
            .get(&compaction_group_id)
            .unwrap_or_else(|| EMPTY_SET.deref())
    }

    pub fn compaction_group_member_tables(&self) -> &HashMap<CompactionGroupId, BTreeSet<TableId>> {
        &self.compaction_group_member_tables
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HummockVersion {
    pub id: u64,
    pub levels: HashMap<CompactionGroupId, PbLevels>,
    pub max_committed_epoch: u64,
    safe_epoch: u64,
    pub table_watermarks: HashMap<TableId, Arc<TableWatermarks>>,
    pub table_change_log: HashMap<TableId, TableChangeLog>,
    pub state_table_info: HummockVersionStateTableInfo,
}

impl Default for HummockVersion {
    fn default() -> Self {
        HummockVersion::from_protobuf_inner(&PbHummockVersion::default())
    }
}

impl HummockVersion {
    /// Convert the `PbHummockVersion` received from rpc to `HummockVersion`. No need to
    /// maintain backward compatibility.
    pub fn from_rpc_protobuf(pb_version: &PbHummockVersion) -> Self {
        Self::from_protobuf_inner(pb_version)
    }

    /// Convert the `PbHummockVersion` deserialized from persisted state to `HummockVersion`.
    /// We should maintain backward compatibility.
    pub fn from_persisted_protobuf(pb_version: &PbHummockVersion) -> Self {
        Self::from_protobuf_inner(pb_version)
    }

    fn from_protobuf_inner(pb_version: &PbHummockVersion) -> Self {
        Self {
            id: pb_version.id,
            levels: pb_version
                .levels
                .iter()
                .map(|(group_id, levels)| (*group_id as CompactionGroupId, levels.clone()))
                .collect(),
            max_committed_epoch: pb_version.max_committed_epoch,
            safe_epoch: pb_version.safe_epoch,
            table_watermarks: pb_version
                .table_watermarks
                .iter()
                .map(|(table_id, table_watermark)| {
                    (
                        TableId::new(*table_id),
                        Arc::new(TableWatermarks::from_protobuf(table_watermark)),
                    )
                })
                .collect(),
            table_change_log: pb_version
                .table_change_logs
                .iter()
                .map(|(table_id, change_log)| {
                    (
                        TableId::new(*table_id),
                        TableChangeLog::from_protobuf(change_log),
                    )
                })
                .collect(),
            state_table_info: HummockVersionStateTableInfo::from_protobuf(
                &pb_version.state_table_info,
            ),
        }
    }

    pub fn to_protobuf(&self) -> PbHummockVersion {
        PbHummockVersion {
            id: self.id,
            levels: self
                .levels
                .iter()
                .map(|(group_id, levels)| (*group_id as _, levels.clone()))
                .collect(),
            max_committed_epoch: self.max_committed_epoch,
            safe_epoch: self.safe_epoch,
            table_watermarks: self
                .table_watermarks
                .iter()
                .map(|(table_id, watermark)| (table_id.table_id, watermark.to_protobuf()))
                .collect(),
            table_change_logs: self
                .table_change_log
                .iter()
                .map(|(table_id, change_log)| (table_id.table_id, change_log.to_protobuf()))
                .collect(),
            state_table_info: self.state_table_info.to_protobuf(),
        }
    }

    pub fn estimated_encode_len(&self) -> usize {
        self.levels.len() * size_of::<CompactionGroupId>()
            + self
                .levels
                .values()
                .map(|level| level.encoded_len())
                .sum::<usize>()
            + self.table_watermarks.len() * size_of::<u32>()
            + self
                .table_watermarks
                .values()
                .map(|table_watermark| table_watermark.estimated_encode_len())
                .sum::<usize>()
    }

    pub fn next_version_id(&self) -> HummockVersionId {
        self.id + 1
    }

    pub fn need_fill_backward_compatible_state_table_info_delta(&self) -> bool {
        // for backward-compatibility of previous hummock version delta
        self.state_table_info.state_table_info.is_empty()
            && self.levels.values().any(|group| {
                // state_table_info is not previously filled, but there previously exists some tables
                #[expect(deprecated)]
                !group.member_table_ids.is_empty()
            })
    }

    pub fn may_fill_backward_compatible_state_table_info_delta(
        &self,
        delta: &mut HummockVersionDelta,
    ) {
        #[expect(deprecated)]
        // for backward-compatibility of previous hummock version delta
        for (cg_id, group) in &self.levels {
            for table_id in &group.member_table_ids {
                assert!(
                    delta
                        .state_table_info_delta
                        .insert(
                            TableId::new(*table_id),
                            StateTableInfoDelta {
                                committed_epoch: self.max_committed_epoch,
                                safe_epoch: self.safe_epoch,
                                compaction_group_id: *cg_id,
                            }
                        )
                        .is_none(),
                    "duplicate table id {} in cg {}",
                    table_id,
                    cg_id
                );
            }
        }
    }

    pub(crate) fn set_safe_epoch(&mut self, safe_epoch: u64) {
        self.safe_epoch = safe_epoch;
    }

    pub fn visible_table_safe_epoch(&self) -> u64 {
        self.safe_epoch
    }

    pub fn create_init_version(default_compaction_config: Arc<CompactionConfig>) -> HummockVersion {
        let mut init_version = HummockVersion {
            id: FIRST_VERSION_ID,
            levels: Default::default(),
            max_committed_epoch: INVALID_EPOCH,
            safe_epoch: INVALID_EPOCH,
            table_watermarks: HashMap::new(),
            table_change_log: HashMap::new(),
            state_table_info: HummockVersionStateTableInfo::empty(),
        };
        for group_id in [
            StaticCompactionGroupId::StateDefault as CompactionGroupId,
            StaticCompactionGroupId::MaterializedView as CompactionGroupId,
        ] {
            init_version.levels.insert(
                group_id,
                build_initial_compaction_group_levels(group_id, default_compaction_config.as_ref()),
            );
        }
        init_version
    }

    pub fn version_delta_after(&self) -> HummockVersionDelta {
        HummockVersionDelta {
            id: self.next_version_id(),
            prev_id: self.id,
            safe_epoch: self.safe_epoch,
            trivial_move: false,
            max_committed_epoch: self.max_committed_epoch,
            group_deltas: Default::default(),
            new_table_watermarks: HashMap::new(),
            removed_table_ids: HashSet::new(),
            change_log_delta: HashMap::new(),
            state_table_info_delta: Default::default(),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct HummockVersionDelta {
    pub id: u64,
    pub prev_id: u64,
    pub group_deltas: HashMap<CompactionGroupId, PbGroupDeltas>,
    pub max_committed_epoch: u64,
    pub safe_epoch: u64,
    pub trivial_move: bool,
    pub new_table_watermarks: HashMap<TableId, TableWatermarks>,
    pub removed_table_ids: HashSet<TableId>,
    pub change_log_delta: HashMap<TableId, ChangeLogDelta>,
    pub state_table_info_delta: HashMap<TableId, StateTableInfoDelta>,
}

impl Default for HummockVersionDelta {
    fn default() -> Self {
        HummockVersionDelta::from_protobuf_inner(&PbHummockVersionDelta::default())
    }
}

impl HummockVersionDelta {
    /// Convert the `PbHummockVersionDelta` deserialized from persisted state to `HummockVersionDelta`.
    /// We should maintain backward compatibility.
    pub fn from_persisted_protobuf(delta: &PbHummockVersionDelta) -> Self {
        Self::from_protobuf_inner(delta)
    }

    /// Convert the `PbHummockVersionDelta` received from rpc to `HummockVersionDelta`. No need to
    /// maintain backward compatibility.
    pub fn from_rpc_protobuf(delta: &PbHummockVersionDelta) -> Self {
        Self::from_protobuf_inner(delta)
    }

    fn from_protobuf_inner(delta: &PbHummockVersionDelta) -> Self {
        Self {
            id: delta.id,
            prev_id: delta.prev_id,
            group_deltas: delta.group_deltas.clone(),
            max_committed_epoch: delta.max_committed_epoch,
            safe_epoch: delta.safe_epoch,
            trivial_move: delta.trivial_move,
            new_table_watermarks: delta
                .new_table_watermarks
                .iter()
                .map(|(table_id, watermarks)| {
                    (
                        TableId::new(*table_id),
                        TableWatermarks::from_protobuf(watermarks),
                    )
                })
                .collect(),
            removed_table_ids: delta
                .removed_table_ids
                .iter()
                .map(|table_id| TableId::new(*table_id))
                .collect(),
            change_log_delta: delta
                .change_log_delta
                .iter()
                .map(|(table_id, log_delta)| {
                    (
                        TableId::new(*table_id),
                        ChangeLogDelta {
                            new_log: log_delta.new_log.clone(),
                            truncate_epoch: log_delta.truncate_epoch,
                        },
                    )
                })
                .collect(),
            state_table_info_delta: delta
                .state_table_info_delta
                .iter()
                .map(|(table_id, delta)| (TableId::new(*table_id), delta.clone()))
                .collect(),
        }
    }

    pub fn to_protobuf(&self) -> PbHummockVersionDelta {
        PbHummockVersionDelta {
            id: self.id,
            prev_id: self.prev_id,
            group_deltas: self.group_deltas.clone(),
            max_committed_epoch: self.max_committed_epoch,
            safe_epoch: self.safe_epoch,
            trivial_move: self.trivial_move,
            new_table_watermarks: self
                .new_table_watermarks
                .iter()
                .map(|(table_id, watermarks)| (table_id.table_id, watermarks.to_protobuf()))
                .collect(),
            removed_table_ids: self
                .removed_table_ids
                .iter()
                .map(|table_id| table_id.table_id)
                .collect(),
            change_log_delta: self
                .change_log_delta
                .iter()
                .map(|(table_id, log_delta)| (table_id.table_id, log_delta.clone()))
                .collect(),
            state_table_info_delta: self
                .state_table_info_delta
                .iter()
                .map(|(table_id, delta)| (table_id.table_id, delta.clone()))
                .collect(),
        }
    }
}

impl HummockVersionDelta {
    /// Get the newly added object ids from the version delta.
    ///
    /// Note: the result can be false positive because we only collect the set of sst object ids in the `inserted_table_infos`,
    /// but it is possible that the object is moved or split from other compaction groups or levels.
    pub fn newly_added_object_ids(&self) -> HashSet<HummockSstableObjectId> {
        self.group_deltas
            .values()
            .flat_map(|group_deltas| {
                group_deltas.group_deltas.iter().flat_map(|group_delta| {
                    group_delta.delta_type.iter().flat_map(|delta_type| {
                        static EMPTY_VEC: Vec<SstableInfo> = Vec::new();
                        let sst_slice = match delta_type {
                            DeltaType::IntraLevel(level_delta) => &level_delta.inserted_table_infos,
                            DeltaType::GroupConstruct(_)
                            | DeltaType::GroupDestroy(_)
                            | DeltaType::GroupMetaChange(_)
                            | DeltaType::GroupTableChange(_) => &EMPTY_VEC,
                        };
                        sst_slice.iter().map(|sst| sst.object_id)
                    })
                })
            })
            .chain(self.change_log_delta.values().flat_map(|delta| {
                let new_log = delta.new_log.as_ref().unwrap();
                new_log
                    .new_value
                    .iter()
                    .map(|sst| sst.object_id)
                    .chain(new_log.old_value.iter().map(|sst| sst.object_id))
            }))
            .collect()
    }
}
