// Copyright 2023 RisingWave Labs
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use fixedbitset::FixedBitSet;
use itertools::{Either, Itertools};
use pretty_xmlish::{Pretty, StrAssocArr};
use risingwave_common::catalog::{Field, FieldDisplay, Schema};
use risingwave_common::types::DataType;
use risingwave_common::util::sort_util::{ColumnOrder, ColumnOrderDisplay, OrderType};
use risingwave_expr::agg::AggKind;
use risingwave_pb::expr::PbAggCall;
use risingwave_pb::stream_plan::{agg_call_state, AggCallState as AggCallStatePb};

use super::super::utils::TableCatalogBuilder;
use super::{impl_distill_unit_from_fields, stream, GenericPlanNode, GenericPlanRef};
use crate::expr::{Expr, ExprRewriter, InputRef, InputRefDisplay};
use crate::optimizer::optimizer_context::OptimizerContextRef;
use crate::optimizer::plan_node::batch::BatchPlanRef;
use crate::optimizer::property::{Distribution, FunctionalDependencySet, RequiredDist};
use crate::stream_fragmenter::BuildFragmentGraphState;
use crate::utils::{
    ColIndexMapping, ColIndexMappingRewriteExt, Condition, ConditionDisplay, IndexRewriter,
};
use crate::TableCatalog;

/// [`Agg`] groups input data by their group key and computes aggregation functions.
///
/// It corresponds to the `GROUP BY` operator in a SQL query statement together with the aggregate
/// functions in the `SELECT` clause.
///
/// The output schema will first include the group key and then the aggregation calls.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Agg<PlanRef> {
    pub agg_calls: Vec<PlanAggCall>,
    pub group_key: FixedBitSet,
    pub input: PlanRef,
}

impl<PlanRef: GenericPlanRef> Agg<PlanRef> {
    pub(crate) fn rewrite_exprs(&mut self, r: &mut dyn ExprRewriter) {
        self.agg_calls.iter_mut().for_each(|call| {
            call.filter = call.filter.clone().rewrite_expr(r);
        });
    }

    pub(crate) fn output_len(&self) -> usize {
        self.group_key.count_ones(..) + self.agg_calls.len()
    }

    /// get the Mapping of columnIndex from input column index to output column index,if a input
    /// column corresponds more than one out columns, mapping to any one
    pub fn o2i_col_mapping(&self) -> ColIndexMapping {
        let mut map = vec![None; self.output_len()];
        for (i, key) in self.group_key.ones().enumerate() {
            map[i] = Some(key);
        }
        ColIndexMapping::with_target_size(map, self.input.schema().len())
    }

    /// get the Mapping of columnIndex from input column index to out column index
    pub fn i2o_col_mapping(&self) -> ColIndexMapping {
        let mut map = vec![None; self.input.schema().len()];
        for (i, key) in self.group_key.ones().enumerate() {
            map[key] = Some(i);
        }
        ColIndexMapping::with_target_size(map, self.output_len())
    }

    pub(crate) fn can_two_phase_agg(&self) -> bool {
        self.call_support_two_phase()
            && !self.is_agg_result_affected_by_order()
            && self.two_phase_agg_enabled()
    }

    /// Must try two phase agg iff we are forced to, and we satisfy the constraints.
    pub(crate) fn must_try_two_phase_agg(&self) -> bool {
        self.two_phase_agg_forced() && self.can_two_phase_agg()
    }

    fn two_phase_agg_forced(&self) -> bool {
        self.ctx().session_ctx().config().get_force_two_phase_agg()
    }

    fn two_phase_agg_enabled(&self) -> bool {
        self.ctx().session_ctx().config().get_enable_two_phase_agg()
    }

    /// Generally used by two phase hash agg.
    /// If input dist already satisfies hash agg distribution,
    /// it will be more expensive to do two phase agg, should just do shuffle agg.
    pub(crate) fn hash_agg_dist_satisfied_by_input_dist(&self, input_dist: &Distribution) -> bool {
        let required_dist = RequiredDist::shard_by_key(
            self.input.schema().len(),
            &self.group_key.ones().collect_vec(),
        );
        input_dist.satisfies(&required_dist)
    }

