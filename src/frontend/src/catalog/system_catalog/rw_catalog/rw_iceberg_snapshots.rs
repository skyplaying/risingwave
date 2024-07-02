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

use std::ops::Deref;

use icelake::Table;
use jsonbb::{Value, ValueRef};
use risingwave_common::types::{Fields, JsonbVal, Timestamptz};
use risingwave_connector::sink::iceberg::IcebergConfig;
use risingwave_connector::source::ConnectorProperties;
use risingwave_connector::WithPropertiesExt;
use risingwave_frontend_macro::system_catalog;

use crate::catalog::system_catalog::SysCatalogReaderImpl;
use crate::error::Result;

#[derive(Fields)]
struct RwIcebergSnapshots {
    #[primary_key]
    source_id: i32,
    schema_name: String,
    source_name: String,
    sequence_number: i64,
    snapshot_id: i64,
    timestamp_ms: Option<Timestamptz>,
    manifest_list: String,
    summary: JsonbVal,
}

#[system_catalog(table, "rw_catalog.rw_iceberg_snapshots")]
async fn read(reader: &SysCatalogReaderImpl) -> Result<Vec<RwIcebergSnapshots>> {
    let iceberg_sources = {
        let catalog_reader = reader.catalog_reader.read_guard();
        let schemas = catalog_reader.iter_schemas(&reader.auth_context.database)?;

        let mut iceberg_sources = vec![];
        for schema in schemas {
            for source in schema.iter_source() {
                if source.with_properties.is_iceberg_connector() {
                    iceberg_sources.push((schema.name.clone(), source.deref().clone()))
                }
            }
        }
        iceberg_sources
    };

    let mut result = vec![];
    for (schema_name, source) in iceberg_sources {
        let source_props = source.with_properties.clone();
        let config = ConnectorProperties::extract(source_props, false)?;
        if let ConnectorProperties::Iceberg(iceberg_properties) = config {
            let iceberg_config: IcebergConfig = iceberg_properties.to_iceberg_config();
            let table: Table = iceberg_config.load_table().await?;
            if let Some(snapshots) = &table.current_table_metadata().snapshots {
                result.extend(snapshots.iter().map(|snapshot| {
                    RwIcebergSnapshots {
                        source_id: source.id as i32,
                        schema_name: schema_name.clone(),
                        source_name: source.name.clone(),
                        sequence_number: snapshot.sequence_number,
                        snapshot_id: snapshot.snapshot_id,
                        timestamp_ms: Timestamptz::from_millis(snapshot.timestamp_ms),
                        manifest_list: snapshot.manifest_list.clone(),
                        summary: Value::object(
                            snapshot
                                .summary
                                .iter()
                                .map(|(k, v)| (k.as_str(), ValueRef::String(v))),
                        )
                        .into(),
                    }
                }));
            }
        }
    }
    Ok(result)
}
