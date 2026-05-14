//! eval/SQL parity gate: for every `mars-expr::Expr` operator, the in-memory
//! evaluator and the SQL lowering must agree on which rows match.
//!
//! New operators added to `mars-expr` must extend the case list here so
//! divergence between eval and lowering is caught before it reaches production.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use mars_expr::{AttributeAccess, CmpOp, Expr, Literal, LogicOp, eval};
use mars_source::{SourceBinding, SourceCollectionId};
use mars_source_postgres::{SqlParam, lower_to_sql};
use mars_types::CrsCode;
use rand::distr::{Alphanumeric, SampleString};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_postgres::types::ToSql;

#[derive(Clone)]
struct Row {
    gid: i64,
    attrs: Vec<(String, Literal)>,
}

impl AttributeAccess for Row {
    fn get(&self, name: &str) -> Option<Literal> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }
}

fn rows() -> Vec<Row> {
    vec![
        Row {
            gid: 1,
            attrs: vec![
                ("kind".into(), Literal::String("a".into())),
                ("label".into(), Literal::String("alpha".into())),
                ("weight".into(), Literal::Float(1.5)),
                ("qty".into(), Literal::Int(10)),
                ("optstr".into(), Literal::String("present".into())),
            ],
        },
        Row {
            gid: 2,
            attrs: vec![
                ("kind".into(), Literal::String("b".into())),
                ("label".into(), Literal::String("beta".into())),
                ("weight".into(), Literal::Float(2.5)),
                ("qty".into(), Literal::Int(20)),
                ("optstr".into(), Literal::Null),
            ],
        },
        Row {
            gid: 3,
            attrs: vec![
                ("kind".into(), Literal::String("a".into())),
                ("label".into(), Literal::String("alphabet".into())),
                ("weight".into(), Literal::Float(3.5)),
                ("qty".into(), Literal::Int(30)),
                ("optstr".into(), Literal::Null),
            ],
        },
        Row {
            gid: 4,
            attrs: vec![
                ("kind".into(), Literal::String("c".into())),
                ("label".into(), Literal::String("gamma".into())),
                ("weight".into(), Literal::Float(4.5)),
                ("qty".into(), Literal::Int(40)),
                ("optstr".into(), Literal::String("also-present".into())),
            ],
        },
    ]
}

fn binding() -> SourceBinding {
    SourceBinding::new(
        SourceCollectionId::new("parity"),
        "public.parity_t",
        "geom",
        "gid",
        vec![
            "kind".into(),
            "label".into(),
            "weight".into(),
            "qty".into(),
            "optstr".into(),
        ],
        CrsCode::new("EPSG:25832"),
    )
    .unwrap()
}

fn eval_matches(expr: &Expr) -> BTreeSet<i64> {
    let mut out = BTreeSet::new();
    for row in rows() {
        match eval(expr, &row) {
            Ok(Literal::Bool(true)) => {
                out.insert(row.gid);
            }
            // NULL ≡ unknown ≡ not matched in a WHERE clause (SQL three-valued logic)
            Ok(_) => {}
            Err(e) => panic!("eval failure on row {}: {e}", row.gid),
        }
    }
    out
}

fn param_box(p: &SqlParam) -> Box<dyn ToSql + Sync + Send> {
    match p {
        SqlParam::Null => Box::new(Option::<i64>::None),
        SqlParam::Bool(b) => Box::new(*b),
        SqlParam::Int(i) => Box::new(*i),
        SqlParam::Float(f) => Box::new(*f),
        SqlParam::Text(s) => Box::new(s.clone()),
    }
}

async fn sql_matches(client: &tokio_postgres::Client, expr: &Expr) -> BTreeSet<i64> {
    let (where_sql, params) = lower_to_sql(expr, &binding(), 1).unwrap();
    let sql = format!("SELECT gid FROM parity_t WHERE {where_sql} ORDER BY gid");
    let boxed: Vec<Box<dyn ToSql + Sync + Send>> = params.iter().map(param_box).collect();
    let borrowed: Vec<&(dyn ToSql + Sync)> = boxed.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();
    let rows = client.query(&sql, &borrowed).await.unwrap_or_else(|e| {
        panic!("sql `{sql}` failed: {e}");
    });
    rows.iter().map(|r| r.get::<_, i64>(0)).collect()
}

fn ident(name: &str) -> Expr {
    Expr::Ident(name.into())
}

fn lit_int(n: i64) -> Expr {
    Expr::Literal(Literal::Int(n))
}

fn lit_str(s: &str) -> Expr {
    Expr::Literal(Literal::String(s.into()))
}

fn lit_float(f: f64) -> Expr {
    Expr::Literal(Literal::Float(f))
}

