#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // exercise the artifact reader on arbitrary bytes; any panic is a bug.
    let bytes = bytes::Bytes::copy_from_slice(data);
    let _ = mars_artifact::ArtifactReader::open(bytes);
});
