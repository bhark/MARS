//! parse + eval cost across representative `when:` filters. eval runs over a
//! synthetic row corpus so per-feature evaluator throughput is visible.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_expr::{AttributeAccess, Expr, Literal, eval, parse};

const ROWS: u64 = 10_000;

const SHORT_EQ: &str = "ttype = 'forest'";
const COMPOUND: &str = "ttype = 'forest' AND area >= 1000 AND name LIKE 'foo%'";
const IN_LIST_8: &str = "kind IN ('a','b','c','d','e','f','g','h')";

struct Map(HashMap<String, Literal>);

impl AttributeAccess for Map {
    fn get(&self, name: &str) -> Option<Literal> {
        self.0.get(name).cloned()
    }
}

fn row_for_compound(i: u64) -> Map {
    // every fourth row matches `ttype = 'forest'`, area gates half of those,
    // and the LIKE pattern shifts whether the third clause short-circuits.
    let mut m = HashMap::with_capacity(3);
    let ttype = if i.is_multiple_of(4) { "forest" } else { "field" };
    m.insert("ttype".into(), Literal::String(ttype.into()));
    m.insert("area".into(), Literal::Int(((i % 4096) * 7) as i64));
    let name = if i.is_multiple_of(3) {
        format!("foo_{i}")
    } else {
        format!("bar_{i}")
    };
    m.insert("name".into(), Literal::String(name));
    Map(m)
}

fn row_for_in_list(i: u64) -> Map {
    // distribute across the 8 list values plus a miss bucket.
    let kinds = ["a", "b", "c", "d", "e", "f", "g", "h", "z"];
    let mut m = HashMap::with_capacity(1);
    m.insert("kind".into(), Literal::String(kinds[(i as usize) % kinds.len()].into()));
    Map(m)
}

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("expr_parse");
    for (name, src) in [
        ("short_eq", SHORT_EQ),
        ("compound_and_or", COMPOUND),
        ("in_list_8", IN_LIST_8),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), &src, |b, src| {
            b.iter(|| {
                let e = parse(black_box(src)).unwrap();
                black_box(e)
            });
        });
    }
    group.finish();
}

fn bench_eval(c: &mut Criterion) {
    let compound: Expr = parse(COMPOUND).unwrap();
    let in_list: Expr = parse(IN_LIST_8).unwrap();
    let compound_rows: Vec<Map> = (0..ROWS).map(row_for_compound).collect();
    let in_list_rows: Vec<Map> = (0..ROWS).map(row_for_in_list).collect();

    let mut group = c.benchmark_group("expr_eval");
    group.throughput(Throughput::Elements(ROWS));

    group.bench_function("compound_and_or", |b| {
        b.iter(|| {
            let mut hits = 0u64;
            for row in &compound_rows {
                let r = eval(&compound, row).unwrap();
                if matches!(r, Literal::Bool(true)) {
                    hits += 1;
                }
            }
            black_box(hits)
        });
    });

    group.bench_function("in_list_8", |b| {
        b.iter(|| {
            let mut hits = 0u64;
            for row in &in_list_rows {
                let r = eval(&in_list, row).unwrap();
                if matches!(r, Literal::Bool(true)) {
                    hits += 1;
                }
            }
            black_box(hits)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_parse, bench_eval);
criterion_main!(benches);
