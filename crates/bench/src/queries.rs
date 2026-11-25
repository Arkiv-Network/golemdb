//! Running a scenario's queries between blocks.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::ops::Bound;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use golemdb_api::{
    Api, Bounds, Budget, CellNameRef, CellValue, CountOp, Database, EqValue, Filter, InValues,
    QueryOp,
};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::config::{self, Query, Rank, Scenario};
use crate::generate::{Generator, scramble};
use crate::quantile;
use crate::workloads::csv;

pub const LOG: &str = "queries.csv";
pub const HEADER: &str = "commit,records,query,matches,returned,runs,first_ms,p50_ms,p99_ms,max_ms";

/// A query stops repeating after this long, even short of `runs`.
const MAX_TIME_PER_QUERY: Duration = Duration::from_secs(10);

pub struct Runner<'a> {
    queries: &'a [Query],
    runs: usize,
    seed: u64,
    log: BufWriter<File>,
    pub rows: Vec<Row>,
}

pub struct Row {
    pub commit: u64,
    pub name: String,
    pub first_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

impl<'a> Runner<'a> {
    /// Builds every query once, so a bad filter fails before anything runs.
    pub fn new(scenario: &'a Scenario, generator: &Generator) -> Result<Self> {
        let queries = &scenario.workload.queries;
        let mut rng = ChaCha8Rng::seed_from_u64(scenario.seed);
        for query in queries {
            op(query, generator, &mut rng).map_err(|e| anyhow!("query `{}`: {e}", query.name))?;
        }
        Ok(Self {
            queries,
            runs: scenario.workload.query_runs,
            seed: scenario.seed,
            log: csv(&scenario.out.join(LOG), HEADER)?,
            rows: Vec::new(),
        })
    }

