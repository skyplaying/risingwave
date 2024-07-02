//  Copyright 2024 RisingWave Labs
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
//
// Copyright (c) 2011-present, Facebook, Inc.  All rights reserved.
// This source code is licensed under both the GPLv2 (found in the
// COPYING file in the root directory) and Apache 2.0 License
// (found in the LICENSE.Apache file in the root directory).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use risingwave_common::catalog::{TableId, TableOption};
use risingwave_hummock_sdk::HummockCompactionTaskId;
use risingwave_pb::hummock::compact_task;
use risingwave_pb::hummock::hummock_version::Levels;

use super::{CompactionSelector, DynamicLevelSelectorCore};
use crate::hummock::compaction::picker::{SpaceReclaimCompactionPicker, SpaceReclaimPickerState};
use crate::hummock::compaction::{
    create_compaction_task, CompactionDeveloperConfig, CompactionTask, LocalSelectorStatistic,
};
use crate::hummock::level_handler::LevelHandler;
use crate::hummock::model::CompactionGroup;

#[derive(Default)]
pub struct SpaceReclaimCompactionSelector {
    state: HashMap<u64, SpaceReclaimPickerState>,
}

impl CompactionSelector for SpaceReclaimCompactionSelector {
    fn pick_compaction(
        &mut self,
        task_id: HummockCompactionTaskId,
        group: &CompactionGroup,
        levels: &Levels,
        member_table_ids: &BTreeSet<TableId>,
        level_handlers: &mut [LevelHandler],
        _selector_stats: &mut LocalSelectorStatistic,
        _table_id_to_options: HashMap<u32, TableOption>,
        developer_config: Arc<CompactionDeveloperConfig>,
    ) -> Option<CompactionTask> {
        let dynamic_level_core =
            DynamicLevelSelectorCore::new(group.compaction_config.clone(), developer_config);
        let mut picker = SpaceReclaimCompactionPicker::new(
            group.compaction_config.max_space_reclaim_bytes,
            member_table_ids
                .iter()
                .map(|table_id| table_id.table_id)
                .collect(),
        );
        let ctx = dynamic_level_core.calculate_level_base_size(levels);
        let state = self.state.entry(group.group_id).or_default();

        let compaction_input = picker.pick_compaction(levels, level_handlers, state)?;
        compaction_input.add_pending_task(task_id, level_handlers);

        Some(create_compaction_task(
            dynamic_level_core.get_config(),
            compaction_input,
            ctx.base_level,
            self.task_type(),
        ))
    }

    fn name(&self) -> &'static str {
        "SpaceReclaimCompaction"
    }

    fn task_type(&self) -> compact_task::TaskType {
        compact_task::TaskType::SpaceReclaim
    }
}
