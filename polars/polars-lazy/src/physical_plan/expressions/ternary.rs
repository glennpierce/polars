use crate::physical_plan::state::ExecutionState;
use crate::prelude::*;
use polars_core::frame::groupby::GroupsProxy;
use polars_core::prelude::*;
use polars_core::series::unstable::UnstableSeries;
use polars_core::POOL;
use std::convert::TryFrom;
use std::sync::Arc;

pub struct TernaryExpr {
    predicate: Arc<dyn PhysicalExpr>,
    truthy: Arc<dyn PhysicalExpr>,
    falsy: Arc<dyn PhysicalExpr>,
    expr: Expr,
}

impl TernaryExpr {
    pub fn new(
        predicate: Arc<dyn PhysicalExpr>,
        truthy: Arc<dyn PhysicalExpr>,
        falsy: Arc<dyn PhysicalExpr>,
        expr: Expr,
    ) -> Self {
        Self {
            predicate,
            truthy,
            falsy,
            expr,
        }
    }
}

fn expand_lengths(truthy: &mut Series, falsy: &mut Series, mask: &mut BooleanChunked) {
    let len = std::cmp::max(std::cmp::max(truthy.len(), falsy.len()), mask.len());
    if len > 1 {
        if falsy.len() == 1 {
            *falsy = falsy.expand_at_index(0, len);
        }
        if truthy.len() == 1 {
            *truthy = truthy.expand_at_index(0, len);
        }
        if mask.len() == 1 {
            *mask = mask.expand_at_index(0, len);
        }
    }
}

impl PhysicalExpr for TernaryExpr {
    fn as_expression(&self) -> &Expr {
        &self.expr
    }
    fn evaluate(&self, df: &DataFrame, state: &ExecutionState) -> Result<Series> {
        let mask_series = self.predicate.evaluate(df, state)?;
        let mut mask = mask_series.bool()?.clone();

        let op_truthy = || self.truthy.evaluate(df, state);
        let op_falsy = || self.falsy.evaluate(df, state);

        let (truthy, falsy) = POOL.install(|| rayon::join(op_truthy, op_falsy));
        let mut truthy = truthy?;
        let mut falsy = falsy?;
        expand_lengths(&mut truthy, &mut falsy, &mut mask);

        truthy.zip_with(&mask, &falsy)
    }
    fn to_field(&self, input_schema: &Schema) -> Result<Field> {
        self.truthy.to_field(input_schema)
    }

