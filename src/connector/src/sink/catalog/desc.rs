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

use std::collections::BTreeMap;

use itertools::Itertools;
use risingwave_common::catalog::{
    ColumnCatalog, ConnectionId, CreateType, DatabaseId, SchemaId, TableId, UserId,
};
use risingwave_common::util::sort_util::ColumnOrder;
use risingwave_pb::secret::PbSecretRef;
use risingwave_pb::stream_plan::PbSinkDesc;

use super::{SinkCatalog, SinkFormatDesc, SinkId, SinkType};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SinkDesc {
    /// Id of the sink. For debug now.
    pub id: SinkId,

    /// Name of the sink. For debug now.
    pub name: String,

    /// Full SQL definition of the sink. For debug now.
    pub definition: String,

    /// All columns of the sink. Note that this is NOT sorted by columnId in the vector.
    pub columns: Vec<ColumnCatalog>,

    /// Primiary keys of the sink. Derived by the frontend.
    pub plan_pk: Vec<ColumnOrder>,

    /// User-defined primary key indices for upsert sink.
    pub downstream_pk: Vec<usize>,

    /// Distribution key indices of the sink. For example, if `distribution_key = [1, 2]`, then the
    /// distribution keys will be `columns[1]` and `columns[2]`.
    pub distribution_key: Vec<usize>,

    /// The properties of the sink.
    pub properties: BTreeMap<String, String>,

    // The append-only behavior of the physical sink connector. Frontend will determine `sink_type`
    // based on both its own derivation on the append-only attribute and other user-specified
    // options in `properties`.
    pub sink_type: SinkType,

    // The format and encode of the sink.
    pub format_desc: Option<SinkFormatDesc>,

    /// Name of the database
    pub db_name: String,

    /// Name of the "table" field for Debezium. If the sink is from table or mv,
    /// it is the name of table/mv. Otherwise, it is the name of the sink.
    pub sink_from_name: String,

    /// Id of the target table for sink into table.
    pub target_table: Option<TableId>,

    /// See the same name field in `SinkWriterParam`.
    pub extra_partition_col_idx: Option<usize>,

    /// Whether the sink job should run in foreground or background.
    pub create_type: CreateType,
}

impl SinkDesc {
    pub fn into_catalog(
        self,
        schema_id: SchemaId,
        database_id: DatabaseId,
        owner: UserId,
        connection_id: Option<ConnectionId>,
        dependent_relations: Vec<TableId>,
        secret_ref: BTreeMap<String, PbSecretRef>,
    ) -> SinkCatalog {
        SinkCatalog {
            id: self.id,
            schema_id,
            database_id,
            name: self.name,
            definition: self.definition,
            columns: self.columns,
            plan_pk: self.plan_pk,
            downstream_pk: self.downstream_pk,
            distribution_key: self.distribution_key,
            owner,
            dependent_relations,
            properties: self.properties.into_iter().collect(),
            sink_type: self.sink_type,
            format_desc: self.format_desc,
            connection_id,
            created_at_epoch: None,
            initialized_at_epoch: None,
            db_name: self.db_name,
            sink_from_name: self.sink_from_name,
            target_table: self.target_table,
            created_at_cluster_version: None,
            initialized_at_cluster_version: None,
            create_type: self.create_type,
            secret_ref,
        }
    }

    pub fn to_proto(&self) -> PbSinkDesc {
        PbSinkDesc {
            id: self.id.sink_id,
            name: self.name.clone(),
            definition: self.definition.clone(),
            column_catalogs: self
                .columns
                .iter()
                .map(|column| column.to_protobuf())
                .collect_vec(),
            plan_pk: self.plan_pk.iter().map(|k| k.to_protobuf()).collect_vec(),
            downstream_pk: self.downstream_pk.iter().map(|idx| *idx as _).collect_vec(),
            distribution_key: self.distribution_key.iter().map(|k| *k as _).collect_vec(),
            properties: self.properties.clone().into_iter().collect(),
            sink_type: self.sink_type.to_proto() as i32,
            format_desc: self.format_desc.as_ref().map(|f| f.to_proto()),
            db_name: self.db_name.clone(),
            sink_from_name: self.sink_from_name.clone(),
            target_table: self.target_table.map(|table_id| table_id.table_id()),
            extra_partition_col_idx: self.extra_partition_col_idx.map(|idx| idx as u64),
        }
    }
}
