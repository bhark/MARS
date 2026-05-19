use crate::ConfigError;
use crate::model::{Compiler, Render};

pub(super) fn validate_compiler_and_render(compiler: &Compiler, render: &Render) -> Result<(), ConfigError> {
    let _ = compiler.window_dur()?;

    let working_set = compiler.compile_page_working_set()?;
    if working_set == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_page_working_set_bytes must be > 0".into(),
        ));
    }

    let plan_budget = compiler.compile_plan_budget()?;
    if plan_budget == 0 {
        return Err(ConfigError::Invalid(
            "compiler.compile_plan_budget_bytes must be > 0".into(),
        ));
    }

    // an unset `compile_binding_parallelism` self-sizes at compile time; an
    // explicit value above the smallest postgis pool ceiling is clamped there
    // with a warning, not rejected. only an explicit `0` is a config error -
    // it would halt the compiler outright rather than throttle it.
    if compiler.compile_binding_parallelism == Some(0) {
        return Err(ConfigError::Invalid(
            "compiler.compile_binding_parallelism must be > 0 when set (omit it to auto-size)".into(),
        ));
    }

    let _ = compiler.rebalance.window_dur()?;

    if render.page_fetch_concurrency == 0 {
        return Err(ConfigError::Invalid(
            "render.page_fetch_concurrency must be >= 1".into(),
        ));
    }
    Ok(())
}