    #[allow(clippy::ptr_arg)]
    fn evaluate_on_groups<'a>(
        &self,
        df: &DataFrame,
        groups: &'a GroupsProxy,
        state: &ExecutionState,
    ) -> Result<AggregationContext<'a>> {
        let required_height = df.height();

        let op_mask = || self.predicate.evaluate_on_groups(df, groups, state);
        let op_truthy = || self.truthy.evaluate_on_groups(df, groups, state);
        let op_falsy = || self.falsy.evaluate_on_groups(df, groups, state);

        let (ac_mask, (ac_truthy, ac_falsy)) =
            POOL.install(|| rayon::join(op_mask, || rayon::join(op_truthy, op_falsy)));
        let mut ac_mask = ac_mask?;
        let mut ac_truthy = ac_truthy?;
        let mut ac_falsy = ac_falsy?;

        let mask_s = ac_mask.flat_naive();

        assert!(
            ac_truthy.can_combine(&ac_falsy),
            "cannot combine this ternary expression, the groups do not match"
        );

        match (ac_truthy.agg_state(), ac_falsy.agg_state()) {
            // if the groups_len == df.len we can just apply all flat.
            (AggState::AggregatedFlat(s), AggState::NotAggregated(_) | AggState::Literal(_))
                if s.len() != df.height() =>
            {
                // this is a flat series of len eq to group tuples
                let truthy = ac_truthy.aggregated_arity_operation();
                let truthy = truthy.as_ref();
                let arr_truthy = &truthy.chunks()[0];
                assert_eq!(truthy.len(), groups.len());

                // we create a dummy Series that is not cloned nor moved
                // so we can swap the ArrayRef during the hot loop
                // this prevents a series Arc alloc and a vec alloc per iteration
                let dummy = Series::try_from(("dummy", vec![arr_truthy.clone()])).unwrap();
                let mut us = UnstableSeries::new(&dummy);

                // this is now a list
                let falsy = ac_falsy.aggregated_arity_operation();
                let falsy = falsy.as_ref();
                let falsy = falsy.list().unwrap();

                let mask = ac_mask.aggregated_arity_operation();
                let mask = mask.as_ref();
                let mask = mask.list()?;
                if !matches!(mask.inner_dtype(), DataType::Boolean) {
                    return Err(PolarsError::ComputeError(
                        format!("expected mask of type bool, got {:?}", mask.inner_dtype()).into(),
                    ));
                }

                let mut ca: ListChunked = falsy
                    .amortized_iter()
                    .zip(mask.amortized_iter())
                    .enumerate()
                    .map(|(idx, (opt_falsy, opt_mask))| {
                        match (opt_falsy, opt_mask) {
                            (Some(falsy), Some(mask)) => {
                                let falsy = falsy.as_ref();
                                let mask = mask.as_ref();
                                let mask = mask.bool()?;

                                // Safety:
                                // we are in bounds
                                let arr = unsafe { Arc::from(arr_truthy.slice_unchecked(idx, 1)) };
                                us.swap(arr);
                                let truthy = us.as_ref();

                                Some(truthy.zip_with(mask, falsy))
                            }
                            _ => None,
                        }
                        .transpose()
                    })
                    .collect::<Result<_>>()?;
                ca.rename(truthy.name());

                ac_truthy.with_series(ca.into_series(), true);
                Ok(ac_truthy)
            }
            // all aggregated or literal
            // simply align lengths and zip
            (
                AggState::Literal(truthy) | AggState::AggregatedFlat(truthy),
                AggState::AggregatedFlat(falsy) | AggState::Literal(falsy),
            )
            | (AggState::AggregatedList(truthy), AggState::AggregatedList(falsy))
                if matches!(ac_mask.agg_state(), AggState::AggregatedFlat(_)) =>
            {
                let mut truthy = truthy.clone();
                let mut falsy = falsy.clone();
                let mut mask = ac_mask.series().bool()?.clone();
                expand_lengths(&mut truthy, &mut falsy, &mut mask);
                let mut out = truthy.zip_with(&mask, &falsy).unwrap();
                out.rename(truthy.name());
                ac_truthy.with_series(out, true);
                Ok(ac_truthy)
            }
            // if the groups_len == df.len we can just apply all flat.
            (AggState::NotAggregated(_) | AggState::Literal(_), AggState::AggregatedFlat(s))
                if s.len() != df.height() =>
            {
                // this is now a list
                let truthy = ac_truthy.aggregated_arity_operation();
                let truthy = truthy.as_ref();
                let truthy = truthy.list().unwrap();

                // this is a flat series of len eq to group tuples
                let falsy = ac_falsy.aggregated_arity_operation();
                assert_eq!(falsy.len(), groups.len());
                let falsy = falsy.as_ref();
                let arr_falsy = &falsy.chunks()[0];

                // we create a dummy Series that is not cloned nor moved
                // so we can swap the ArrayRef during the hot loop
                // this prevents a series Arc alloc and a vec alloc per iteration
                let dummy = Series::try_from(("dummy", vec![arr_falsy.clone()])).unwrap();
                let mut us = UnstableSeries::new(&dummy);

                let mask = ac_mask.aggregated_arity_operation();
                let mask = mask.as_ref();
                let mask = mask.list()?;
                if !matches!(mask.inner_dtype(), DataType::Boolean) {
                    return Err(PolarsError::ComputeError(
                        format!("expected mask of type bool, got {:?}", mask.inner_dtype()).into(),
                    ));
                }

                let mut ca: ListChunked = truthy
                    .amortized_iter()
                    .zip(mask.amortized_iter())
                    .enumerate()
                    .map(|(idx, (opt_truthy, opt_mask))| {
                        match (opt_truthy, opt_mask) {
                            (Some(truthy), Some(mask)) => {
                                let truthy = truthy.as_ref();
                                let mask = mask.as_ref();
                                let mask = mask.bool()?;

                                // Safety:
                                // we are in bounds
                                let arr = unsafe { Arc::from(arr_falsy.slice_unchecked(idx, 1)) };
                                us.swap(arr);
                                let falsy = us.as_ref();

                                Some(truthy.zip_with(mask, falsy))
                            }
                            _ => None,
                        }
                        .transpose()
                    })
                    .collect::<Result<_>>()?;
                ca.rename(truthy.name());

                ac_truthy.with_series(ca.into_series(), true);
                Ok(ac_truthy)
            }

            // Both are or a flat series or aggregated into a list
            // so we can flatten the Series an apply the operators
            _ => {
                let mask = mask_s.bool()?;
                let out = ac_truthy
                    .flat_naive()
                    .zip_with(mask, ac_falsy.flat_naive().as_ref())?;

                assert!((out.len() == required_height), "The output of the `when -> then -> otherwise-expr` is of a different length than the groups.\
The expr produced {} values. Where the original DataFrame has {} values",
                        out.len(),
                        required_height);

                ac_truthy.with_series(out, false);

                Ok(ac_truthy)
            }
        }
    }
    fn as_partitioned_aggregator(&self) -> Option<&dyn PartitionedAggregation> {
        Some(self)
    }
}

impl PartitionedAggregation for TernaryExpr {
    fn evaluate_partitioned(
        &self,
        df: &DataFrame,
        groups: &GroupsProxy,
        state: &ExecutionState,
    ) -> Result<Series> {
        let truthy = self.truthy.as_partitioned_aggregator().unwrap();
        let falsy = self.falsy.as_partitioned_aggregator().unwrap();
        let mask = self.predicate.as_partitioned_aggregator().unwrap();

        let mut truthy = truthy.evaluate_partitioned(df, groups, state)?;
        let mut falsy = falsy.evaluate_partitioned(df, groups, state)?;
        let mask = mask.evaluate_partitioned(df, groups, state)?;
        let mut mask = mask.bool()?.clone();

        expand_lengths(&mut truthy, &mut falsy, &mut mask);
        truthy.zip_with(&mask, &falsy)
    }

    fn finalize(
        &self,
        partitioned: Series,
        _groups: &GroupsProxy,
        _state: &ExecutionState,
    ) -> Result<Series> {
        Ok(partitioned)
    }
}
