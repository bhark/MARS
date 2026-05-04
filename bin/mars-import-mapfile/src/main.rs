//! mars-import-mapfile: translate a MapServer mapfile into a MARS YAML config.
//! Phase 0 scaffolder. Synchronous; no tokio.

mod emitter;
mod scanner;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::warn;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer};

use crate::emitter::Skeleton;
use crate::scanner::{Token, block_range, is_block_opener, scan};

#[derive(Debug, Parser)]
#[command(
    name = "mars-import-mapfile",
    version,
    about = "Translate a MapServer mapfile to a MARS YAML config (scaffolding)."
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
}

/// keywords whose presence we don't translate yet. some are block openers,
/// some are scalar directives — `walk` handles both.
const UNSUPPORTED: &[&str] = &[
    "CLASS",
    "STYLE",
    "SYMBOL",
    "FONTSET",
    "LEGEND",
    "PROJECTION",
    "METADATA",
    "OUTPUTFORMAT",
    "LABEL",
    "FEATURE",
    "JOIN",
    "COMPOSITE",
    "CLUSTER",
    "GRID",
    "VALIDATION",
];

fn is_unsupported(kw: &str) -> bool {
    let up = kw.to_ascii_uppercase();
    UNSUPPORTED.iter().any(|b| *b == up)
}

struct WarnCounter {
    counter: Arc<AtomicUsize>,
}

impl<S> Layer<S> for WarnCounter
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
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

    let src = fs::read_to_string(&cli.input)
        .with_context(|| format!("reading {}", cli.input.display()))?;
    let skeleton = translate(&src);
    let yaml = emitter::render(&skeleton);

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

/// translate a mapfile source into a YAML skeleton, warning on unsupported
/// constructs as a side-effect via `tracing::warn!`.
pub(crate) fn translate(src: &str) -> Skeleton {
    let tokens = scan(src);
    let mut skel = Skeleton::default();

    let map_slice: &[Token] = match tokens
        .iter()
        .position(|t| t.keyword.eq_ignore_ascii_case("MAP"))
        .and_then(|i| block_range(&tokens, i))
    {
        Some(r) => &tokens[r.start + 1..r.end.saturating_sub(1).max(r.start + 1)],
        None => &tokens[..],
    };

    walk(map_slice, &mut skel);
    skel
}

fn walk(tokens: &[Token], skel: &mut Skeleton) {
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let kw = t.keyword.to_ascii_uppercase();

        if kw == "NAME" && skel.service_name.is_none() {
            if let Some(v) = t.args.first() {
                skel.service_name = Some(v.clone());
            }
            i += 1;
            continue;
        }
        if kw == "TITLE" && skel.service_title.is_none() {
            if let Some(v) = t.args.first() {
                skel.service_title = Some(v.clone());
            }
            i += 1;
            continue;
        }

        if kw == "LAYER" {
            let range = block_range(tokens, i).unwrap_or(i..i + 1);
            let body: &[Token] = if range.end > range.start + 1 {
                &tokens[range.start + 1..range.end - 1]
            } else {
                &[]
            };
            handle_layer(body, t.line, skel);
            i = range.end;
            continue;
        }

        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "phase-0: unsupported mapfile construct");
            // skip block contents so we don't double-warn on nested tokens
            if is_block_opener(&kw)
                && let Some(r) = block_range(tokens, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }
}

fn handle_layer(body: &[Token], layer_line: usize, skel: &mut Skeleton) {
    let mut name: Option<String> = None;
    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        let kw = t.keyword.to_ascii_uppercase();
        if kw == "NAME" && name.is_none() {
            name = t.args.first().cloned();
            i += 1;
            continue;
        }
        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "phase-0: unsupported mapfile construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(body, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }
    let resolved = name.unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));
    skel.layers.push(resolved);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_extracts_name_title_and_layers() {
        let src = r#"
MAP
  NAME "demo"
  TITLE "Demo Service"
  LAYER
    NAME "roads"
    TYPE LINE
  END
  LAYER
    NAME "buildings"
    TYPE POLYGON
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("demo"));
        assert_eq!(skel.service_title.as_deref(), Some("Demo Service"));
        assert_eq!(
            skel.layers,
            vec!["roads".to_string(), "buildings".to_string()]
        );
    }

    #[test]
    fn unsupported_construct_does_not_break_translation() {
        let src = r#"
MAP
  NAME "x"
  SYMBOL
    NAME "dot"
    TYPE ELLIPSE
  END
  LAYER
    NAME "l1"
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("x"));
        assert_eq!(skel.layers, vec!["l1".to_string()]);
    }

    #[test]
    fn comments_and_case_are_handled() {
        let src = r#"
map # top-level
  name "abc"   # service name
  layer
    name "only"
  end
end
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("abc"));
        assert_eq!(skel.layers, vec!["only".to_string()]);
    }
}
