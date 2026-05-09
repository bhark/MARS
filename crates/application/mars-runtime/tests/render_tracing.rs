//! verifies the per-phase tracing spans in `render_plan` fire under the
//! `mars_runtime::render` target. structurally guards against silent
//! regression if a future refactor moves a span site or renames the target.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io;
use std::sync::{Arc, Mutex};

use common::build_fixture;
use tracing_subscriber::{EnvFilter, fmt::MakeWriter, util::SubscriberInitExt};

#[derive(Clone, Default)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedWriter {
    type Writer = CapturedWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn render_emits_expected_span_tree_under_target() {
    let writer = CapturedWriter::default();
    let captured = writer.0.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("mars_runtime::render=info"))
        .with_writer(writer)
        .with_target(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
        .finish();

    // current-thread runtime keeps every poll on this thread so the
    // thread-local default dispatcher set below stays in scope across
    // every await; multi-thread workers would defeat that.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = subscriber.set_default();
    rt.block_on(async {
        let fix = build_fixture().await;
        let plan = fix.render_plan();
        fix.runtime.render(&plan).await.expect("render");
    });
    drop(_guard);

    let buf = captured.lock().unwrap();
    let log = String::from_utf8(buf.clone()).unwrap();

    for span in [
        "render.plan",
        "render.layer",
        "render.layer.fetch",
        "render.layer.decode",
        "render.collide",
        "render.paint",
        "render.encode",
    ] {
        assert!(
            log.contains(span),
            "expected span `{span}` in captured trace; got:\n{log}"
        );
    }
    assert!(
        log.contains("mars_runtime::render"),
        "expected `mars_runtime::render` target in captured trace; got:\n{log}"
    );
}