    fn call_support_two_phase(&self) -> bool {
        !self.agg_calls.is_empty()
            && self.agg_calls.iter().all(|call| {
                matches!(
                    call.agg_kind,
                    AggKind::Min | AggKind::Max | AggKind::Sum | AggKind::Count
                ) && !call.distinct
            })
    }

    /// Check if the aggregation result will be affected by order by clause, if any.
    pub(crate) fn is_agg_result_affected_by_order(&self) -> bool {
        self.agg_calls
            .iter()
            .any(|call| matches!(call.agg_kind, AggKind::StringAgg | AggKind::ArrayAgg))
    }

    pub(crate) fn watermark_group_key(&self, input_watermark_columns: &FixedBitSet) -> Vec<usize> {
        self.group_key
            .ones()
            .filter(|&idx| input_watermark_columns.contains(idx))
            .collect()
    }

    pub fn new(agg_calls: Vec<PlanAggCall>, group_key: FixedBitSet, input: PlanRef) -> Self {
        Self {
            agg_calls,
            group_key,
            input,
        }
    }
}

impl<PlanRef: BatchPlanRef> Agg<PlanRef> {
    // Check if the input is already sorted on group keys.
    pub(crate) fn input_provides_order_on_group_keys(&self) -> bool {
        self.group_key.ones().all(|group_by_idx| {
            self.input
                .order()
                .column_orders
                .iter()
                .any(|order| order.column_index == group_by_idx)
        })
    }
}

impl<PlanRef: GenericPlanRef> GenericPlanNode for Agg<PlanRef> {
    fn schema(&self) -> Schema {
        let fields = self
            .group_key
            .ones()
            .map(|i| self.input.schema().fields()[i].clone())
            .chain(self.agg_calls.iter().map(|agg_call| {
                let plan_agg_call_display = PlanAggCallDisplay {
                    plan_agg_call: agg_call,
                    input_schema: self.input.schema(),
                };
                let name = format!("{:?}", plan_agg_call_display);
                Field::with_name(agg_call.return_type.clone(), name)
            }))
            .collect();
        Schema { fields }
    }

    fn logical_pk(&self) -> Option<Vec<usize>> {
        Some((0..self.group_key.count_ones(..)).collect())
    }

    fn ctx(&self) -> OptimizerContextRef {
        self.input.ctx()
    }

    fn functional_dependency(&self) -> FunctionalDependencySet {
        let output_len = self.output_len();
        let _input_len = self.input.schema().len();
        let mut fd_set = FunctionalDependencySet::with_key(
            output_len,
            &(0..self.group_key.count_ones(..)).collect_vec(),
        );
        // take group keys from input_columns, then grow the target size to column_cnt
        let i2o = self.i2o_col_mapping();
        for fd in self.input.functional_dependency().as_dependencies() {
            if let Some(fd) = i2o.rewrite_functional_dependency(fd) {
                fd_set.add_functional_dependency(fd);
            }
        }
        fd_set
    }
}

pub enum AggCallState {
    ResultValue,
    Table(Box<TableState>),
    MaterializedInput(Box<MaterializedInputState>),
}

impl AggCallState {
    pub fn into_prost(self, state: &mut BuildFragmentGraphState) -> AggCallStatePb {
        AggCallStatePb {
            inner: Some(match self {
                AggCallState::ResultValue => {
                    agg_call_state::Inner::ResultValueState(agg_call_state::ResultValueState {})
                }
                AggCallState::Table(s) => {
                    agg_call_state::Inner::TableState(agg_call_state::TableState {
                        table: Some(
                            s.table
                                .with_id(state.gen_table_id_wrapped())
                                .to_internal_table_prost(),
                        ),
                    })
                }
                AggCallState::MaterializedInput(s) => {
                    agg_call_state::Inner::MaterializedInputState(
                        agg_call_state::MaterializedInputState {
                            table: Some(
                                s.table
                                    .with_id(state.gen_table_id_wrapped())
                                    .to_internal_table_prost(),
                            ),
                            included_upstream_indices: s
                                .included_upstream_indices
                                .into_iter()
                                .map(|x| x as _)
                                .collect(),
                            table_value_indices: s
                                .table_value_indices
                                .into_iter()
                                .map(|x| x as _)
                                .collect(),
                        },
                    )
                }
            }),
        }
    }
}

