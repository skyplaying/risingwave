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

use risingwave_hummock_sdk::HummockContextId;
use risingwave_pb::hummock::HummockPinnedSnapshot;

use crate::hummock::model::HUMMOCK_PINNED_SNAPSHOT_CF_NAME;
use crate::model::{MetadataModel, MetadataModelResult};

/// `HummockPinnedSnapshot` tracks pinned snapshots by given context id.
impl MetadataModel for HummockPinnedSnapshot {
    type KeyType = HummockContextId;
    type PbType = HummockPinnedSnapshot;

    fn cf_name() -> String {
        String::from(HUMMOCK_PINNED_SNAPSHOT_CF_NAME)
    }

    fn to_protobuf(&self) -> Self::PbType {
        self.clone()
    }

    fn from_protobuf(prost: Self::PbType) -> Self {
        prost
    }

    fn key(&self) -> MetadataModelResult<Self::KeyType> {
        Ok(self.context_id)
    }
}
