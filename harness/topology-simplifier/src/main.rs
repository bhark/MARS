//! topology-simplifier harness entrypoint.
//!
//! workspace-excluded operator tool; see README. successive commits add the
//! image cross-check and the gate write-up.

use clap::Parser;

mod dp;
mod graph;
mod ingest;
mod reassemble;
mod timing;
mod verify;

use timing::{StageTimings, fmt_stage_normalised, peak_rss_kib};

#[derive(Debug, Parser)]
#[command(name = "topology-simplifier", about = "Phase 0 topology-aware simplification spike")]
struct Args {
    /// path to TSV (id\thex_ewkb) polygon fixture dump
    #[arg(long)]
    fixture: std::path::PathBuf,

    /// output directory for gate report artifacts
    #[arg(long)]
    out: std::path::PathBuf,

    /// quantisation grid in millimetres (canonical-CRS units)
    #[arg(long, default_value_t = 1)]
    quantise_mm: u32,

    /// per-arc DP tolerances in metres (one per level); applied in order
    #[arg(long, value_delimiter = ',', default_values_t = [1.0_f64, 5.0, 25.0])]
    tolerance_m: Vec<f64>,

    /// max acceptable degenerate-reassembly fraction (per gate)
    #[arg(long, default_value_t = 0.001)]
    degenerate_threshold: f64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    eprintln!(
        "topology-simplifier: fixture={} out={} quantise={}mm tolerances={:?}",
        args.fixture.display(),
        args.out.display(),
        args.quantise_mm,
        args.tolerance_m,
    );
    let _ = args.degenerate_threshold;

    let mut t = StageTimings::default();

    let (geoms, ingest_stats) = t.record("ingest", || ingest::load_fixture(&args.fixture))?;
    eprintln!(
        "ingest: lines={} kept={} skipped_non_polygon={} skipped_bad_line={} skipped_bad_hex={} skipped_bad_wkb={}",
        ingest_stats.lines_read,
        ingest_stats.kept,
        ingest_stats.skipped_non_polygon,
        ingest_stats.skipped_bad_line,
        ingest_stats.skipped_bad_hex,
        ingest_stats.skipped_bad_wkb,
    );

    let (topo, gstats) = t.record("graph", || graph::build_topology(&geoms, args.quantise_mm));
    eprintln!(
        "graph: features={} rings={} vertices={} edges={} junctions={} arcs={} shared={} islands={}",
        gstats.feature_count,
        gstats.ring_count,
        gstats.vertex_count,
        gstats.edge_count,
        gstats.junction_count,
        gstats.arc_count,
        gstats.shared_arc_count,
        gstats.island_arc_count,
    );

    for (i, tol) in args.tolerance_m.iter().enumerate() {
        let level_label = format!("level{}", i + 1);
        let simp = t.record(&format!("dp_{level_label}"), || dp::simplify_arcs(&topo, *tol));
        let (out, rstats) = t.record(&format!("reassemble_{level_label}"), || {
            reassemble::reassemble(&geoms, &topo, &simp)
        });
        let sstats = t.record(&format!("verify_{level_label}"), || {
            verify::verify_seams(&topo, &simp, &out)
        });
        eprintln!(
            "level {} (tol {} m): rings={} collapsed_ring={} collapsed_arc={} invalid_hole={} self_isect={} \
             shared_arcs={} seam_violations={}",
            i + 1,
            tol,
            rstats.rings_total,
            rstats.collapsed_ring_count,
            rstats.collapsed_arc_count,
            rstats.invalid_reassembly_count,
            rstats.self_intersection_count,
            sstats.shared_arc_count,
            sstats.seam_violation_count,
        );
    }

    eprintln!();
    eprintln!("timings (feature_count = {}):", ingest_stats.kept);
    for (name, dur) in &t.stages {
        eprintln!("{}", fmt_stage_normalised(name, *dur, ingest_stats.kept));
    }
    eprintln!("  {:<24} {:>10.3} ms", "TOTAL", timing::dur_to_ms(t.total()));
    match peak_rss_kib() {
        Some(kib) => eprintln!("peak RSS: {:.1} MiB ({} KiB)", (kib as f64) / 1024.0, kib),
        None => eprintln!("peak RSS: n/a (non-linux or /proc/self/status read failed)"),
    }

    eprintln!("(image cross-check + gate write-up land in subsequent commits)");
    Ok(())
}
