use super::*;

#[cfg_attr(not(feature = "dynamic_groupby"), allow(dead_code))]
pub(crate) struct GroupByDynamicExec {
    pub(crate) input: Box<dyn Executor>,
    // we will use this later
    #[allow(dead_code)]
    pub(crate) keys: Vec<Arc<dyn PhysicalExpr>>,
    pub(crate) aggs: Vec<Arc<dyn PhysicalExpr>>,
    pub(crate) options: DynamicGroupOptions,
    pub(crate) input_schema: SchemaRef,
    pub(crate) slice: Option<(i64, usize)>,
}

impl GroupByDynamicExec {
    #[cfg(feature = "dynamic_groupby")]
    fn execute_impl(&mut self, state: &mut ExecutionState, mut df: DataFrame) -> Result<DataFrame> {
        df.as_single_chunk_par();
        state.set_schema(self.input_schema.clone());

        // if the periods are larger than the intervals,
        // the groups overlap
        if self.options.every < self.options.period {
            state.flags |= StateFlags::OVERLAPPING_GROUPS
        }

        let keys = self
            .keys
            .iter()
            .map(|e| e.evaluate(&df, state))
            .collect::<Result<Vec<_>>>()?;

        let (mut time_key, mut keys, groups) = df.groupby_dynamic(keys, &self.options)?;

        let mut groups = &groups;
        #[allow(unused_assignments)]
        // it is unused because we only use it to keep the lifetime of sliced_group valid
        let mut sliced_groups = None;

        if let Some((offset, len)) = self.slice {
            sliced_groups = Some(groups.slice(offset, len));
            groups = sliced_groups.as_deref().unwrap();

            time_key = time_key.slice(offset, len);

            // todo! optimize this, we can prevent an agg_first aggregation upstream
            // the ordering has changed due to the groupby
            for key in keys.iter_mut() {
                *key = key.slice(offset, len)
            }
        }

        let agg_columns = POOL.install(|| {
            self.aggs
                .par_iter()
                .map(|expr| {
                    let agg = expr.evaluate_on_groups(&df, groups, state)?.finalize();
                    if agg.len() != groups.len() {
                        return Err(PolarsError::ComputeError(
                            format!("returned aggregation is a different length: {} than the group lengths: {}",
                                    agg.len(),
                                    groups.len()).into()
                        ))
                    }
                    Ok(agg)
                })
                .collect::<Result<Vec<_>>>()
        })?;

        state.clear_schema_cache();
        let mut columns = Vec::with_capacity(agg_columns.len() + 1 + keys.len());
        columns.extend_from_slice(&keys);
        columns.push(time_key);
        columns.extend_from_slice(&agg_columns);

        DataFrame::new(columns)
    }
}

impl Executor for GroupByDynamicExec {
    #[cfg(not(feature = "dynamic_groupby"))]
    fn execute(&mut self, _state: &mut ExecutionState) -> Result<DataFrame> {
        panic!("activate feature dynamic_groupby")
    }

    #[cfg(feature = "dynamic_groupby")]
    fn execute(&mut self, state: &mut ExecutionState) -> Result<DataFrame> {
        #[cfg(debug_assertions)]
        {
            if state.verbose() {
                println!("run GroupbyDynamicExec")
            }
        }
        let df = self.input.execute(state)?;

        let profile_name = if state.has_node_timer() {
            let by = self
                .keys
                .iter()
                .map(|s| Ok(s.to_field(&self.input_schema)?.name))
                .collect::<Result<Vec<_>>>()?;
            let name = column_delimited("groupby_dynamic".to_string(), &by);
            Cow::Owned(name)
        } else {
            Cow::Borrowed("")
        };

        if state.has_node_timer() {
            let new_state = state.clone();
            new_state.record(|| self.execute_impl(state, df), profile_name)
        } else {
            self.execute_impl(state, df)
        }
    }
}
