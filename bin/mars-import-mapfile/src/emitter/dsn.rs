//! PostGIS CONNECTION string folding across per-layer skeletons.

/// outcome of folding per-layer PostGIS CONNECTION lifts into a single
/// MAP-scope `source.dsn`. config currently has no per-layer dsn override,
/// so mixed input falls back to the placeholder.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LiftedSourceDsn<'a> {
    Placeholder,
    Lifted(&'a str),
    Mixed,
}

pub(super) fn fold_postgis_dsns<'a, I>(dsns: I) -> LiftedSourceDsn<'a>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut chosen: Option<&str> = None;
    for d in dsns {
        match chosen {
            None => chosen = Some(d),
            Some(c) if c == d => {}
            Some(_) => return LiftedSourceDsn::Mixed,
        }
    }
    match chosen {
        Some(d) => LiftedSourceDsn::Lifted(d),
        None => LiftedSourceDsn::Placeholder,
    }
}

#[cfg(test)]
mod tests;
