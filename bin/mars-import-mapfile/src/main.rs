//! mars-import-mapfile: opinionated translator from MapServer mapfile to MARS YAML.
//!
//! coverage is the subset exercised by the parity harness.
//! Synchronous; no tokio.

mod directive;
mod emitter;
mod expression;
mod parsing;
mod scanner;
mod translate;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer};

use crate::scanner::scan_file;
use crate::translate::translate_tokens;

#[derive(Debug, Parser)]
#[command(
    name = "mars-import-mapfile",
    version,
    about = "Translate a MapServer mapfile to a MARS YAML config."
)]
struct Cli {
    /// path to the input mapfile.
    input: PathBuf,
    /// path to write the output YAML to (defaults to stdout).
    #[arg(long)]
    output: Option<PathBuf>,
    /// exit non-zero (code 2) if any warnings were emitted.
    #[arg(long)]
    strict: bool,
    /// include only layers whose NAME matches one of these values (repeatable).
    #[arg(long = "include-layer")]
    include_layers: Vec<String>,
    /// override the default scale-band ladder.
    /// format: `name:cap,name:cap,...` with strictly-increasing caps; the
    /// final cap may be `max` to mean unbounded. example:
    /// `detail:2500,hi:12500,mid:50000,lo:250000,overview:max`.
    #[arg(long = "bands", value_parser = parse_bands_arg)]
    bands: Option<Vec<(String, u64)>>,
}

/// parse the `--bands` CLI value into an ordered ladder.
fn parse_bands_arg(s: &str) -> Result<Vec<(String, u64)>, String> {
    let mut out = Vec::new();
    let mut prev: Option<u64> = None;
    for (idx, part) in s.split(',').enumerate() {
        let entry = part.trim();
        let Some((name, cap_str)) = entry.split_once(':') else {
            return Err(format!("band entry {idx} {entry:?} missing `:`"));
        };
        let name = name.trim();
        let cap_str = cap_str.trim();
        if name.is_empty() {
            return Err(format!("band entry {idx} has empty name"));
        }
        let cap: u64 = if cap_str.eq_ignore_ascii_case("max") {
            u64::MAX
        } else {
            cap_str
                .parse()
                .map_err(|e| format!("band entry {idx} cap {cap_str:?}: {e}"))?
        };
        if let Some(p) = prev
            && cap <= p
        {
            return Err(format!(
                "band caps must strictly increase; entry {idx} cap {cap} <= previous {p}"
            ));
        }
        prev = Some(cap);
        out.push((name.to_string(), cap));
    }
    if out.is_empty() {
        return Err("--bands must declare at least one band".into());
    }
    Ok(out)
}

struct WarnCounter {
    counter: Arc<AtomicUsize>,
}

impl<S> Layer<S> for WarnCounter
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn install_tracing(counter: Arc<AtomicUsize>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(io::stderr);
    let count_layer = WarnCounter { counter }.with_filter(filter);
    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(count_layer)
        .try_init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let warn_count = Arc::new(AtomicUsize::new(0));
    install_tracing(warn_count.clone());

    let tokens = scan_file(&cli.input).with_context(|| format!("scanning {}", cli.input.display()))?;
    let include_layers: Option<std::collections::HashSet<String>> = if cli.include_layers.is_empty() {
        None
    } else {
        Some(cli.include_layers.into_iter().map(|s| s.to_lowercase()).collect())
    };
    let base_dir = cli.input.parent();
    let skeleton = translate_tokens(&tokens, include_layers.as_ref(), base_dir, cli.strict);
    let bands = cli.bands.unwrap_or_else(emitter::default_bands);
    let yaml = emitter::render(&skeleton, &bands);

    match &cli.output {
        Some(p) => fs::write(p, &yaml).with_context(|| format!("writing {}", p.display()))?,
        None => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(yaml.as_bytes())?;
        }
    }

    if cli.strict && warn_count.load(Ordering::Relaxed) > 0 {
        std::process::exit(2);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