pub struct TableState {
    pub table: TableCatalog,
}

pub struct MaterializedInputState {
    pub table: TableCatalog,
    pub included_upstream_indices: Vec<usize>,
    pub table_value_indices: Vec<usize>,
}

impl<PlanRef: stream::StreamPlanRef> Agg<PlanRef> {
    pub fn infer_tables(
        &self,
        me: &impl stream::StreamPlanRef,
        vnode_col_idx: Option<usize>,
        window_col_idx: Option<usize>,
    ) -> (
        TableCatalog,
        Vec<AggCallState>,
        HashMap<usize, TableCatalog>,
    ) {
        (
            self.infer_result_table(me, vnode_col_idx, window_col_idx),
            self.infer_stream_agg_state(me, vnode_col_idx, window_col_idx),
            self.infer_distinct_dedup_tables(me, vnode_col_idx, window_col_idx),
        )
    }

    fn get_ordered_group_key(&self, window_col_idx: Option<usize>) -> Vec<usize> {
        if let Some(window_col_idx) = window_col_idx {
            assert!(self.group_key.contains(window_col_idx));
            Either::Left(
                std::iter::once(window_col_idx)
                    .chain(self.group_key.ones().filter(move |&i| i != window_col_idx)),
            )
        } else {
            Either::Right(self.group_key.ones())
        }
        .collect()
    }

    /// Create a new table builder with group key columns added.
    ///
    /// # Returns
    ///
    /// - table builder with group key columns added
    /// - included upstream indices
    /// - column mapping from upstream to table
    fn create_table_builder(
        &self,
        ctx: OptimizerContextRef,
        window_col_idx: Option<usize>,
    ) -> (TableCatalogBuilder, Vec<usize>, BTreeMap<usize, usize>) {
        // NOTE: this function should be called to get a table builder, so that all state tables
        // created for Agg node have the same group key columns and pk ordering.
        let mut table_builder =
            TableCatalogBuilder::new(ctx.with_options().internal_table_subset());

        assert!(table_builder.columns().is_empty());
        assert_eq!(table_builder.get_current_pk_len(), 0);

        // add group key column to table builder
        let mut included_upstream_indices = vec![];
        let mut column_mapping = BTreeMap::new();
        let in_fields = self.input.schema().fields();
        for idx in self.group_key.ones() {
            let tbl_col_idx = table_builder.add_column(&in_fields[idx]);
            included_upstream_indices.push(idx);
            column_mapping.insert(idx, tbl_col_idx);
        }

        // configure state table primary key (ordering)
        let ordered_group_key = self.get_ordered_group_key(window_col_idx);
        for idx in ordered_group_key {
            table_builder.add_order_column(column_mapping[&idx], OrderType::ascending());
        }

        (table_builder, included_upstream_indices, column_mapping)
    }