/// One case per operator variant. Adding a variant to `mars_expr::Expr` /
/// `CmpOp` / `LogicOp` must add a case here; the parity check runs both eval
/// and the SQL lowering against the same row set and asserts identical
/// matching ids.
fn cases() -> Vec<(&'static str, Expr)> {
    vec![
        ("cmp_eq", Expr::Cmp { op: CmpOp::Eq, lhs: Box::new(ident("kind")), rhs: Box::new(lit_str("a")) }),
        ("cmp_ne", Expr::Cmp { op: CmpOp::Ne, lhs: Box::new(ident("kind")), rhs: Box::new(lit_str("a")) }),
        ("cmp_lt", Expr::Cmp { op: CmpOp::Lt, lhs: Box::new(ident("qty")), rhs: Box::new(lit_int(25)) }),
        ("cmp_le", Expr::Cmp { op: CmpOp::Le, lhs: Box::new(ident("qty")), rhs: Box::new(lit_int(20)) }),
        ("cmp_gt", Expr::Cmp { op: CmpOp::Gt, lhs: Box::new(ident("weight")), rhs: Box::new(lit_float(2.0)) }),
        ("cmp_ge", Expr::Cmp { op: CmpOp::Ge, lhs: Box::new(ident("weight")), rhs: Box::new(lit_float(3.5)) }),
        (
            "logic_and",
            Expr::Logic {
                op: LogicOp::And,
                args: vec![
                    Expr::Cmp { op: CmpOp::Eq, lhs: Box::new(ident("kind")), rhs: Box::new(lit_str("a")) },
                    Expr::Cmp { op: CmpOp::Ge, lhs: Box::new(ident("qty")), rhs: Box::new(lit_int(20)) },
                ],
            },
        ),
        (
            "logic_or",
            Expr::Logic {
                op: LogicOp::Or,
                args: vec![
                    Expr::Cmp { op: CmpOp::Eq, lhs: Box::new(ident("kind")), rhs: Box::new(lit_str("c")) },
                    Expr::Cmp { op: CmpOp::Lt, lhs: Box::new(ident("qty")), rhs: Box::new(lit_int(15)) },
                ],
            },
        ),
        (
            "not",
            Expr::Not(Box::new(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(ident("kind")),
                rhs: Box::new(lit_str("a")),
            })),
        ),
        (
            "in_list",
            Expr::In {
                lhs: Box::new(ident("kind")),
                list: vec![Literal::String("a".into()), Literal::String("c".into())],
            },
        ),
        (
            "in_empty",
            Expr::In {
                lhs: Box::new(ident("kind")),
                list: vec![],
            },
        ),
        ("like", Expr::Like { lhs: Box::new(ident("label")), pattern: "alpha%".into() }),
        ("is_null", Expr::IsNull(Box::new(ident("optstr")))),
        ("is_not_null", Expr::IsNotNull(Box::new(ident("optstr")))),
    ]
}

#[tokio::test]
async fn eval_and_sql_lowering_agree_across_every_operator() {
    let password = Alphanumeric.sample_string(&mut rand::rng(), 16);
    let container = GenericImage::new("postgis/postgis", "16-3.4")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", &password)
        .with_env_var("POSTGRES_USER", "mars")
        .with_env_var("POSTGRES_DB", "mars")
        .start()
        .await
        .expect("docker available");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let dsn = format!("host=127.0.0.1 port={port} user=mars password={password} dbname=mars");

    let (client, conn) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    client
        .batch_execute(
            "CREATE TABLE parity_t (
                 gid    bigint primary key,
                 kind   text,
                 label  text,
                 weight double precision,
                 qty    bigint,
                 optstr text
             );",
        )
        .await
        .unwrap();
    for row in rows() {
        let kind = match row.get("kind").unwrap() {
            Literal::String(s) => s,
            _ => unreachable!(),
        };
        let label = match row.get("label").unwrap() {
            Literal::String(s) => s,
            _ => unreachable!(),
        };
        let weight = match row.get("weight").unwrap() {
            Literal::Float(f) => f,
            _ => unreachable!(),
        };
        let qty = match row.get("qty").unwrap() {
            Literal::Int(i) => i,
            _ => unreachable!(),
        };
        let optstr: Option<String> = match row.get("optstr").unwrap() {
            Literal::String(s) => Some(s),
            Literal::Null => None,
            _ => unreachable!(),
        };
        client
            .execute(
                "INSERT INTO parity_t (gid, kind, label, weight, qty, optstr)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[&row.gid, &kind, &label, &weight, &qty, &optstr],
            )
            .await
            .unwrap();
    }

    for (name, expr) in cases() {
        let e_set = eval_matches(&expr);
        let s_set = sql_matches(&client, &expr).await;
        assert_eq!(e_set, s_set, "parity violation for case `{name}`: eval={e_set:?} sql={s_set:?}");
    }
}
