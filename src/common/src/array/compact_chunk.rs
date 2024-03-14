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
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::mem;

use itertools::Itertools;
use prehash::{new_prehashed_map_with_capacity, Passthru, Prehashed};

use super::stream_chunk::{OpRowMutRef, StreamChunkMut};
use super::stream_chunk_builder::StreamChunkBuilder;
use super::stream_record::Record;
use super::DataType;
use crate::array::{Op, RowRef, StreamChunk};
use crate::row::{Project, RowExt};
use crate::util::hash_util::Crc32FastBuilder;

/// Compact the stream chunks with just modify the `Ops` and visibility of the chunk. Currently, two
/// transformation will be applied
/// - remove intermediate operation of the same key. The operations of the same stream key will only
///   have three kind of patterns Insert, Delete or Update.
/// - For the update (-old row, +old row), when old row is exactly same. The two rowOp will be
///   removed.
pub struct StreamChunkCompactor {
    chunks: Vec<StreamChunk>,
    stream_key: Vec<usize>,
}

struct OpRowMutRefTuple<'a> {
    previous: Option<OpRowMutRef<'a>>,
    latest: OpRowMutRef<'a>,
}

impl<'a> OpRowMutRefTuple<'a> {
    /// return true if no row left
    fn push(&mut self, mut op_row: OpRowMutRef<'a>) -> bool {
        debug_assert!(self.latest.vis());
        match (self.latest.op(), op_row.op()) {
            (Op::Insert, Op::Insert) => panic!("receive duplicated insert on the stream"),
            (Op::Delete, Op::Delete) => panic!("receive duplicated delete on the stream"),
            (Op::Insert, Op::Delete) => {
                self.latest.set_vis(false);
                op_row.set_vis(false);
                self.latest = if let Some(prev) = self.previous.take() {
                    prev
                } else {
                    return true;
                }
            }
            (Op::Delete, Op::Insert) => {
                // The operation for the key must be (+, -, +) or (-, +). And the (+, -) must has
                // been filtered.
                debug_assert!(self.previous.is_none());
                self.previous = Some(mem::replace(&mut self.latest, op_row));
            }
            // `all the updateDelete` and `updateInsert` should be normalized to `delete`
            // and`insert`
            _ => unreachable!(),
        };
        false
    }

    fn as_update_op(&mut self) -> Option<(&mut OpRowMutRef<'a>, &mut OpRowMutRef<'a>)> {
        self.previous.as_mut().map(|prev| {
            debug_assert_eq!(prev.op(), Op::Delete);
            debug_assert_eq!(self.latest.op(), Op::Insert);
            (prev, &mut self.latest)
        })
    }
}

type OpRowMap<'a, 'b> =
    HashMap<Prehashed<Project<'b, RowRef<'a>>>, OpRowMutRefTuple<'a>, BuildHasherDefault<Passthru>>;

