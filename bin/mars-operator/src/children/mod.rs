//! Per-CR child object builders. Each module yields a fully-populated wire
//! type ready for server-side apply.

pub(crate) mod compiler;
pub(crate) mod configmap;
pub(crate) mod labels;
pub(crate) mod pdb;
pub(crate) mod pvc;
pub(crate) mod runtime;
pub(crate) mod service;

#[cfg(test)]
pub(crate) mod test_support;
