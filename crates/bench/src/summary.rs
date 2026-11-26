//! `summary.json`: the run's headline numbers, as a flat list of name, value
//! and unit. Whatever reads it needs no knowledge of the workload.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::config::Scenario;
use crate::quantile;
use crate::workloads::{Commit, Outcome};

/// A point's numbers cover this many commits ending at it (fewer in a short run).
const STRETCH: u64 = 1000;

pub const FILE: &str = "summary.json";

#[derive(Serialize)]
struct Summary<'a> {
    /// Bumped when the result files change shape.
    schema: u32,
    scenario: &'a str,
    /// The zero of `elapsed_ms` in the commit log.
    started_unix_ms: u64,
    metrics: Vec<Metric>,
}

#[derive(Serialize)]
struct Metric {
    name: String,
    value: f64,
    unit: &'static str,
}

pub fn write(scenario: &Scenario, outcome: &Outcome, started: Duration) -> Result<()> {
    let mut metrics = Vec::new();
    let mut add = |name: String, value: f64, unit: &'static str| {
        if value.is_finite() {
            metrics.push(Metric { name, value, unit });
        }
    };
    let all = &outcome.commits;
    let ops: usize = all.iter().map(|c| c.ops).sum();
    let secs = all.iter().map(|c| c.branch_ms).sum::<f64>() / 1000.0;
    add("throughput".into(), ops as f64 / secs, "ops/s");
    add("time in blocks".into(), secs, "s");

    // Read where a group of blocks ends and where the queries run.
    let mut points: Vec<u64> = scenario.workload.query_at.clone();
    let mut end = 0;
    for group in &scenario.workload.blocks {
        end += group.repeat as u64;
        points.push(end);
    }
    points.sort_unstable();
    points.dedup();
    let stretch = STRETCH.min(end / 4).max(1);
    for at in points {
        let near: Vec<&Commit> = all
            .iter()
            .filter(|c| c.no <= at && c.no + stretch > at)
            .collect();
        let values = |f: fn(&Commit) -> f64| near.iter().map(|c| f(c)).collect::<Vec<_>>();
        for (call, f) in [
            ("seal", (|c| c.seal_ms) as fn(&Commit) -> f64),
            ("commit", |c| c.commit_ms),
            ("block", |c| c.branch_ms),
        ] {
            add(
                format!("{call} p50 at commit {at}"),
                quantile(values(f), 0.5),
                "ms",
            );
            add(
                format!("{call} p99 at commit {at}"),
                quantile(values(f), 0.99),
                "ms",
            );
        }
        add(
            format!("written per commit p50 at commit {at}"),
            quantile(values(|c| c.written as f64), 0.5),
            "bytes",
        );
        add(
            format!("file size at commit {at}"),
            near.last().map_or(f64::NAN, |c| c.db_mib),
            "MiB",
        );
    }
    let written: u64 = all.iter().map(|c| c.written).sum();
    let payload: usize = all.iter().map(|c| c.bytes).sum();
    add(
        "written per payload byte".into(),
        written as f64 / payload as f64,
        "ratio",
    );
    add(
        "syncs per commit".into(),
        all.iter().map(|c| c.syncs).sum::<u64>() as f64 / all.len() as f64,
        "count",
    );

    // `file` is left to the commit log's size, which the points above use too.
    add(
        "file size".into(),
        all.last().map_or(f64::NAN, |c| c.db_mib),
        "MiB",
    );
    for (name, mib) in outcome.tables.iter().filter(|(name, _)| name != "file") {
        let outside = ["free", "other", "unused"].contains(&name.as_str());
        let kind = if outside { "space" } else { "table" };
        add(format!("{kind} {name}"), *mib, "MiB");
    }
    for row in &outcome.queries {
        let (name, at) = (&row.name, row.commit);
        add(
            format!("query `{name}` first at commit {at}"),
            row.first_ms,
            "ms",
        );
        add(
            format!("query `{name}` p50 at commit {at}"),
            row.p50_ms,
            "ms",
        );
        add(
            format!("query `{name}` p99 at commit {at}"),
            row.p99_ms,
            "ms",
        );
    }

    let summary = Summary {
        schema: 1,
        scenario: &scenario.name,
        started_unix_ms: started.as_millis() as u64,
        metrics,
    };
    let mut json = serde_json::to_string_pretty(&summary)?;
    json.push('\n');
    std::fs::write(scenario.out.join(FILE), json)?;
    Ok(())
}
