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

use std::fmt::Debug;
use std::sync::Arc;

use anyhow::Context;
use apache_avro::types::Value;
use apache_avro::{from_avro_datum, Reader, Schema};
use risingwave_common::{bail, try_match_expand};
use risingwave_connector_codec::decoder::avro::{
    avro_schema_to_column_descs, AvroAccess, AvroParseOptions, ResolvedAvroSchema,
};
use risingwave_pb::plan_common::ColumnDesc;

use super::ConfluentSchemaCache;
use crate::error::ConnectorResult;
use crate::parser::unified::AccessImpl;
use crate::parser::util::bytes_from_url;
use crate::parser::{AccessBuilder, AvroProperties, EncodingProperties, EncodingType, MapHandling};
use crate::schema::schema_registry::{
    extract_schema_id, get_subject_by_strategy, handle_sr_list, Client,
};

// Default avro access builder
#[derive(Debug)]
pub struct AvroAccessBuilder {
    schema: Arc<ResolvedAvroSchema>,
    /// Refer to [`AvroParserConfig::writer_schema_cache`].
    pub writer_schema_cache: Option<Arc<ConfluentSchemaCache>>,
    value: Option<Value>,
}

impl AccessBuilder for AvroAccessBuilder {
    async fn generate_accessor(&mut self, payload: Vec<u8>) -> ConnectorResult<AccessImpl<'_>> {
        self.value = self.parse_avro_value(&payload).await?;
        Ok(AccessImpl::Avro(AvroAccess::new(
            self.value.as_ref().unwrap(),
            AvroParseOptions::create(&self.schema.resolved_schema),
        )))
    }
}

impl AvroAccessBuilder {
    pub fn new(config: AvroParserConfig, encoding_type: EncodingType) -> ConnectorResult<Self> {
        let AvroParserConfig {
            schema,
            key_schema,
            writer_schema_cache,
            ..
        } = config;
        Ok(Self {
            schema: match encoding_type {
                EncodingType::Key => key_schema.context("Avro with empty key schema")?,
                EncodingType::Value => schema,
            },
            writer_schema_cache,
            value: None,
        })
    }

