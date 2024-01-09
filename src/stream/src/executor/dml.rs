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
use std::mem;
use std::num::NonZeroU64;

use either::Either;
use futures::StreamExt;
use futures_async_stream::try_stream;
use risingwave_common::array::StreamChunk;
use risingwave_common::catalog::{ColumnDesc, Schema, TableId, TableVersionId};
use risingwave_common::transaction::transaction_id::TxnId;
use risingwave_common::transaction::transaction_message::TxnMsg;
use risingwave_source::dml_manager::DmlManagerRef;

use super::error::StreamExecutorError;
use super::{
    expect_first_barrier, BoxedExecutor, BoxedMessageStream, Executor, ExecutorInfo, Message,
    Mutation, PkIndicesRef,
};
use crate::common::StreamChunkBuilder;
use crate::executor::stream_reader::StreamReaderWithPause;

/// [`DmlExecutor`] accepts both stream data and batch data for data manipulation on a specific
/// table. The two streams will be merged into one and then sent to downstream.
pub struct DmlExecutor {
    info: ExecutorInfo,

    upstream: BoxedExecutor,

    /// Stores the information of batch data channels.
    dml_manager: DmlManagerRef,

    // Id of the table on which DML performs.
    table_id: TableId,

    // Version of the table on which DML performs.
    table_version_id: TableVersionId,

    // Column descriptions of the table.
    column_descs: Vec<ColumnDesc>,

    chunk_size: usize,

    // Rate limit for DML.
    rate_limit: Option<NonZeroU64>,
}

/// If a transaction's data is less than `MAX_CHUNK_FOR_ATOMICITY` * `CHUNK_SIZE`, we can provide
/// atomicity. Otherwise, it is possible that part of transaction's data is sent to the downstream
/// without barrier boundaries. There are some cases that could cause non-atomicity for large
/// transaction. 1. The system crashes.
/// 2. Actor scale-in or migration.
/// 3. Dml's batch query error occurs at the middle of its execution. (e.g. Remove UDF function
/// server become unavailable).
const MAX_CHUNK_FOR_ATOMICITY: usize = 32;

#[derive(Debug, Default)]
struct TxnBuffer {
    vec: Vec<StreamChunk>,
    // When vec size exceeds `MAX_CHUNK_FOR_ATOMICITY`, set true to `overflow`.
    overflow: bool,
}