    /// Infer `AggCallState`s for streaming agg.
    pub fn infer_stream_agg_state(
        &self,
        me: &impl stream::StreamPlanRef,
        vnode_col_idx: Option<usize>,
        window_col_idx: Option<usize>,
    ) -> Vec<AggCallState> {
        let in_fields = self.input.schema().fields().to_vec();
        let in_pks = self.input.logical_pk().to_vec();
        let in_append_only = self.input.append_only();
        let in_dist_key = self.input.distribution().dist_column_indices().to_vec();

        let gen_materialized_input_state = |sort_keys: Vec<(OrderType, usize)>,
                                            include_keys: Vec<usize>|
         -> MaterializedInputState {
            let (mut table_builder, mut included_upstream_indices, mut column_mapping) =
                self.create_table_builder(me.ctx(), window_col_idx);
            let read_prefix_len_hint = table_builder.get_current_pk_len();

            let mut table_value_indices = BTreeSet::new(); // table column indices of value columns
            let mut add_column =
                |upstream_idx, order_type, is_value, table_builder: &mut TableCatalogBuilder| {
                    column_mapping.entry(upstream_idx).or_insert_with(|| {
                        let table_col_idx = table_builder.add_column(&in_fields[upstream_idx]);
                        if let Some(order_type) = order_type {
                            table_builder.add_order_column(table_col_idx, order_type);
                        }
                        included_upstream_indices.push(upstream_idx);
                        table_col_idx
                    });
                    if is_value {
                        // note that some indices may be added before as group keys which are not
                        // value
                        table_value_indices.insert(column_mapping[&upstream_idx]);
                    }
                };

            for (order_type, idx) in sort_keys {
                add_column(idx, Some(order_type), true, &mut table_builder);
            }
            for &idx in &in_pks {
                add_column(idx, Some(OrderType::ascending()), true, &mut table_builder);
            }
            for idx in include_keys {
                add_column(idx, None, true, &mut table_builder);
            }

            let mapping =
                ColIndexMapping::with_included_columns(&included_upstream_indices, in_fields.len());
            let tb_dist = mapping.rewrite_dist_key(&in_dist_key);
            if let Some(tb_vnode_idx) = vnode_col_idx.and_then(|idx| mapping.try_map(idx)) {
                table_builder.set_vnode_col_idx(tb_vnode_idx);
            }

            // set value indices to reduce ser/de overhead
            let table_value_indices = table_value_indices.into_iter().collect_vec();
            table_builder.set_value_indices(table_value_indices.clone());

            MaterializedInputState {
                table: table_builder.build(tb_dist.unwrap_or_default(), read_prefix_len_hint),
                included_upstream_indices,
                table_value_indices,
            }
        };

        let gen_table_state = |agg_kind: AggKind| -> TableState {
            let (mut table_builder, included_upstream_indices, _) =
                self.create_table_builder(me.ctx(), window_col_idx);
            let read_prefix_len_hint = table_builder.get_current_pk_len();

            match agg_kind {
                AggKind::ApproxCountDistinct => {
                    // Add register column.
                    table_builder.add_column(&Field {
                        data_type: DataType::List(Box::new(DataType::Int64)),
                        name: String::from("registers"),
                        sub_fields: vec![],
                        type_name: String::default(),
                    });
                }
                _ => panic!("state of agg kind `{agg_kind}` is not supposed to be `TableState`"),
            }

            let mapping =
                ColIndexMapping::with_included_columns(&included_upstream_indices, in_fields.len());
            let tb_dist = mapping.rewrite_dist_key(&in_dist_key);
            if let Some(tb_vnode_idx) = vnode_col_idx.and_then(|idx| mapping.try_map(idx)) {
                table_builder.set_vnode_col_idx(tb_vnode_idx);
            }
            TableState {
                table: table_builder.build(tb_dist.unwrap_or_default(), read_prefix_len_hint),
            }
        };

        self.agg_calls
            .iter()
            .map(|agg_call| match agg_call.agg_kind {
                AggKind::Min
                | AggKind::Max
                | AggKind::StringAgg
                | AggKind::ArrayAgg
                | AggKind::JsonbAgg
                | AggKind::JsonbObjectAgg
                | AggKind::FirstValue => {
                    if !in_append_only {
                        // columns with order requirement in state table
                        let sort_keys = {
                            match agg_call.agg_kind {
                                AggKind::Min => {
                                    vec![(OrderType::ascending(), agg_call.inputs[0].index)]
                                }
                                AggKind::Max => {
                                    vec![(OrderType::descending(), agg_call.inputs[0].index)]
                                }
                                AggKind::StringAgg
                                | AggKind::ArrayAgg
                                | AggKind::JsonbAgg
                                | AggKind::JsonbObjectAgg => agg_call
                                    .order_by
                                    .iter()
                                    .map(|o| (o.order_type, o.column_index))
                                    .collect(),
                                _ => unreachable!(),
                            }
                        };
                        // other columns that should be contained in state table
                        let include_keys = match agg_call.agg_kind {
                            AggKind::StringAgg
                            | AggKind::ArrayAgg
                            | AggKind::JsonbAgg
                            | AggKind::JsonbObjectAgg => {
                                agg_call.inputs.iter().map(|i| i.index).collect()
                            }
                            _ => vec![],
                        };
                        let state = gen_materialized_input_state(sort_keys, include_keys);
                        AggCallState::MaterializedInput(Box::new(state))
                    } else {
                        AggCallState::ResultValue
                    }
                }
                AggKind::BitXor
                | AggKind::Sum
                | AggKind::Sum0
                | AggKind::Count
                | AggKind::Avg
                | AggKind::StddevPop
                | AggKind::StddevSamp
                | AggKind::VarPop
                | AggKind::VarSamp => AggCallState::ResultValue,
                AggKind::ApproxCountDistinct => {
                    if !in_append_only {
                        // FIXME: now the approx count distinct on a non-append-only stream does not
                        // really has state and can handle failover or scale-out correctly
                        AggCallState::ResultValue
                    } else {
                        let state = gen_table_state(agg_call.agg_kind);
                        AggCallState::Table(Box::new(state))
                    }
                }
                // TODO: is its state a Table?
                AggKind::BitAnd
                | AggKind::BitOr
                | AggKind::BoolAnd
                | AggKind::BoolOr
                | AggKind::PercentileCont
                | AggKind::PercentileDisc
                | AggKind::Mode => {
                    unimplemented!()
                }
            })
            .collect()
    }

