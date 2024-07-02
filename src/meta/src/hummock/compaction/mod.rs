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

#![expect(clippy::arc_with_non_send_sync, reason = "FIXME: later")]

pub mod compaction_config;
mod overlap_strategy;
use risingwave_common::catalog::{TableId, TableOption};
use risingwave_pb::hummock::compact_task::{self, TaskType};

mod picker;
pub mod selector;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

use picker::{LevelCompactionPicker, TierCompactionPicker};
use risingwave_hummock_sdk::{can_concat, CompactionGroupId, HummockCompactionTaskId};
use risingwave_pb::hummock::compaction_config::CompactionMode;
use risingwave_pb::hummock::hummock_version::Levels;
use risingwave_pb::hummock::{CompactTask, CompactionConfig, LevelType};
pub use selector::CompactionSelector;

use self::selector::{EmergencySelector, LocalSelectorStatistic};
use super::check_cg_write_limit;
use crate::hummock::compaction::overlap_strategy::{OverlapStrategy, RangeOverlapStrategy};
use crate::hummock::compaction::picker::CompactionInput;
use crate::hummock::level_handler::LevelHandler;
use crate::hummock::model::CompactionGroup;
use crate::MetaOpts;

#[derive(Clone)]
pub struct CompactStatus {
    pub compaction_group_id: CompactionGroupId,
    pub level_handlers: Vec<LevelHandler>,
}

impl Debug for CompactStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactStatus")
            .field("compaction_group_id", &self.compaction_group_id)
            .field("level_handlers", &self.level_handlers)
            .finish()
    }
}

impl PartialEq for CompactStatus {
    fn eq(&self, other: &Self) -> bool {
        self.level_handlers.eq(&other.level_handlers)
            && self.compaction_group_id == other.compaction_group_id
    }
}

pub struct CompactionTask {
    pub input: CompactionInput,
    pub base_level: usize,
    pub compression_algorithm: String,
    pub target_file_size: u64,
    pub compaction_task_type: compact_task::TaskType,
}

pub fn create_overlap_strategy(compaction_mode: CompactionMode) -> Arc<dyn OverlapStrategy> {
    match compaction_mode {
        CompactionMode::Range => Arc::new(RangeOverlapStrategy::default()),
        CompactionMode::Unspecified => unreachable!(),
    }
}

impl CompactStatus {
    pub fn new(compaction_group_id: CompactionGroupId, max_level: u64) -> CompactStatus {
        let mut level_handlers = vec![];
        for level in 0..=max_level {
            level_handlers.push(LevelHandler::new(level as u32));
        }
        CompactStatus {
            compaction_group_id,
            level_handlers,
        }
    }

    pub fn get_compact_task(
        &mut self,
        levels: &Levels,
        member_table_ids: &BTreeSet<TableId>,
        task_id: HummockCompactionTaskId,
        group: &CompactionGroup,
        stats: &mut LocalSelectorStatistic,
        selector: &mut Box<dyn CompactionSelector>,
        table_id_to_options: HashMap<u32, TableOption>,
        developer_config: Arc<CompactionDeveloperConfig>,
    ) -> Option<CompactionTask> {
        // When we compact the files, we must make the result of compaction meet the following
        // conditions, for any user key, the epoch of it in the file existing in the lower
        // layer must be larger.
        if let Some(task) = selector.pick_compaction(
            task_id,
            group,
            levels,
            member_table_ids,
            &mut self.level_handlers,
            stats,
            table_id_to_options.clone(),
            developer_config.clone(),
        ) {
            return Some(task);
        } else {
            let compaction_group_config = &group.compaction_config;
            if check_cg_write_limit(levels, compaction_group_config.as_ref()).is_write_stop()
                && compaction_group_config.enable_emergency_picker
            {
                return EmergencySelector::default().pick_compaction(
                    task_id,
                    group,
                    levels,
                    member_table_ids,
                    &mut self.level_handlers,
                    stats,
                    table_id_to_options,
                    developer_config,
                );
            }
        }

        None
    }

    pub fn is_trivial_move_task(task: &CompactTask) -> bool {
        if task.task_type() != TaskType::Dynamic && task.task_type() != TaskType::Emergency {
            return false;
        }

        if task.input_ssts.len() == 1 {
            return task.input_ssts[0].level_idx == 0
                && can_concat(&task.input_ssts[0].table_infos);
        } else if task.input_ssts.len() != 2
            || task.input_ssts[0].level_type() != LevelType::Nonoverlapping
        {
            return false;
        }

        // it may be a manual compaction task
        if task.input_ssts[0].level_idx == task.input_ssts[1].level_idx
            && task.input_ssts[0].level_idx > 0
        {
            return false;
        }

        if task.input_ssts[1].level_idx == task.target_level
            && task.input_ssts[1].table_infos.is_empty()
        {
            return true;
        }

        false
    }

    pub fn is_trivial_reclaim(task: &CompactTask) -> bool {
        let exist_table_ids = HashSet::<u32>::from_iter(task.existing_table_ids.clone());
        task.input_ssts.iter().all(|level| {
            level.table_infos.iter().all(|sst| {
                sst.table_ids
                    .iter()
                    .all(|table_id| !exist_table_ids.contains(table_id))
            })
        })
    }

    /// Declares a task as either succeeded, failed or canceled.
    pub fn report_compact_task(&mut self, compact_task: &CompactTask) {
        for level in &compact_task.input_ssts {
            self.level_handlers[level.level_idx as usize].remove_task(compact_task.task_id);
        }
    }

    pub fn compaction_group_id(&self) -> CompactionGroupId {
        self.compaction_group_id
    }
}

pub fn create_compaction_task(
    compaction_config: &CompactionConfig,
    input: CompactionInput,
    base_level: usize,
    compaction_task_type: compact_task::TaskType,
) -> CompactionTask {
    let target_file_size = if input.target_level == 0 {
        compaction_config.target_file_size_base
    } else {
        assert!(input.target_level >= base_level);
        let step = (input.target_level - base_level) / 2;
        compaction_config.target_file_size_base << step
    };

    CompactionTask {
        compression_algorithm: get_compression_algorithm(
            compaction_config,
            base_level,
            input.target_level,
        ),
        base_level,
        input,
        target_file_size,
        compaction_task_type,
    }
}

pub fn get_compression_algorithm(
    compaction_config: &CompactionConfig,
    base_level: usize,
    level: usize,
) -> String {
    if level == 0 || level < base_level {
        compaction_config.compression_algorithm[0].clone()
    } else {
        let idx = level - base_level + 1;
        compaction_config.compression_algorithm[idx].clone()
    }
}

pub struct CompactionDeveloperConfig {
    /// l0 picker whether to select trivial move task
    pub enable_trivial_move: bool,

    /// l0 multi level picker whether to check the overlap accuracy between sub levels
    pub enable_check_task_level_overlap: bool,
}

impl CompactionDeveloperConfig {
    pub fn new_from_meta_opts(opts: &MetaOpts) -> Self {
        Self {
            enable_trivial_move: opts.enable_trivial_move,
            enable_check_task_level_overlap: opts.enable_check_task_level_overlap,
        }
    }
}

impl Default for CompactionDeveloperConfig {
    fn default() -> Self {
        Self {
            enable_trivial_move: true,
            enable_check_task_level_overlap: true,
        }
    }
}
