//! topology-simplifier harness entrypoint.
//!
//! workspace-excluded operator tool. see README.md for usage and
//! PHASE0_GATE.md for the pass/fail criteria the trailing summary line
//! evaluates.

use clap::Parser;

mod dp;
mod graph;
mod imagediff;
mod ingest;
mod reassemble;
mod timing;
mod verify;

use timing::{StageTimings, fmt_stage_normalised, peak_rss_kib};

#[derive(Debug, Parser)]
#[command(name = "topology-simplifier", about = "topology-aware simplification spike")]
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

    /// canvas size (px on the long axis) for the informational image cross-check
    #[arg(long, default_value_t = 4096)]
    image_canvas_px: u32,

    /// disable the image cross-check entirely (useful on huge fixtures)
    #[arg(long, default_value_t = false)]
    no_image: bool,
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

    std::fs::create_dir_all(&args.out)?;

    // gate-decision accumulators: levels 1+2 must hold zero seam violations,
    // every level must keep degenerate fraction below the threshold.
    let mut seam_violations_at_fine_levels: u64 = 0;
    let mut over_degenerate_threshold = false;
    let mut per_level_summary: Vec<(usize, u64, u64, u64)> = Vec::new();

    for (i, tol) in args.tolerance_m.iter().enumerate() {
        let level_label = format!("{}", i + 1);
        let simp = t.record(&format!("dp_l{level_label}"), || dp::simplify_arcs(&topo, *tol));
        let (out, rstats) = t.record(&format!("reassemble_l{level_label}"), || {
            reassemble::reassemble(&geoms, &topo, &simp)
        });
        let sstats = t.record(&format!("verify_l{level_label}"), || {
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
        if !args.no_image {
            let report = t.record(&format!("imagediff_l{level_label}"), || {
                imagediff::render_and_diff(&geoms, &out, args.image_canvas_px, &args.out, &level_label)
            })?;
            eprintln!(
                "  imagediff: {}/{} differing pixels ({:.4}%) - {}",
                report.differing_pixels,
                report.total_pixels,
                if report.total_pixels == 0 {
                    0.0
                } else {
                    100.0 * (report.differing_pixels as f64) / (report.total_pixels as f64)
                },
                report.diff_path.display(),
            );
        }

        // gate accumulators
        if i < 2 {
            seam_violations_at_fine_levels += sstats.seam_violation_count;
        }
        let degenerate = rstats.collapsed_ring_count + rstats.invalid_reassembly_count + rstats.self_intersection_count;
        if ingest_stats.kept > 0 {
            let frac = (degenerate as f64) / (ingest_stats.kept as f64);
            if frac > args.degenerate_threshold {
                over_degenerate_threshold = true;
            }
        }
        per_level_summary.push((i + 1, sstats.seam_violation_count, degenerate, ingest_stats.kept));
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

    eprintln!();
    eprintln!("gate (see PHASE0_GATE.md):");
    for (lvl, viol, degen, kept) in &per_level_summary {
        eprintln!(
            "  level {}: seam_violations={} degenerate={}/{} ({:.4}%)",
            lvl,
            viol,
            degen,
            kept,
            if *kept == 0 {
                0.0
            } else {
                100.0 * (*degen as f64) / (*kept as f64)
            },
        );
    }
    let pass = seam_violations_at_fine_levels == 0 && !over_degenerate_threshold;
    if pass {
        eprintln!("GATE: PASS (no seam violations at levels 1-2; degenerate fraction within threshold)");
    } else {
        let mut reasons: Vec<String> = Vec::new();
        if seam_violations_at_fine_levels > 0 {
            reasons.push(format!(
                "{} seam violation(s) at levels 1-2",
                seam_violations_at_fine_levels
            ));
        }
        if over_degenerate_threshold {
            reasons.push(format!(
                "degenerate fraction exceeded {:.3}% threshold",
                args.degenerate_threshold * 100.0
            ));
        }
        eprintln!("GATE: FAIL ({})", reasons.join("; "));
    }
    Ok(())
}