    pub fn infer_result_table(
        &self,
        me: &impl GenericPlanRef,
        vnode_col_idx: Option<usize>,
        window_col_idx: Option<usize>,
    ) -> TableCatalog {
        let out_fields = me.schema().fields();
        let in_dist_key = self.input.distribution().dist_column_indices().to_vec();
        let n_group_key_cols = self.group_key.count_ones(..);

        let (mut table_builder, _, _) = self.create_table_builder(me.ctx(), window_col_idx);
        let read_prefix_len_hint = table_builder.get_current_pk_len();

        for field in out_fields.iter().skip(n_group_key_cols) {
            table_builder.add_column(field);
        }

        let mapping = self.i2o_col_mapping();
        let tb_dist = mapping.rewrite_dist_key(&in_dist_key).unwrap_or_default();
        if let Some(tb_vnode_idx) = vnode_col_idx.and_then(|idx| mapping.try_map(idx)) {
            table_builder.set_vnode_col_idx(tb_vnode_idx);
        }

        // the result_table is composed of group_key and all agg_call's values, so the value_indices
        // of this table should skip group_key.len().
        table_builder.set_value_indices((n_group_key_cols..out_fields.len()).collect());
        table_builder.build(tb_dist, read_prefix_len_hint)
    }

    /// Infer dedup tables for distinct agg calls, partitioned by distinct columns.
    /// Since distinct agg calls only dedup on the first argument, the key of the result map is
    /// `usize`, i.e. the distinct column index.
    ///
    /// Dedup table schema:
    /// group key | distinct key | count for AGG1(distinct x) | count for AGG2(distinct x) | ...
    pub fn infer_distinct_dedup_tables(
        &self,
        me: &impl GenericPlanRef,
        vnode_col_idx: Option<usize>,
        window_col_idx: Option<usize>,
    ) -> HashMap<usize, TableCatalog> {
        let in_dist_key = self.input.distribution().dist_column_indices().to_vec();
        let in_fields = self.input.schema().fields();

        self.agg_calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.distinct) // only distinct agg calls need dedup table
            .into_group_map_by(|(_, call)| call.inputs[0].index) // one table per distinct column
            .into_iter()
            .map(|(distinct_col, indices_and_calls)| {
                let (mut table_builder, mut key_cols, _) =
                    self.create_table_builder(me.ctx(), window_col_idx);
                let table_col_idx = table_builder.add_column(&in_fields[distinct_col]);
                table_builder.add_order_column(table_col_idx, OrderType::ascending());
                key_cols.push(distinct_col);

                let read_prefix_len_hint = table_builder.get_current_pk_len();

                // Agg calls with same distinct column share the same dedup table, but they may have
                // different filter conditions, so the count of occurrence of one distinct key may
                // differ among different calls. We add one column for each call in the dedup table.
                for (call_index, _) in indices_and_calls {
                    table_builder.add_column(&Field {
                        data_type: DataType::Int64,
                        name: format!("count_for_agg_call_{}", call_index),
                        sub_fields: vec![],
                        type_name: String::default(),
                    });
                }
                table_builder
                    .set_value_indices((key_cols.len()..table_builder.columns().len()).collect());

                let mapping = ColIndexMapping::with_included_columns(&key_cols, in_fields.len());
                if let Some(idx) = vnode_col_idx.and_then(|idx| mapping.try_map(idx)) {
                    table_builder.set_vnode_col_idx(idx);
                }
                let dist_key = mapping.rewrite_dist_key(&in_dist_key).unwrap_or_default();
                let table = table_builder.build(dist_key, read_prefix_len_hint);
                (distinct_col, table)
            })
            .collect()
    }

    pub fn decompose(self) -> (Vec<PlanAggCall>, FixedBitSet, PlanRef) {
        (self.agg_calls, self.group_key, self.input)
    }

    pub fn fmt_with_name(&self, f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
        let mut builder = f.debug_struct(name);
        self.fmt_fields_with_builder(&mut builder);
        builder.finish()
    }

    pub fn fmt_fields_with_builder(&self, builder: &mut fmt::DebugStruct<'_, '_>) {
        if self.group_key.count_ones(..) != 0 {
            builder.field("group_key", &self.group_key_display());
        }
        builder.field("aggs", &self.agg_calls_display());
    }

    pub fn fields_pretty<'a>(&self) -> StrAssocArr<'a> {
        let last = ("aggs", self.agg_calls_pretty());
        if self.group_key.count_ones(..) != 0 {
            let first = ("group_key", self.group_key_pretty());
            vec![first, last]
        } else {
            vec![last]
        }
    }

    fn agg_calls_display(&self) -> Vec<PlanAggCallDisplay<'_>> {
        let f = |plan_agg_call| PlanAggCallDisplay {
            plan_agg_call,
            input_schema: self.input.schema(),
        };
        self.agg_calls.iter().map(f).collect()
    }

    fn agg_calls_pretty<'a>(&self) -> Pretty<'a> {
        let f = |plan_agg_call| {
            Pretty::debug(&PlanAggCallDisplay {
                plan_agg_call,
                input_schema: self.input.schema(),
            })
        };
        Pretty::Array(self.agg_calls.iter().map(f).collect())
    }

    fn group_key_display(&self) -> Vec<FieldDisplay<'_>> {
        let f = |i| FieldDisplay(self.input.schema().fields.get(i).unwrap());
        self.group_key.ones().map(f).collect()
    }

    fn group_key_pretty<'a>(&self) -> Pretty<'a> {
        let f = |i| Pretty::display(&FieldDisplay(self.input.schema().fields.get(i).unwrap()));
        Pretty::Array(self.group_key.ones().map(f).collect())
    }
}