    pub fn run(
        &mut self,
        db: &Database,
        generator: &Generator,
        commit: u64,
        records: u64,
    ) -> Result<()> {
        for (i, query) in self.queries.iter().enumerate() {
            // `random` ranks from a stream of this query's own at this commit: how many runs fit
            // in the time limit must not change what any other query asks.
            let mut rng = ChaCha8Rng::seed_from_u64(scramble(
                self.seed ^ scramble(commit ^ scramble(i as u64)),
            ));
            let mut times = Vec::new();
            let (mut matches, mut returned) = (Vec::new(), Vec::new());
            let start = Instant::now();
            while times.len() < self.runs && start.elapsed() < MAX_TIME_PER_QUERY {
                // Built outside the timer: only the engine call is measured.
                let op = op(query, generator, &mut rng)?;
                let t = Instant::now();
                let (took, matched) = match op {
                    Op::Query(op) => {
                        let page = db.query(None, op).into_result()?;
                        let took = t.elapsed();
                        returned.push(page.records.len());
                        (took, page.total_matched.unwrap_or(0))
                    }
                    Op::Count(op) => {
                        let count = db.count(None, op).into_result()?;
                        (t.elapsed(), count)
                    }
                };
                times.push(took.as_secs_f64() * 1000.0);
                matches.push(matched);
            }
            // Medians, as `random` values match a different number each run.
            let (matches, returned) = (median(matches), median(returned));
            let (first, runs) = (times[0], times.len());
            let max = times.iter().copied().fold(0.0, f64::max);
            let (p50, p99) = (quantile(times.clone(), 0.5), quantile(times, 0.99));
            writeln!(
                self.log,
                "{commit},{records},\"{}\",{matches},{returned},{runs},{first:.3},{p50:.3},{p99:.3},{max:.3}",
                // Quoted: names may hold commas.
                query.name.replace('"', "\"\""),
            )?;
            self.log.flush()?;
            self.rows.push(Row {
                commit,
                name: query.name.clone(),
                first_ms: first,
                p50_ms: p50,
                p99_ms: p99,
            });
        }
        Ok(())
    }
}

enum Op {
    Query(QueryOp),
    Count(CountOp),
}

/// With fresh values for `random` ranks. Pages always ask for the total:
/// the whole match set is built anyway, so it costs nothing.
fn op(query: &Query, generator: &Generator, rng: &mut ChaCha8Rng) -> Result<Op> {
    let filter = build(&query.filter, generator, rng)?;
    if query.count {
        return Ok(Op::Count(CountOp::filter(filter, Budget::Unlimited)));
    }
    let mut op = QueryOp::from_filter(filter, Budget::Unlimited).with_total();
    if let Some(limit) = query.limit {
        op = op.limit(limit);
    }
    if let Some(offset) = query.offset {
        op = op.offset(offset);
    }
    if let Some(only) = &query.only {
        op = op.only(only);
    }
    Ok(Op::Query(op))
}

fn build(spec: &config::Filter, generator: &Generator, rng: &mut ChaCha8Rng) -> Result<Filter> {
    let field = |name: &str| CellNameRef::raw(name.as_bytes()).into();
    let mut value = |name: &str, rank: Rank| -> Result<CellValue> {
        let n = match rank {
            Rank::Fixed(n) => n,
            Rank::Random(_) => generator.random_rank(name, rng)?,
        };
        generator.value_at(name, n)
    };
    Ok(match spec {
        config::Filter::Eq(leaf) => {
            let (name, &rank) = single(leaf)?;
            Filter::Eq {
                field: field(name),
                value: EqValue::try_from(value(name, rank)?).map_err(|e| anyhow!("{e:?}"))?,
            }
        }
        config::Filter::In(leaf) => {
            let (name, ranks) = single(leaf)?;
            let values = ranks
                .iter()
                .map(|&rank| value(name, rank))
                .collect::<Result<Vec<_>>>()?;
            Filter::In {
                field: field(name),
                values: InValues::try_from(values).map_err(|e| anyhow!("{e:?}"))?,
            }
        }
        config::Filter::Range(leaf) => {
            let (name, b) = single(leaf)?;
            let mut bound =
                |included: Option<Rank>, excluded: Option<Rank>| -> Result<Bound<CellValue>> {
                    Ok(match (included, excluded) {
                        (Some(_), Some(_)) => {
                            bail!("`{name}`: a side is either inclusive or exclusive")
                        }
                        (Some(rank), None) => Bound::Included(value(name, rank)?),
                        (None, Some(rank)) => Bound::Excluded(value(name, rank)?),
                        (None, None) => Bound::Unbounded,
                    })
                };
            let (lower, upper) = (bound(b.gte, b.gt)?, bound(b.lte, b.lt)?);
            Filter::Range {
                field: field(name),
                bounds: Bounds::try_new(generator.type_of(name)?, lower, upper)
                    .map_err(|e| anyhow!("{e:?}"))?,
            }
        }
        config::Filter::Prefix(leaf) => {
            let (name, prefix) = single(leaf)?;
            generator.type_of(name)?;
            Filter::Prefix {
                field: field(name),
                prefix: prefix.clone(),
            }
        }
        config::Filter::Exists(name) => {
            generator.type_of(name)?;
            Filter::Exists {
                field: field(name),
                ty: None,
            }
        }
        config::Filter::And(children) => Filter::And(
            children
                .iter()
                .map(|c| build(c, generator, rng))
                .collect::<Result<_>>()?,
        ),
        config::Filter::Or(children) => Filter::Or(
            children
                .iter()
                .map(|c| build(c, generator, rng))
                .collect::<Result<_>>()?,
        ),
    })
}

fn median<T: Ord + Copy + Default>(mut values: Vec<T>) -> T {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or_default()
}

/// A leaf's `{ name = value }`: exactly one attribute.
fn single<V>(leaf: &std::collections::BTreeMap<String, V>) -> Result<(&str, &V)> {
    match leaf.iter().next() {
        Some((name, value)) if leaf.len() == 1 => Ok((name, value)),
        _ => bail!("a leaf names exactly one attribute"),
    }
}
