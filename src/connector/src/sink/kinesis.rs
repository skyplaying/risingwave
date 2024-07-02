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

use anyhow::{anyhow, Context};
use aws_sdk_kinesis::operation::put_records::builders::PutRecordsFluentBuilder;
use aws_sdk_kinesis::primitives::Blob;
use aws_sdk_kinesis::types::PutRecordsRequestEntry;
use aws_sdk_kinesis::Client as KinesisClient;
use futures::{FutureExt, TryFuture};
use risingwave_common::array::StreamChunk;
use risingwave_common::catalog::Schema;
use risingwave_common::session_config::sink_decouple::SinkDecouple;
use serde_derive::Deserialize;
use serde_with::serde_as;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use with_options::WithOptions;

use super::catalog::SinkFormatDesc;
use super::SinkParam;
use crate::connector_common::KinesisCommon;
use crate::dispatch_sink_formatter_str_key_impl;
use crate::sink::catalog::desc::SinkDesc;
use crate::sink::formatter::SinkFormatterImpl;
use crate::sink::log_store::DeliveryFutureManagerAddFuture;
use crate::sink::writer::{
    AsyncTruncateLogSinkerOf, AsyncTruncateSinkWriter, AsyncTruncateSinkWriterExt, FormattedSink,
};
use crate::sink::{DummySinkCommitCoordinator, Result, Sink, SinkError, SinkWriterParam};

pub const KINESIS_SINK: &str = "kinesis";

#[derive(Clone, Debug)]
pub struct KinesisSink {
    pub config: KinesisSinkConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    format_desc: SinkFormatDesc,
    db_name: String,
    sink_from_name: String,
}

impl TryFrom<SinkParam> for KinesisSink {
    type Error = SinkError;

    fn try_from(param: SinkParam) -> std::result::Result<Self, Self::Error> {
        let schema = param.schema();
        let config = KinesisSinkConfig::from_btreemap(param.properties)?;
        Ok(Self {
            config,
            schema,
            pk_indices: param.downstream_pk,
            format_desc: param
                .format_desc
                .ok_or_else(|| SinkError::Config(anyhow!("missing FORMAT ... ENCODE ...")))?,
            db_name: param.db_name,
            sink_from_name: param.sink_from_name,
        })
    }
}

const KINESIS_SINK_MAX_PENDING_CHUNK_NUM: usize = 64;

impl Sink for KinesisSink {
    type Coordinator = DummySinkCommitCoordinator;
    type LogSinker = AsyncTruncateLogSinkerOf<KinesisSinkWriter>;

    const SINK_NAME: &'static str = KINESIS_SINK;

    fn is_sink_decouple(_desc: &SinkDesc, user_specified: &SinkDecouple) -> Result<bool> {
        match user_specified {
            SinkDecouple::Default | SinkDecouple::Enable => Ok(true),
            SinkDecouple::Disable => Ok(false),
        }
    }

    async fn validate(&self) -> Result<()> {
        // Kinesis requires partition key. There is no builtin support for round-robin as in kafka/pulsar.
        // https://docs.aws.amazon.com/kinesis/latest/APIReference/API_PutRecord.html#Streams-PutRecord-request-PartitionKey
        if self.pk_indices.is_empty() {
            return Err(SinkError::Config(anyhow!(
                "kinesis sink requires partition key (please define in `primary_key` field)",
            )));
        }
        // Check for formatter constructor error, before it is too late for error reporting.
        SinkFormatterImpl::new(
            &self.format_desc,
            self.schema.clone(),
            self.pk_indices.clone(),
            self.db_name.clone(),
            self.sink_from_name.clone(),
            &self.config.common.stream_name,
        )
        .await?;

        // check reachability
        let client = self.config.common.build_client().await?;
        client
            .list_shards()
            .stream_name(&self.config.common.stream_name)
            .send()
            .await
            .context("failed to list shards")
            .map_err(SinkError::Kinesis)?;
        Ok(())
    }

    async fn new_log_sinker(&self, _writer_param: SinkWriterParam) -> Result<Self::LogSinker> {
        Ok(KinesisSinkWriter::new(
            self.config.clone(),
            self.schema.clone(),
            self.pk_indices.clone(),
            &self.format_desc,
            self.db_name.clone(),
            self.sink_from_name.clone(),
        )
        .await?
        .into_log_sinker(KINESIS_SINK_MAX_PENDING_CHUNK_NUM))
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize, WithOptions)]
pub struct KinesisSinkConfig {
    #[serde(flatten)]
    pub common: KinesisCommon,
}