impl_distill_unit_from_fields!(Agg, stream::StreamPlanRef);

/// Rewritten version of [`AggCall`] which uses `InputRef` instead of `ExprImpl`.
/// Refer to [`LogicalAggBuilder::try_rewrite_agg_call`] for more details.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PlanAggCall {
    /// Kind of aggregation function
    pub agg_kind: AggKind,

    /// Data type of the returned column
    pub return_type: DataType,

    /// Column indexes of input columns.
    ///
    /// Its length can be:
    /// - 0 (`RowCount`)
    /// - 1 (`Max`, `Min`)
    /// - 2 (`StringAgg`).
    ///
    /// Usually, we mark the first column as the aggregated column.
    pub inputs: Vec<InputRef>,

    pub distinct: bool,
    pub order_by: Vec<ColumnOrder>,
    /// Selective aggregation: only the input rows for which
    /// `filter` evaluates to `true` will be fed to the aggregate function.
    pub filter: Condition,
}

impl fmt::Debug for PlanAggCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.agg_kind)?;
        if !self.inputs.is_empty() {
            write!(f, "(")?;
            for (idx, input) in self.inputs.iter().enumerate() {
                if idx == 0 && self.distinct {
                    write!(f, "distinct ")?;
                }
                write!(f, "{:?}", input)?;
                if idx != (self.inputs.len() - 1) {
                    write!(f, ",")?;
                }
            }
            if !self.order_by.is_empty() {
                let clause_text = self.order_by.iter().map(|e| format!("{:?}", e)).join(", ");
                write!(f, " order_by({})", clause_text)?;
            }
            write!(f, ")")?;
        }
        if !self.filter.always_true() {
            write!(
                f,
                " filter({:?})",
                self.filter.as_expr_unless_true().unwrap()
            )?;
        }
        Ok(())
    }
}