impl DmlExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        info: ExecutorInfo,
        upstream: BoxedExecutor,
        dml_manager: DmlManagerRef,
        table_id: TableId,
        table_version_id: TableVersionId,
        column_descs: Vec<ColumnDesc>,
        chunk_size: usize,
        rate_limit: Option<NonZeroU64>,
    ) -> Self {
        // If chunk_size > rate_limit, we should decrease its size, so rate limit can take effect.
        let chunk_size = rate_limit
            .map(|rate_limit| std::cmp::min(chunk_size, rate_limit.get() as usize))
            .unwrap_or(chunk_size);
        tracing::debug!("chunk_size: {}", chunk_size);
        Self {
            info,
            upstream,
            dml_manager,
            table_id,
            table_version_id,
            column_descs,
            chunk_size,
            rate_limit,
        }
    }

    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_inner(self: Box<Self>) {
        let mut upstream = self.upstream.execute();

        // The first barrier message should be propagated.
        let barrier = expect_first_barrier(&mut upstream).await?;

        // Construct the reader of batch data (DML from users). We must create a variable to hold
        // this `Arc<TableDmlHandle>` here, or it will be dropped due to the `Weak` reference in
        // `DmlManager`.
        //
        // Note(bugen): Only register after the first barrier message is received, which means the
        // current executor is activated. This avoids the new reader overwriting the old one during
        // the preparation of schema change.
        let batch_reader = self
            .dml_manager
            .register_reader(self.table_id, self.table_version_id, &self.column_descs)
            .map_err(StreamExecutorError::connector_error)?;
        let batch_reader = batch_reader
            .stream_reader()
            .into_stream_with_rate_limit(self.rate_limit);

        // Merge the two streams using `StreamReaderWithPause` because when we receive a pause
        // barrier, we should stop receiving the data from DML. We poll data from the two streams in
        // a round robin way.
        //
        // We bias the upstream side,
        // because barrier should always take precedence,
        // to avoid high barrier latency.
        let mut stream = StreamReaderWithPause::<true, TxnMsg>::new(upstream, batch_reader);

        // If the first barrier requires us to pause on startup, pause the stream.
        if barrier.is_pause_on_startup() {
            stream.pause_stream();
        }

        yield Message::Barrier(barrier);

        // Active transactions: txn_id -> TxnBuffer with transaction chunks.
        let mut active_txn_map: BTreeMap<TxnId, TxnBuffer> = Default::default();
        // A batch group of small chunks.
        let mut batch_group: Vec<StreamChunk> = vec![];

        let mut builder = StreamChunkBuilder::new(
            self.chunk_size,
            self.column_descs
                .iter()
                .map(|c| c.data_type.clone())
                .collect(),
        );

        while let Some(input_msg) = stream.next().await {
            match input_msg? {
                Either::Left(msg) => {
                    // Stream messages.
                    if let Message::Barrier(barrier) = &msg {
                        // We should handle barrier messages here to pause or resume the data from
                        // DML.
                        if let Some(mutation) = barrier.mutation.as_deref() {
                            match mutation {
                                Mutation::Pause => stream.pause_stream(),
                                Mutation::Resume => stream.resume_stream(),
                                _ => {}
                            }
                        }

                        // Flush the remaining batch group
                        if !batch_group.is_empty() {
                            let vec = mem::take(&mut batch_group);
                            for chunk in vec {
                                for (op, row) in chunk.rows() {
                                    if let Some(chunk) = builder.append_row(op, row) {
                                        yield Message::Chunk(chunk);
                                    }
                                }
                            }
                            if let Some(chunk) = builder.take() {
                                yield Message::Chunk(chunk);
                            }
                        }
                    }
                    yield msg;
                }
                Either::Right(txn_msg) => {
                    // Batch data.
                    match txn_msg {
                        TxnMsg::Begin(txn_id) => {
                            active_txn_map
                                .try_insert(txn_id, TxnBuffer::default())
                                .unwrap_or_else(|_| {
                                    panic!("Transaction id collision txn_id = {}.", txn_id)
                                });
                        }
                        TxnMsg::End(txn_id) => {
                            let mut txn_buffer = active_txn_map.remove(&txn_id)
                                .unwrap_or_else(|| panic!("Receive an unexpected transaction end message. Active transaction map doesn't contain this transaction txn_id = {}.", txn_id));

                            let txn_buffer_cardinality = txn_buffer
                                .vec
                                .iter()
                                .map(|c| c.cardinality())
                                .sum::<usize>();
                            let batch_group_cardinality =
                                batch_group.iter().map(|c| c.cardinality()).sum::<usize>();

                            if txn_buffer_cardinality >= self.chunk_size {
                                // txn buffer is too large, so yield batch group first to preserve the transaction order in the same session.
                                if !batch_group.is_empty() {
                                    let vec = mem::take(&mut batch_group);
                                    for chunk in vec {
                                        for (op, row) in chunk.rows() {
                                            if let Some(chunk) = builder.append_row(op, row) {
                                                yield Message::Chunk(chunk);
                                            }
                                        }
                                    }
                                    if let Some(chunk) = builder.take() {
                                        yield Message::Chunk(chunk);
                                    }
                                }

                                // txn buffer isn't small, so yield.
                                for chunk in txn_buffer.vec {
                                    yield Message::Chunk(chunk);
                                }
                            } else if txn_buffer_cardinality + batch_group_cardinality
                                <= self.chunk_size
                            {
                                // txn buffer is small and batch group has space.
                                batch_group.extend(txn_buffer.vec);
                            } else {
                                // txn buffer is small and batch group has no space, so yield the batch group first to preserve the transaction order in the same session.
                                if !batch_group.is_empty() {
                                    let vec = mem::take(&mut batch_group);
                                    for chunk in vec {
                                        for (op, row) in chunk.rows() {
                                            if let Some(chunk) = builder.append_row(op, row) {
                                                yield Message::Chunk(chunk);
                                            }
                                        }
                                    }
                                    if let Some(chunk) = builder.take() {
                                        yield Message::Chunk(chunk);
                                    }
                                }

                                // put txn buffer into the batch group
                                mem::swap(&mut txn_buffer.vec, &mut batch_group);
                            }
                        }
                        TxnMsg::Rollback(txn_id) => {
                            let txn_buffer = active_txn_map.remove(&txn_id)
                                .unwrap_or_else(|| panic!("Receive an unexpected transaction rollback message. Active transaction map doesn't contain this transaction txn_id = {}.", txn_id));
                            if txn_buffer.overflow {
                                tracing::warn!("txn_id={} large transaction tries to rollback, but part of its data has already been sent to the downstream.", txn_id);
                            }
                        }
                        TxnMsg::Data(txn_id, chunk) => {
                            match active_txn_map.get_mut(&txn_id) {
                                Some(txn_buffer) => {
                                    // This transaction is too large, we can't provide atomicity,
                                    // so yield chunk ASAP.
                                    if txn_buffer.overflow {
                                        yield Message::Chunk(chunk);
                                        continue;
                                    }
                                    txn_buffer.vec.push(chunk);
                                    if txn_buffer.vec.len() > MAX_CHUNK_FOR_ATOMICITY {
                                        // Too many chunks for atomicity. Drain and yield them.
                                        tracing::warn!("txn_id={} Too many chunks for atomicity. Sent them to the downstream anyway.", txn_id);
                                        for chunk in txn_buffer.vec.drain(..) {
                                            yield Message::Chunk(chunk);
                                        }
                                        txn_buffer.overflow = true;
                                    }
                                }
                                None => panic!("Receive an unexpected transaction data message. Active transaction map doesn't contain this transaction txn_id = {}.", txn_id),
                            };
                        }
                    }
                }
            }
        }
    }
}

