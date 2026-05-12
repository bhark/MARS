//! image encoding for png and jpeg.

mod jpeg;
mod png;

pub(crate) use jpeg::encode_jpeg;
pub(crate) use png::encode_png;
