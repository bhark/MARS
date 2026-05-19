//! resolution of `compiler.compile_binding_parallelism`.
//!
//! the config field is optional. when unset, the compiler self-sizes it to
//! `min(available_parallelism(), smallest postgis pool max_size)`; an explicit
//! value above the smallest pool ceiling is clamped down to it (each in-flight
//! binding holds one pooled connection). resolution is centralised here so the
//! snapshot and cycle paths cannot disagree, mirroring `governors.rs`.

use std::num::NonZero;

/// fallback parallelism when `available_parallelism()` is unavailable -
/// the historical default of this knob before it became self-sizing.
const FALLBACK_CPUS: usize = 2;

/// how a resolved parallelism value was arrived at, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// explicit config value, used verbatim.
    Explicit,
    /// explicit config value clamped down to the smallest pool ceiling.
    ClampedToPool { requested: usize },
    /// no config value - derived from cpu count and the pool ceiling.
    DerivedAuto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolution {
    value: usize,
    origin: Origin,
}

/// pure resolution: pick the effective parallelism from the explicit config
/// value, the smallest postgis pool ceiling, and the host cpu count. the
/// effective value is floored at 1 (guards a degenerate `pool.max_size: 0`).
fn decide(explicit: Option<usize>, pool_ceiling: Option<usize>, cpus: usize) -> Resolution {
    let (value, origin) = match explicit {
        Some(n) => match pool_ceiling {
            Some(ceil) if n > ceil => (ceil, Origin::ClampedToPool { requested: n }),
            _ => (n, Origin::Explicit),
        },
        None => (pool_ceiling.map_or(cpus, |c| cpus.min(c)), Origin::DerivedAuto),
    };
    Resolution {
        value: value.max(1),
        origin,
    }
}

/// resolve `compile_binding_parallelism` for a compile run, logging how the
/// value was chosen. an explicit value above the smallest pool ceiling is
/// clamped with a warning rather than failing the run.
pub(crate) fn resolve_binding_parallelism(compiler: &mars_config::Compiler, sources: &[mars_config::Source]) -> usize {
    // tightest pool ceiling across postgis sources - parallelism is
    // service-wide, so the smallest configured ceiling caps it. vectorfile
    // sources have no pool concept; a degenerate max_size of 0 is ignored.
    let pool_ceiling = sources
        .iter()
        .filter_map(mars_config::Source::postgis)
        .filter_map(|pg| pg.pool.max_size)
        .filter(|&n| n > 0)
        .min();
    let cpus = std::thread::available_parallelism()
        .map(NonZero::get)
        .unwrap_or(FALLBACK_CPUS);

    let resolved = decide(compiler.compile_binding_parallelism, pool_ceiling, cpus);
    match resolved.origin {
        Origin::ClampedToPool { requested } => tracing::warn!(
            target: "mars_compiler::compile",
            requested,
            pool_ceiling,
            resolved = resolved.value,
            "compile.binding_parallelism: configured value exceeds smallest postgis pool max_size; clamped"
        ),
        Origin::DerivedAuto => tracing::info!(
            target: "mars_compiler::compile",
            resolved = resolved.value,
            cpus,
            pool_ceiling,
            source = "auto",
            "compile.binding_parallelism: resolved"
        ),
        Origin::Explicit => tracing::info!(
            target: "mars_compiler::compile",
            resolved = resolved.value,
            source = "config",
            "compile.binding_parallelism: resolved"
        ),
    }
    resolved.value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_derives_min_of_cpus_and_pool() {
        let r = decide(None, Some(8), 4);
        assert_eq!(
            r,
            Resolution {
                value: 4,
                origin: Origin::DerivedAuto
            }
        );

        let r = decide(None, Some(3), 16);
        assert_eq!(
            r,
            Resolution {
                value: 3,
                origin: Origin::DerivedAuto
            }
        );
    }

    #[test]
    fn auto_derives_cpus_when_no_pool() {
        let r = decide(None, None, 6);
        assert_eq!(
            r,
            Resolution {
                value: 6,
                origin: Origin::DerivedAuto
            }
        );
    }

    #[test]
    fn explicit_below_ceiling_is_honored() {
        // operator throttling DB load - kept verbatim.
        let r = decide(Some(2), Some(8), 16);
        assert_eq!(
            r,
            Resolution {
                value: 2,
                origin: Origin::Explicit
            }
        );
    }

    #[test]
    fn explicit_above_ceiling_is_clamped() {
        let r = decide(Some(32), Some(8), 4);
        assert_eq!(
            r,
            Resolution {
                value: 8,
                origin: Origin::ClampedToPool { requested: 32 }
            }
        );
    }

    #[test]
    fn explicit_without_pool_is_honored() {
        let r = decide(Some(32), None, 4);
        assert_eq!(
            r,
            Resolution {
                value: 32,
                origin: Origin::Explicit
            }
        );
    }

    #[test]
    fn value_is_floored_at_one() {
        // degenerate pool ceilings are filtered before decide(), but a 0
        // reaching here must still not yield a halted compiler.
        let r = decide(Some(0), None, 4);
        assert_eq!(r.value, 1);
        let r = decide(None, None, 0);
        assert_eq!(r.value, 1);
    }
}
