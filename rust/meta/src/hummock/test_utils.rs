// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use itertools::Itertools;
use risingwave_pb::common::{HostAddress, WorkerNode, WorkerType};
use risingwave_pb::hummock::{HummockVersion, KeyRange, SstableInfo, SstableMeta};
use risingwave_storage::hummock::key::key_with_epoch;
use risingwave_storage::hummock::value::HummockValue;
use risingwave_storage::hummock::{
    CompressionAlgorithm, HummockContextId, HummockEpoch, HummockSSTableId, SSTableBuilder,
    SSTableBuilderOptions,
};

use crate::cluster::{ClusterManager, ClusterManagerRef};
use crate::hummock::{HummockManager, HummockManagerRef};
use crate::manager::MetaSrvEnv;
use crate::rpc::metrics::MetaMetrics;
use crate::storage::{MemStore, MetaStore};

pub async fn add_test_tables<S>(
    hummock_manager: &HummockManager<S>,
    context_id: HummockContextId,
) -> Vec<Vec<SstableInfo>>
where
    S: MetaStore,
{
    // Increase version by 2.
    let mut epoch: u64 = 1;
    let table_ids = vec![
        hummock_manager.get_new_table_id().await.unwrap(),
        hummock_manager.get_new_table_id().await.unwrap(),
        hummock_manager.get_new_table_id().await.unwrap(),
    ];
    let (test_tables, _) = generate_test_tables(epoch, table_ids);
    hummock_manager
        .add_tables(context_id, test_tables.clone(), epoch)
        .await
        .unwrap();
    hummock_manager.commit_epoch(epoch).await.unwrap();
    // Current state: {v0: [], v1: [test_tables uncommitted], v2: [test_tables]}

    // Simulate a compaction and increase version by 1.
    let mut compact_task = hummock_manager
        .get_compact_task(context_id)
        .await
        .unwrap()
        .unwrap();
    let (test_tables_2, _) = generate_test_tables(
        epoch,
        vec![hummock_manager.get_new_table_id().await.unwrap()],
    );
    compact_task.sorted_output_ssts = test_tables_2.clone();
    compact_task.task_status = true;
    hummock_manager
        .report_compact_task(compact_task)
        .await
        .unwrap();
    // Current state: {v0: [], v1: [test_tables uncommitted], v2: [test_tables], v3: [test_tables_2,
    // test_tables to_delete]}

    // Increase version by 1.
    epoch += 1;
    let (test_tables_3, _) = generate_test_tables(
        epoch,
        vec![hummock_manager.get_new_table_id().await.unwrap()],
    );
    hummock_manager
        .add_tables(context_id, test_tables_3.clone(), epoch)
        .await
        .unwrap();
    // Current state: {v0: [], v1: [test_tables uncommitted], v2: [test_tables], v3: [test_tables_2,
    // to_delete:test_tables], v4: [test_tables_2, test_tables_3 uncommitted]}
    vec![test_tables, test_tables_2, test_tables_3]
}

pub fn generate_test_tables(
    epoch: u64,
    table_ids: Vec<u64>,
) -> (Vec<SstableInfo>, Vec<(SstableMeta, Bytes)>) {
    // Tables to add
    let opt = SSTableBuilderOptions {
        capacity: 64 * 1024 * 1024,
        block_capacity: 4096,
        restart_interval: 16,
        bloom_false_positive: 0.1,
        compression_algorithm: CompressionAlgorithm::None,
    };

    let mut sst_info = vec![];
    let mut sst_data = vec![];
    for (i, table_id) in table_ids.into_iter().enumerate() {
        let mut b = SSTableBuilder::new(opt.clone());
        let kv_pairs = vec![
            (i + 1, HummockValue::Put(b"test".as_slice())),
            ((i + 1) * 10, HummockValue::Put(b"test".as_slice())),
        ];
        for kv in kv_pairs {
            b.add(&iterator_test_key_of_epoch(table_id, kv.0, epoch), kv.1);
        }
        let (data, meta) = b.finish();
        sst_data.push((meta.clone(), data));
        sst_info.push(SstableInfo {
            id: table_id,
            key_range: Some(KeyRange {
                left: meta.smallest_key,
                right: meta.largest_key,
                inf: false,
            }),
        });
    }
    (sst_info, sst_data)
}

/// Generate keys like `001_key_test_00002` with timestamp `epoch`.
pub fn iterator_test_key_of_epoch(table: u64, idx: usize, ts: HummockEpoch) -> Vec<u8> {
    // key format: {prefix_index}_version
    key_with_epoch(
        format!("{:03}_key_test_{:05}", table, idx)
            .as_bytes()
            .to_vec(),
        ts,
    )
}

pub fn get_sorted_sstable_ids(sstables: &[SstableInfo]) -> Vec<HummockSSTableId> {
    sstables.iter().map(|table| table.id).sorted().collect_vec()
}

pub fn get_sorted_committed_sstable_ids(hummock_version: &HummockVersion) -> Vec<HummockSSTableId> {
    hummock_version
        .levels
        .iter()
        .flat_map(|level| level.table_ids.clone())
        .sorted()
        .collect_vec()
}

pub async fn setup_compute_env(
    port: i32,
) -> (
    MetaSrvEnv<MemStore>,
    HummockManagerRef<MemStore>,
    ClusterManagerRef<MemStore>,
    WorkerNode,
) {
    let env = MetaSrvEnv::for_test().await;
    let cluster_manager = Arc::new(
        ClusterManager::new(env.clone(), Duration::from_secs(1))
            .await
            .unwrap(),
    );
    let hummock_manager = Arc::new(
        HummockManager::new(
            env.clone(),
            cluster_manager.clone(),
            Arc::new(MetaMetrics::new()),
        )
        .await
        .unwrap(),
    );
    let fake_host_address = HostAddress {
        host: "127.0.0.1".to_string(),
        port,
    };
    let (worker_node, _) = cluster_manager
        .add_worker_node(fake_host_address, WorkerType::ComputeNode)
        .await
        .unwrap();
    (env, hummock_manager, cluster_manager, worker_node)
}