impl KinesisSinkConfig {
    pub fn from_btreemap(properties: BTreeMap<String, String>) -> Result<Self> {
        let config =
            serde_json::from_value::<KinesisSinkConfig>(serde_json::to_value(properties).unwrap())
                .map_err(|e| SinkError::Config(anyhow!(e)))?;
        Ok(config)
    }
}

pub struct KinesisSinkWriter {
    pub config: KinesisSinkConfig,
    formatter: SinkFormatterImpl,
    client: KinesisClient,
}

struct KinesisSinkPayloadWriter {
    // builder should always be `Some`. Making it an option so that we can call
    // builder methods that take the builder ownership as input and return with a new builder.
    builder: Option<PutRecordsFluentBuilder>,
}

impl KinesisSinkWriter {
    pub async fn new(
        config: KinesisSinkConfig,
        schema: Schema,
        pk_indices: Vec<usize>,
        format_desc: &SinkFormatDesc,
        db_name: String,
        sink_from_name: String,
    ) -> Result<Self> {
        let formatter = SinkFormatterImpl::new(
            format_desc,
            schema,
            pk_indices,
            db_name,
            sink_from_name,
            &config.common.stream_name,
        )
        .await?;
        let client = config
            .common
            .build_client()
            .await
            .map_err(|err| SinkError::Kinesis(anyhow!(err)))?;
        Ok(Self {
            config: config.clone(),
            formatter,
            client,
        })
    }

    fn new_payload_writer(&self) -> KinesisSinkPayloadWriter {
        let builder = self
            .client
            .put_records()
            .stream_name(&self.config.common.stream_name);
        KinesisSinkPayloadWriter {
            builder: Some(builder),
        }
    }
}

pub type KinesisSinkPayloadWriterDeliveryFuture =
    impl TryFuture<Ok = (), Error = SinkError> + Unpin + Send + 'static;

impl KinesisSinkPayloadWriter {
    fn put_record(&mut self, key: String, payload: Vec<u8>) {
        self.builder = Some(
            self.builder.take().expect("should not be None").records(
                PutRecordsRequestEntry::builder()
                    .partition_key(key)
                    .data(Blob::new(payload))
                    .build()
                    .expect("should not fail because we have set `data` and `partition_key`"),
            ),
        );
    }

    fn finish(self) -> KinesisSinkPayloadWriterDeliveryFuture {
        async move {
            let builder = self.builder.expect("should not be None");
            let context_fmt = format!(
                "failed to put record to {}",
                builder
                    .get_stream_name()
                    .as_ref()
                    .expect("should have set stream name")
            );
            Retry::spawn(
                ExponentialBackoff::from_millis(100).map(jitter).take(3),
                || builder.clone().send(),
            )
            .await
            .with_context(|| context_fmt.clone())
            .map_err(SinkError::Kinesis)?;
            Ok(())
        }
        .boxed()
    }
}

impl FormattedSink for KinesisSinkPayloadWriter {
    type K = String;
    type V = Vec<u8>;

    async fn write_one(&mut self, k: Option<Self::K>, v: Option<Self::V>) -> Result<()> {
        self.put_record(
            k.ok_or_else(|| SinkError::Kinesis(anyhow!("no key provided")))?,
            v.unwrap_or_default(),
        );
        Ok(())
    }
}

impl AsyncTruncateSinkWriter for KinesisSinkWriter {
    type DeliveryFuture = KinesisSinkPayloadWriterDeliveryFuture;

    async fn write_chunk<'a>(
        &'a mut self,
        chunk: StreamChunk,
        mut add_future: DeliveryFutureManagerAddFuture<'a, Self::DeliveryFuture>,
    ) -> Result<()> {
        let mut payload_writer = self.new_payload_writer();
        dispatch_sink_formatter_str_key_impl!(
            &self.formatter,
            formatter,
            payload_writer.write_chunk(chunk, formatter).await
        )?;

        add_future
            .add_future_may_await(payload_writer.finish())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_kinesis::types::PutRecordsRequestEntry;
    use aws_smithy_types::Blob;

    #[test]
    fn test_kinesis_entry_builder_save_unwrap() {
        PutRecordsRequestEntry::builder()
            .data(Blob::new(b"data"))
            .partition_key("partition-key")
            .build()
            .unwrap();
    }
}