impl Executor for DmlExecutor {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.execute_inner().boxed()
    }

    fn schema(&self) -> &Schema {
        &self.info.schema
    }

    fn pk_indices(&self) -> PkIndicesRef<'_> {
        &self.info.pk_indices
    }

    fn identity(&self) -> &str {
        &self.info.identity
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use risingwave_common::array::StreamChunk;
    use risingwave_common::catalog::{ColumnId, Field, INITIAL_TABLE_VERSION_ID};
    use risingwave_common::test_prelude::StreamChunkTestExt;
    use risingwave_common::transaction::transaction_id::TxnId;
    use risingwave_common::types::DataType;
    use risingwave_source::dml_manager::DmlManager;

    use super::*;
    use crate::executor::test_utils::MockSource;

    const TEST_TRANSACTION_ID: TxnId = 0;
    const TEST_SESSION_ID: u32 = 0;

    #[tokio::test]
    async fn test_dml_executor() {
        let table_id = TableId::default();
        let schema = Schema::new(vec![
            Field::unnamed(DataType::Int64),
            Field::unnamed(DataType::Int64),
        ]);
        let column_descs = vec![
            ColumnDesc::unnamed(ColumnId::new(0), DataType::Int64),
            ColumnDesc::unnamed(ColumnId::new(1), DataType::Int64),
        ];
        let pk_indices = vec![0];
        let dml_manager = Arc::new(DmlManager::for_test());

        let (mut tx, source) = MockSource::channel(schema.clone(), pk_indices.clone());
        let info = ExecutorInfo {
            schema,
            pk_indices,
            identity: "DmlExecutor".to_string(),
        };

        let dml_executor = Box::new(DmlExecutor::new(
            info,
            Box::new(source),
            dml_manager.clone(),
            table_id,
            INITIAL_TABLE_VERSION_ID,
            column_descs,
            1024,
            None,
        ));
        let mut dml_executor = dml_executor.execute();

        let stream_chunk1 = StreamChunk::from_pretty(
            " I I
            + 1 1
            + 2 2
            + 3 6",
        );
        let stream_chunk2 = StreamChunk::from_pretty(
            " I I
            + 88 43",
        );
        let stream_chunk3 = StreamChunk::from_pretty(
            " I I
            + 199 40
            + 978 72
            + 134 41
            + 398 98",
        );
        let batch_chunk = StreamChunk::from_pretty(
            "  I I
            U+ 1 11
            U+ 2 22",
        );

        // The first barrier
        tx.push_barrier(1, false);
        let msg = dml_executor.next().await.unwrap().unwrap();
        assert!(matches!(msg, Message::Barrier(_)));

        // Messages from upstream streaming executor
        tx.push_chunk(stream_chunk1);
        tx.push_chunk(stream_chunk2);
        tx.push_chunk(stream_chunk3);

        let table_dml_handle = dml_manager
            .table_dml_handle(table_id, INITIAL_TABLE_VERSION_ID)
            .unwrap();
        let mut write_handle = table_dml_handle
            .write_handle(TEST_SESSION_ID, TEST_TRANSACTION_ID)
            .unwrap();

        // Message from batch
        write_handle.begin().unwrap();
        write_handle.write_chunk(batch_chunk).await.unwrap();
        // Since the end will wait the notifier which is sent by the reader,
        // we need to spawn a task here to avoid dead lock.
        tokio::spawn(async move {
            write_handle.end().await.unwrap();
            // a barrier to trigger batch group flush
            tx.push_barrier(2, false);
        });

        // Consume the 1st message from upstream executor
        let msg = dml_executor.next().await.unwrap().unwrap();
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 1 1
                + 2 2
                + 3 6",
            )
        );

        // Consume the message from batch (because dml executor selects from the streams in a round
        // robin way)

        // TxnMsg::Begin is consumed implicitly

        // Consume the 2nd message from upstream executor
        let msg = dml_executor.next().await.unwrap().unwrap();
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 88 43",
            )
        );

        // TxnMsg::Data is buffed

        // Consume the 3rd message from upstream executor
        let msg = dml_executor.next().await.unwrap().unwrap();
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 199 40
                + 978 72
                + 134 41
                + 398 98",
            )
        );

        // After TxnMsg::End, we can consume dml data
        let msg = dml_executor.next().await.unwrap().unwrap();
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I
                U+ 1 11
                U+ 2 22",
            )
        );

        let msg = dml_executor.next().await.unwrap().unwrap();
        assert!(matches!(msg, Message::Barrier(_)));
    }
}