    /// Note: we should use unresolved schema to parsing bytes into avro value.
    /// Otherwise it's an invalid schema and parsing will fail. (Avro error: Two named schema defined for same fullname)
    async fn parse_avro_value(&self, payload: &[u8]) -> ConnectorResult<Option<Value>> {
        // parse payload to avro value
        // if use confluent schema, get writer schema from confluent schema registry
        if let Some(resolver) = &self.writer_schema_cache {
            let (schema_id, mut raw_payload) = extract_schema_id(payload)?;
            let writer_schema = resolver.get_by_id(schema_id).await?;
            Ok(Some(from_avro_datum(
                writer_schema.as_ref(),
                &mut raw_payload,
                Some(&self.schema.original_schema),
            )?))
        } else {
            let mut reader = Reader::with_schema(&self.schema.original_schema, payload)?;
            match reader.next() {
                Some(Ok(v)) => Ok(Some(v)),
                Some(Err(e)) => Err(e)?,
                None => bail!("avro parse unexpected eof"),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AvroParserConfig {
    pub schema: Arc<ResolvedAvroSchema>,
    pub key_schema: Option<Arc<ResolvedAvroSchema>>,
    /// Writer schema is the schema used to write the data. When parsing Avro data, the exactly same schema
    /// must be used to decode the message, and then convert it with the reader schema.
    pub writer_schema_cache: Option<Arc<ConfluentSchemaCache>>,

    pub map_handling: Option<MapHandling>,
}

impl AvroParserConfig {
    pub async fn new(encoding_properties: EncodingProperties) -> ConnectorResult<Self> {
        let AvroProperties {
            use_schema_registry,
            row_schema_location: schema_location,
            client_config,
            aws_auth_props,
            topic,
            enable_upsert,
            record_name,
            key_record_name,
            name_strategy,
            map_handling,
        } = try_match_expand!(encoding_properties, EncodingProperties::Avro)?;
        let url = handle_sr_list(schema_location.as_str())?;
        if use_schema_registry {
            let client = Client::new(url, &client_config)?;
            let resolver = ConfluentSchemaCache::new(client);

            let subject_key = if enable_upsert {
                Some(get_subject_by_strategy(
                    &name_strategy,
                    topic.as_str(),
                    key_record_name.as_deref(),
                    true,
                )?)
            } else {
                if let Some(name) = &key_record_name {
                    bail!("unused FORMAT ENCODE option: key.message='{name}'");
                }
                None
            };
            let subject_value = get_subject_by_strategy(
                &name_strategy,
                topic.as_str(),
                record_name.as_deref(),
                false,
            )?;
            tracing::debug!("infer key subject {subject_key:?}, value subject {subject_value}");

            Ok(Self {
                schema: Arc::new(ResolvedAvroSchema::create(
                    resolver.get_by_subject(&subject_value).await?,
                )?),
                key_schema: if let Some(subject_key) = subject_key {
                    Some(Arc::new(ResolvedAvroSchema::create(
                        resolver.get_by_subject(&subject_key).await?,
                    )?))
                } else {
                    None
                },
                writer_schema_cache: Some(Arc::new(resolver)),
                map_handling,
            })
        } else {
            if enable_upsert {
                bail!("avro upsert without schema registry is not supported");
            }
            let url = url.first().unwrap();
            let schema_content = bytes_from_url(url, aws_auth_props.as_ref()).await?;
            let schema = Schema::parse_reader(&mut schema_content.as_slice())
                .context("failed to parse avro schema")?;
            Ok(Self {
                schema: Arc::new(ResolvedAvroSchema::create(Arc::new(schema))?),
                key_schema: None,
                writer_schema_cache: None,
                map_handling,
            })
        }
    }

    pub fn map_to_columns(&self) -> ConnectorResult<Vec<ColumnDesc>> {
        avro_schema_to_column_descs(&self.schema.resolved_schema, self.map_handling)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod test {
    use std::env;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::ops::Sub;
    use std::path::PathBuf;

    use apache_avro::schema::RecordSchema;
    use apache_avro::types::Record;
    use apache_avro::{Codec, Days, Duration, Millis, Months, Writer};
    use itertools::Itertools;
    use risingwave_common::array::Op;
    use risingwave_common::catalog::ColumnId;
    use risingwave_common::row::Row;
    use risingwave_common::types::{DataType, Date};
    use risingwave_common::util::iter_util::ZipEqFast;
    use risingwave_pb::catalog::StreamSourceInfo;
    use risingwave_pb::plan_common::{PbEncodeType, PbFormatType};
    use url::Url;

    use super::*;
    use crate::connector_common::AwsAuthProps;
    use crate::parser::plain_parser::PlainParser;
    use crate::parser::{AccessBuilderImpl, SourceStreamChunkBuilder, SpecificParserConfig};
    use crate::source::{SourceColumnDesc, SourceContext};

    fn test_data_path(file_name: &str) -> String {
        let curr_dir = env::current_dir().unwrap().into_os_string();
        curr_dir.into_string().unwrap() + "/src/test_data/" + file_name
    }

    fn e2e_file_path(file_name: &str) -> String {
        let curr_dir = env::current_dir().unwrap().into_os_string();
        let binding = PathBuf::from(curr_dir);
        let dir = binding.parent().unwrap().parent().unwrap();
        dir.join("scripts/source/test_data/")
            .join(file_name)
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    #[ignore]
    async fn test_load_schema_from_s3() {
        let schema_location = "s3://mingchao-schemas/complex-schema.avsc".to_string();
        let url = Url::parse(&schema_location).unwrap();
        let aws_auth_config: AwsAuthProps =
            serde_json::from_str(r#"region":"ap-southeast-1"#).unwrap();
        let schema_content = bytes_from_url(&url, Some(&aws_auth_config)).await;
        assert!(schema_content.is_ok());
        let schema = Schema::parse_reader(&mut schema_content.unwrap().as_slice());
        assert!(schema.is_ok());
        println!("schema = {:?}", schema.unwrap());
    }

    #[tokio::test]
    async fn test_load_schema_from_local() {
        let schema_location = Url::from_file_path(test_data_path("complex-schema.avsc")).unwrap();
        let schema_content = bytes_from_url(&schema_location, None).await;
        assert!(schema_content.is_ok());
        let schema = Schema::parse_reader(&mut schema_content.unwrap().as_slice());
        assert!(schema.is_ok());
        println!("schema = {:?}", schema.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn test_load_schema_from_https() {
        let schema_location =
            "https://mingchao-schemas.s3.ap-southeast-1.amazonaws.com/complex-schema.avsc";
        let url = Url::parse(schema_location).unwrap();
        let schema_content = bytes_from_url(&url, None).await;
        assert!(schema_content.is_ok());
        let schema = Schema::parse_reader(&mut schema_content.unwrap().as_slice());
        assert!(schema.is_ok());
        println!("schema = {:?}", schema.unwrap());
    }

    async fn new_avro_conf_from_local(file_name: &str) -> ConnectorResult<AvroParserConfig> {
        let schema_path = format!("file://{}", test_data_path(file_name));
        let info = StreamSourceInfo {
            row_schema_location: schema_path.clone(),
            use_schema_registry: false,
            format: PbFormatType::Plain.into(),
            row_encode: PbEncodeType::Avro.into(),
            ..Default::default()
        };
        let parser_config = SpecificParserConfig::new(&info, &Default::default())?;
        AvroParserConfig::new(parser_config.encoding_config).await
    }

    async fn new_avro_parser_from_local(file_name: &str) -> ConnectorResult<PlainParser> {
        let conf = new_avro_conf_from_local(file_name).await?;

        Ok(PlainParser {
            key_builder: None,
            payload_builder: AccessBuilderImpl::Avro(AvroAccessBuilder::new(
                conf,
                EncodingType::Value,
            )?),
            rw_columns: Vec::default(),
            source_ctx: SourceContext::dummy().into(),
            transaction_meta_builder: None,
        })
    }

    #[tokio::test]
    async fn test_avro_parser() {
        let mut parser = new_avro_parser_from_local("simple-schema.avsc")
            .await
            .unwrap();
        let builder = try_match_expand!(&parser.payload_builder, AccessBuilderImpl::Avro).unwrap();
        let schema = builder.schema.original_schema.clone();
        let record = build_avro_data(&schema);
        assert_eq!(record.fields.len(), 11);
        let mut writer = Writer::with_codec(&schema, Vec::new(), Codec::Snappy);
        writer.append(record.clone()).unwrap();
        let flush = writer.flush().unwrap();
        assert!(flush > 0);
        let input_data = writer.into_inner().unwrap();
        let columns = build_rw_columns();
        let mut builder = SourceStreamChunkBuilder::with_capacity(columns, 1);
        {
            let writer = builder.row_writer();
            parser
                .parse_inner(None, Some(input_data), writer)
                .await
                .unwrap();
        }
        let chunk = builder.finish();
        let (op, row) = chunk.rows().next().unwrap();
        assert_eq!(op, Op::Insert);

        expect_test::expect![[r#"
            ("id", Int(32)) => Some(Int32(32))
            ("sequence_id", Long(64)) => Some(Int64(64))
            ("name", Union(1, String("str_value"))) => Some(Utf8("str_value"))
            ("score", Float(32.0)) => Some(Float32(OrderedFloat(32.0)))
            ("avg_score", Double(64.0)) => Some(Float64(OrderedFloat(64.0)))
            ("is_lasted", Boolean(true)) => Some(Bool(true))
            ("entrance_date", Date(0)) => Some(Date(Date(1970-01-01)))
            ("birthday", TimestampMillis(0)) => Some(Timestamptz(Timestamptz(0)))
            ("anniversary", TimestampMicros(0)) => Some(Timestamptz(Timestamptz(0)))
            ("passed", Duration(Duration { months: Months(1), days: Days(1), millis: Millis(1000) })) => Some(Interval(Interval { months: 1, days: 1, usecs: 1000000 }))
            ("bytes", Bytes([1, 2, 3, 4, 5])) => Some(Bytea([1, 2, 3, 4, 5]))"#]].assert_eq(&format!(
            "{}",
            record
                .fields
                .iter()
                .zip_eq_fast(row.iter())
                .format_with("\n", |(avro, datum), f| {
                    f(&format_args!("{:?} => {:?}", avro, datum))
                })
        ));
    }

    fn build_rw_columns() -> Vec<SourceColumnDesc> {
        vec![
            SourceColumnDesc::simple("id", DataType::Int32, ColumnId::from(0)),
            SourceColumnDesc::simple("sequence_id", DataType::Int64, ColumnId::from(1)),
            SourceColumnDesc::simple("name", DataType::Varchar, ColumnId::from(2)),
            SourceColumnDesc::simple("score", DataType::Float32, ColumnId::from(3)),
            SourceColumnDesc::simple("avg_score", DataType::Float64, ColumnId::from(4)),
            SourceColumnDesc::simple("is_lasted", DataType::Boolean, ColumnId::from(5)),
            SourceColumnDesc::simple("entrance_date", DataType::Date, ColumnId::from(6)),
            SourceColumnDesc::simple("birthday", DataType::Timestamptz, ColumnId::from(7)),
            SourceColumnDesc::simple("anniversary", DataType::Timestamptz, ColumnId::from(8)),
            SourceColumnDesc::simple("passed", DataType::Interval, ColumnId::from(9)),
            SourceColumnDesc::simple("bytes", DataType::Bytea, ColumnId::from(10)),
        ]
    }

    fn build_field(schema: &Schema) -> Option<Value> {
        match schema {
            Schema::String => Some(Value::String("str_value".to_string())),
            Schema::Int => Some(Value::Int(32_i32)),
            Schema::Long => Some(Value::Long(64_i64)),
            Schema::Float => Some(Value::Float(32_f32)),
            Schema::Double => Some(Value::Double(64_f64)),
            Schema::Boolean => Some(Value::Boolean(true)),
            Schema::Bytes => Some(Value::Bytes(vec![1, 2, 3, 4, 5])),

            Schema::Date => {
                let original_date = Date::from_ymd_uncheck(1970, 1, 1).and_hms_uncheck(0, 0, 0);
                let naive_date = Date::from_ymd_uncheck(1970, 1, 1).and_hms_uncheck(0, 0, 0);
                let num_days = naive_date.0.sub(original_date.0).num_days() as i32;
                Some(Value::Date(num_days))
            }
            Schema::TimestampMillis => {
                let datetime = Date::from_ymd_uncheck(1970, 1, 1).and_hms_uncheck(0, 0, 0);
                let timestamp_mills =
                    Value::TimestampMillis(datetime.0.and_utc().timestamp() * 1_000);
                Some(timestamp_mills)
            }
            Schema::TimestampMicros => {
                let datetime = Date::from_ymd_uncheck(1970, 1, 1).and_hms_uncheck(0, 0, 0);
                let timestamp_micros =
                    Value::TimestampMicros(datetime.0.and_utc().timestamp() * 1_000_000);
                Some(timestamp_micros)
            }
            Schema::Duration => {
                let months = Months::new(1);
                let days = Days::new(1);
                let millis = Millis::new(1000);
                Some(Value::Duration(Duration::new(months, days, millis)))
            }

            Schema::Union(union_schema) => {
                let inner_schema = union_schema
                    .variants()
                    .iter()
                    .find_or_first(|s| !matches!(s, &&Schema::Null))
                    .unwrap();

                match build_field(inner_schema) {
                    None => {
                        let index_of_union = union_schema
                            .find_schema_with_known_schemata::<&Schema>(&Value::Null, None, &None)
                            .unwrap()
                            .0 as u32;
                        Some(Value::Union(index_of_union, Box::new(Value::Null)))
                    }
                    Some(value) => {
                        let index_of_union = union_schema
                            .find_schema_with_known_schemata::<&Schema>(&value, None, &None)
                            .unwrap()
                            .0 as u32;
                        Some(Value::Union(index_of_union, Box::new(value)))
                    }
                }
            }
            _ => None,
        }
    }

    fn build_avro_data(schema: &Schema) -> Record<'_> {
        let mut record = Record::new(schema).unwrap();
        if let Schema::Record(RecordSchema {
            name: _, fields, ..
        }) = schema.clone()
        {
            for field in &fields {
                let value = build_field(&field.schema)
                    .unwrap_or_else(|| panic!("No value defined for field, {}", field.name));
                record.put(field.name.as_str(), value)
            }
        }
        record
    }

    #[tokio::test]
    async fn test_map_to_columns() {
        let conf = new_avro_conf_from_local("simple-schema.avsc")
            .await
            .unwrap();
        let columns = conf.map_to_columns().unwrap();
        assert_eq!(columns.len(), 11);
        println!("{:?}", columns);
    }

    #[tokio::test]
    async fn test_new_avro_parser() {
        let avro_parser_rs = new_avro_parser_from_local("simple-schema.avsc").await;
        let avro_parser = avro_parser_rs.unwrap();
        println!("avro_parser = {:?}", avro_parser);
    }

    #[tokio::test]
    async fn test_avro_union_type() {
        let parser = new_avro_parser_from_local("union-schema.avsc")
            .await
            .unwrap();
        let builder = try_match_expand!(&parser.payload_builder, AccessBuilderImpl::Avro).unwrap();
        let schema = &builder.schema.original_schema;
        let mut null_record = Record::new(schema).unwrap();
        null_record.put("id", Value::Int(5));
        null_record.put("age", Value::Union(0, Box::new(Value::Null)));
        null_record.put("sequence_id", Value::Union(0, Box::new(Value::Null)));
        null_record.put("name", Value::Union(0, Box::new(Value::Null)));
        null_record.put("score", Value::Union(1, Box::new(Value::Null)));
        null_record.put("avg_score", Value::Union(0, Box::new(Value::Null)));
        null_record.put("is_lasted", Value::Union(0, Box::new(Value::Null)));
        null_record.put("entrance_date", Value::Union(0, Box::new(Value::Null)));
        null_record.put("birthday", Value::Union(0, Box::new(Value::Null)));
        null_record.put("anniversary", Value::Union(0, Box::new(Value::Null)));

        let mut writer = Writer::new(schema, Vec::new());
        writer.append(null_record).unwrap();
        writer.flush().unwrap();

        let record = build_avro_data(schema);
        writer.append(record).unwrap();
        writer.flush().unwrap();

        let records = writer.into_inner().unwrap();

        let reader: Vec<_> = Reader::with_schema(schema, &records[..]).unwrap().collect();
        assert_eq!(2, reader.len());
        let null_record_expected: Vec<(String, Value)> = vec![
            ("id".to_string(), Value::Int(5)),
            ("age".to_string(), Value::Union(0, Box::new(Value::Null))),
            (
                "sequence_id".to_string(),
                Value::Union(0, Box::new(Value::Null)),
            ),
            ("name".to_string(), Value::Union(0, Box::new(Value::Null))),
            ("score".to_string(), Value::Union(1, Box::new(Value::Null))),
            (
                "avg_score".to_string(),
                Value::Union(0, Box::new(Value::Null)),
            ),
            (
                "is_lasted".to_string(),
                Value::Union(0, Box::new(Value::Null)),
            ),
            (
                "entrance_date".to_string(),
                Value::Union(0, Box::new(Value::Null)),
            ),
            (
                "birthday".to_string(),
                Value::Union(0, Box::new(Value::Null)),
            ),
            (
                "anniversary".to_string(),
                Value::Union(0, Box::new(Value::Null)),
            ),
        ];
        let null_record_value = reader.first().unwrap().as_ref().unwrap();
        match null_record_value {
            Value::Record(values) => {
                assert_eq!(values, &null_record_expected)
            }
            _ => unreachable!(),
        }
    }

    // run this script when updating `simple-schema.avsc`, the script will generate new value in
    // `avro_simple_schema_bin.1`
    #[ignore]
    #[tokio::test]
    async fn update_avro_payload() {
        let conf = new_avro_conf_from_local("simple-schema.avsc")
            .await
            .unwrap();
        let mut writer = Writer::new(conf.schema.original_schema.as_ref(), Vec::new());
        let record = build_avro_data(conf.schema.original_schema.as_ref());
        writer.append(record).unwrap();
        let encoded = writer.into_inner().unwrap();
        println!("path = {:?}", e2e_file_path("avro_simple_schema_bin.1"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(e2e_file_path("avro_simple_schema_bin.1"))
            .unwrap();
        file.write_all(encoded.as_slice()).unwrap();
        println!(
            "encoded = {:?}",
            String::from_utf8_lossy(encoded.as_slice())
        );
    }
}