#[derive(Clone, Debug)]
pub enum RowOp<'a> {
    Insert(RowRef<'a>),
    Delete(RowRef<'a>),
    /// (old_value, new_value)
    Update((RowRef<'a>, RowRef<'a>)),
}

pub struct RowOpMap<'a, 'b> {
    map: HashMap<Prehashed<Project<'b, RowRef<'a>>>, RowOp<'a>, BuildHasherDefault<Passthru>>,
}

impl<'a, 'b> RowOpMap<'a, 'b> {
    fn with_capacity(estimate_size: usize) -> Self {
        Self {
            map: new_prehashed_map_with_capacity(estimate_size),
        }
    }

    pub fn insert(&mut self, k: Prehashed<Project<'b, RowRef<'a>>>, v: RowRef<'a>) {
        let entry = self.map.entry(k);
        match entry {
            Entry::Vacant(e) => {
                e.insert(RowOp::Insert(v));
            }
            Entry::Occupied(mut e) => match e.get() {
                RowOp::Delete(ref old_v) => {
                    e.insert(RowOp::Update((*old_v, v)));
                }
                RowOp::Insert(_) => {
                    tracing::warn!("double insert for the same pk");
                    e.insert(RowOp::Insert(v));
                }
                RowOp::Update((ref old_v, _)) => {
                    tracing::warn!("double insert for the same pk");
                    e.insert(RowOp::Update((*old_v, v)));
                }
            },
        }
    }

    pub fn delete(&mut self, k: Prehashed<Project<'b, RowRef<'a>>>, v: RowRef<'a>) {
        let entry = self.map.entry(k);
        match entry {
            Entry::Vacant(e) => {
                e.insert(RowOp::Delete(v));
            }
            Entry::Occupied(mut e) => match e.get() {
                RowOp::Insert(_) => {
                    e.remove();
                }
                RowOp::Update((ref prev, _)) => {
                    e.insert(RowOp::Delete(*prev));
                }
                RowOp::Delete(_) => {
                    tracing::warn!("double delete for the same pk");
                    e.insert(RowOp::Delete(v));
                }
            },
        }
    }

    pub fn into_chunks(self, chunk_size: usize, data_types: Vec<DataType>) -> Vec<StreamChunk> {
        let mut ret = vec![];
        let mut builder = StreamChunkBuilder::new(chunk_size, data_types);
        for (_, row_op) in self.map {
            match row_op {
                RowOp::Insert(row) => {
                    if let Some(c) = builder.append_record(Record::Insert { new_row: row }) {
                        ret.push(c)
                    }
                }
                RowOp::Delete(row) => {
                    if let Some(c) = builder.append_record(Record::Delete { old_row: row }) {
                        ret.push(c)
                    }
                }
                RowOp::Update((old, new)) => {
                    if old == new {
                        continue;
                    }
                    if let Some(c) = builder.append_record(Record::Update {
                        old_row: old,
                        new_row: new,
                    }) {
                        ret.push(c)
                    }
                }
            }
        }
        if let Some(c) = builder.take() {
            ret.push(c);
        }
        ret
    }
}

impl StreamChunkCompactor {
    pub fn new(stream_key: Vec<usize>, chunks: Vec<StreamChunk>) -> Self {
        Self { chunks, stream_key }
    }

    pub fn into_inner(self) -> (Vec<StreamChunk>, Vec<usize>) {
        (self.chunks, self.stream_key)
    }

    /// Compact a chunk by modifying the ops and the visibility of a stream chunk. All UPDATE INSERT
    /// and UPDATE DELETE will be converted to INSERT and DELETE, and dropped according to
    /// certain rules (see `merge_insert` and `merge_delete` for more details).
    pub fn into_compacted_chunks(self) -> impl Iterator<Item = StreamChunk> {
        let (chunks, key_indices) = self.into_inner();

        let estimate_size = chunks.iter().map(|c| c.cardinality()).sum();
        let mut chunks: Vec<(Vec<u64>, StreamChunkMut)> = chunks
            .into_iter()
            .map(|c| {
                let hash_values = c
                    .data_chunk()
                    .get_hash_values(&key_indices, Crc32FastBuilder)
                    .into_iter()
                    .map(|hash| hash.value())
                    .collect_vec();
                (hash_values, StreamChunkMut::from(c))
            })
            .collect_vec();

        let mut op_row_map: OpRowMap<'_, '_> = new_prehashed_map_with_capacity(estimate_size);
        for (hash_values, c) in &mut chunks {
            for (row, mut op_row) in c.to_rows_mut() {
                op_row.set_op(op_row.op().normalize_update());
                let hash = hash_values[row.index()];
                let stream_key = row.project(&key_indices);
                match op_row_map.entry(Prehashed::new(stream_key, hash)) {
                    Entry::Vacant(v) => {
                        v.insert(OpRowMutRefTuple {
                            previous: None,
                            latest: op_row,
                        });
                    }
                    Entry::Occupied(mut o) => {
                        if o.get_mut().push(op_row) {
                            o.remove_entry();
                        }
                    }
                }
            }
        }
        for tuple in op_row_map.values_mut() {
            if let Some((prev, latest)) = tuple.as_update_op() {
                if prev.row_ref() == latest.row_ref() {
                    prev.set_vis(false);
                    latest.set_vis(false);
                } else if prev.same_chunk(latest) && prev.index() + 1 == latest.index() {
                    // TODO(st1page): use next_one check in bitmap
                    prev.set_op(Op::UpdateDelete);
                    latest.set_op(Op::UpdateInsert);
                }
            }
        }
        chunks.into_iter().map(|(_, c)| c.into())
    }

    /// re-construct the stream chunks to compact them with the stream key.
    pub fn reconstructed_compacted_chunks(
        self,
        chunk_size: usize,
        data_types: Vec<DataType>,
    ) -> Vec<StreamChunk> {
        let (chunks, key_indices) = self.into_inner();

        let estimate_size = chunks.iter().map(|c| c.cardinality()).sum();
        let chunks: Vec<(_, _, _)> = chunks
            .into_iter()
            .map(|c| {
                let (c, ops) = c.into_parts();
                let hash_values = c
                    .get_hash_values(&key_indices, Crc32FastBuilder)
                    .into_iter()
                    .map(|hash| hash.value())
                    .collect_vec();
                (hash_values, ops, c)
            })
            .collect_vec();
        let mut map = RowOpMap::with_capacity(estimate_size);
        for (hash_values, ops, c) in &chunks {
            for row in c.rows() {
                let hash = hash_values[row.index()];
                let op = ops[row.index()];
                let stream_key = row.project(&key_indices);
                let k = Prehashed::new(stream_key, hash);
                match op {
                    Op::Insert | Op::UpdateInsert => map.insert(k, row),
                    Op::Delete | Op::UpdateDelete => map.delete(k, row),
                }
            }
        }
        map.into_chunks(chunk_size, data_types)
    }
}

pub fn merge_chunk_row(stream_chunk: StreamChunk, pk_indices: &[usize]) -> StreamChunk {
    let compactor = StreamChunkCompactor::new(pk_indices.to_vec(), vec![stream_chunk]);
    compactor.into_compacted_chunks().next().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::StreamChunk;
    use crate::test_prelude::StreamChunkTestExt;

    #[test]
    fn test_merge_chunk_row() {
        let pk_indices = [0, 1];
        let chunks = vec![
            StreamChunk::from_pretty(
                " I I I
                - 1 1 1
                + 1 1 2
                + 2 5 7
                + 4 9 2
                - 2 5 7
                + 2 5 5
                - 6 6 9
                + 6 6 9
                - 9 9 1",
            ),
            StreamChunk::from_pretty(
                " I I I
                - 6 6 9
                + 9 9 9
                - 9 9 4
                + 2 2 2
                + 9 9 1",
            ),
        ];
        let compactor = StreamChunkCompactor::new(pk_indices.to_vec(), chunks);
        let mut iter = compactor.into_compacted_chunks();
        assert_eq!(
            iter.next().unwrap().compact(),
            StreamChunk::from_pretty(
                " I I I
                U- 1 1 1
                U+ 1 1 2
                + 4 9 2
                + 2 5 5
                - 6 6 9",
            )
        );
        assert_eq!(
            iter.next().unwrap().compact(),
            StreamChunk::from_pretty(
                " I I I
                + 2 2 2",
            )
        );

        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_compact_chunk_row() {
        let pk_indices = [0, 1];
        let chunks = vec![
            StreamChunk::from_pretty(
                " I I I
            - 1 1 1
            + 1 1 2
            + 2 5 7
            + 4 9 2
            - 2 5 7
            + 2 5 5
            - 6 6 9
            + 6 6 9
            - 9 9 1",
            ),
            StreamChunk::from_pretty(
                " I I I
            - 6 6 9
            + 9 9 9
            - 9 9 4
            + 2 2 2
            + 9 9 1",
            ),
        ];
        let compactor = StreamChunkCompactor::new(pk_indices.to_vec(), chunks);

        let chunks = compactor.reconstructed_compacted_chunks(
            100,
            vec![DataType::Int64, DataType::Int64, DataType::Int64],
        );
        assert_eq!(
            chunks.into_iter().next().unwrap(),
            StreamChunk::from_pretty(
                " I I I
                 + 2 5 5
                 - 6 6 9
                 + 4 9 2
                U- 1 1 1
                U+ 1 1 2
                 + 2 2 2",
            )
        );
    }
}