impl PlanAggCall {
    pub fn rewrite_input_index(&mut self, mapping: ColIndexMapping) {
        // modify input
        self.inputs.iter_mut().for_each(|x| {
            x.index = mapping.map(x.index);
        });

        // modify order_by exprs
        self.order_by.iter_mut().for_each(|x| {
            x.column_index = mapping.map(x.column_index);
        });

        // modify filter
        let mut rewriter = IndexRewriter::new(mapping);
        self.filter.conjunctions.iter_mut().for_each(|x| {
            *x = rewriter.rewrite_expr(x.clone());
        });
    }

    pub fn to_protobuf(&self) -> PbAggCall {
        PbAggCall {
            r#type: self.agg_kind.to_protobuf().into(),
            return_type: Some(self.return_type.to_protobuf()),
            args: self.inputs.iter().map(InputRef::to_proto).collect(),
            distinct: self.distinct,
            order_by: self.order_by.iter().map(ColumnOrder::to_protobuf).collect(),
            filter: self.filter.as_expr_unless_true().map(|x| x.to_expr_proto()),
        }
    }

    pub fn partial_to_total_agg_call(&self, partial_output_idx: usize) -> PlanAggCall {
        let total_agg_kind = match &self.agg_kind {
            AggKind::BitAnd
            | AggKind::BitOr
            | AggKind::BitXor
            | AggKind::BoolAnd
            | AggKind::BoolOr
            | AggKind::Min
            | AggKind::Max
            | AggKind::StringAgg
            | AggKind::FirstValue
            | AggKind::PercentileCont
            | AggKind::PercentileDisc
            | AggKind::Mode => self.agg_kind,
            AggKind::Count | AggKind::ApproxCountDistinct | AggKind::Sum0 => AggKind::Sum0,
            AggKind::Sum => AggKind::Sum,
            AggKind::Avg => {
                panic!("Avg aggregation should have been rewritten to Sum+Count")
            }
            AggKind::ArrayAgg | AggKind::JsonbAgg | AggKind::JsonbObjectAgg => {
                panic!("2-phase {} is not supported yet", self.agg_kind)
            }
            AggKind::StddevPop | AggKind::StddevSamp | AggKind::VarPop | AggKind::VarSamp => {
                panic!("Stddev/Var aggregation should have been rewritten to Sum, Count and Case")
            }
        };
        PlanAggCall {
            agg_kind: total_agg_kind,
            inputs: vec![InputRef::new(partial_output_idx, self.return_type.clone())],
            order_by: vec![], // order must make no difference when we use 2-phase agg
            filter: Condition::true_cond(),
            ..self.clone()
        }
    }

    pub fn count_star() -> Self {
        PlanAggCall {
            agg_kind: AggKind::Count,
            return_type: DataType::Int64,
            inputs: vec![],
            distinct: false,
            order_by: vec![],
            filter: Condition::true_cond(),
        }
    }

    pub fn with_condition(mut self, filter: Condition) -> Self {
        self.filter = filter;
        self
    }

    pub fn input_indices(&self) -> Vec<usize> {
        self.inputs.iter().map(|input| input.index()).collect()
    }
}

pub struct PlanAggCallDisplay<'a> {
    pub plan_agg_call: &'a PlanAggCall,
    pub input_schema: &'a Schema,
}

impl fmt::Debug for PlanAggCallDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let that = self.plan_agg_call;
        write!(f, "{}", that.agg_kind)?;
        if !that.inputs.is_empty() {
            write!(f, "(")?;
            for (idx, input) in that.inputs.iter().enumerate() {
                if idx == 0 && that.distinct {
                    write!(f, "distinct ")?;
                }
                write!(
                    f,
                    "{}",
                    InputRefDisplay {
                        input_ref: input,
                        input_schema: self.input_schema
                    }
                )?;
                if idx != (that.inputs.len() - 1) {
                    write!(f, ", ")?;
                }
            }
            if !that.order_by.is_empty() {
                write!(
                    f,
                    " order_by({})",
                    that.order_by.iter().format_with(", ", |o, f| {
                        f(&ColumnOrderDisplay {
                            column_order: o,
                            input_schema: self.input_schema,
                        })
                    })
                )?;
            }
            write!(f, ")")?;
        }

        if !that.filter.always_true() {
            write!(
                f,
                " filter({:?})",
                ConditionDisplay {
                    condition: &that.filter,
                    input_schema: self.input_schema,
                }
            )?;
        }
        Ok(())
    }
}
