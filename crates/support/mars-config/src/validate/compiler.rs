use crate::ConfigError;
use crate::model::Config;

pub(super) fn validate_compiler_and_render(config: &Config) -> Result<(), ConfigError> {
    let _ = config.compiler.window_dur()?;

    let working_set = config.compiler.compile_page_working_set()?;
    if working_set == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_page_working_set_bytes must be > 0".into(),
        ));
    }

    let plan_budget = config.compiler.compile_plan_budget()?;
    if plan_budget == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_plan_budget_bytes must be > 0".into(),
        ));
    }

    let parallelism = config.compiler.compile_binding_parallelism;
    if parallelism == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_binding_parallelism must be > 0".into(),
        ));
    }
    // compare against the tightest pool ceiling across postgis sources -
    // parallelism is service-wide, so the smallest configured ceiling caps
    // it. vectorfile sources have no pool concept and are skipped.
    let pool_ceiling = config
        .sources
        .iter()
        .filter_map(|s| s.postgis())
        .filter_map(|pg| pg.pool.max_size)
        .min();
    if let Some(pool_max) = pool_ceiling
        && parallelism > pool_max
    {
        return Err(ConfigError::Invalid(format!(
            "compiler.compile_binding_parallelism ({parallelism}) exceeds the smallest postgis source pool max_size \
             ({pool_max}); raise the pool size or lower the parallelism"
        )));
    }

    let _ = config.compiler.rebalance.window_dur()?;

    if config.render.page_fetch_concurrency == 0 {
        return Err(ConfigError::Invalid(
            "render.page_fetch_concurrency must be >= 1".into(),
        ));
    }
    Ok(())
}
