use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use golemdb_api::{CellLimits, HashAlgorithm};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub seed: u64,
    pub database: Database,
    pub shape: Shape,
    /// `tables.csv` gets a reading every N commits; each costs a process.
    #[serde(default = "default_table_log_every")]
    pub table_log_every: u64,
    pub workload: Workload,
    #[serde(skip)]
    pub out: PathBuf,
    /// Deleted and recreated for every run.
    #[serde(skip)]
    pub db_dir: PathBuf,
}

impl Scenario {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        let scenario: Self = toml::from_str(&text).with_context(|| path.display().to_string())?;
        if scenario.workload.queries.is_empty() != scenario.workload.query_at.is_empty() {
            bail!("{}: `queries` and `query_at` go together", path.display());
        }
        if scenario.table_log_every == 0 {
            bail!("{}: `table_log_every` must be at least 1", path.display());
        }
        let workload = &scenario.workload;
        let commits: u64 = workload.blocks.iter().map(|b| b.repeat as u64).sum();
        if workload.blocks.iter().any(|b| b.ops == 0 || b.repeat == 0) {
            bail!(
                "{}: blocks need `ops` and `repeat` of at least 1",
                path.display()
            );
        }
        let mixes = workload.blocks.iter().filter_map(|b| b.mix.as_ref());
        if mixes
            .chain([&workload.mix])
            .any(|m| m.create + m.patch + m.delete == 0)
        {
            bail!("{}: a `mix` needs some weight", path.display());
        }
        if workload.query_at.iter().any(|&at| at == 0 || at > commits) {
            bail!(
                "{}: `query_at` must be within the {commits} commits",
                path.display()
            );
        }
        if !workload.queries.is_empty() && workload.query_runs == 0 {
            bail!("{}: `query_runs` must be at least 1", path.display());
        }
        let mut names: Vec<&str> = workload.queries.iter().map(|q| q.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("{}: two queries share a name", path.display());
        }
        for cell in scenario
            .shape
            .attributes
            .iter()
            .chain(&scenario.shape.fields)
        {
            if cell.cardinality == 0 {
                bail!(
                    "{}: `{}`: `cardinality` must be at least 1",
                    path.display(),
                    cell.name
                );
            }
            if let Len::Ranges(ranges) = &cell.len
                && (ranges.iter().map(|r| r.weight).sum::<u32>() == 0
                    || ranges.iter().any(|r| r.min > r.max))
            {
                bail!(
                    "{}: `{}`: `len` ranges need min <= max and some weight",
                    path.display(),
                    cell.name
                );
            }
        }
        Ok(scenario)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub hash: HashAlgorithm,
    pub limits: CellLimits,
    #[serde(default = "default_map_size_gib")]
    pub max_map_size_gib: u64,
    /// Commits whose roots stay readable; nothing is pruned when absent.
    pub pruning_window: Option<u64>,
}

fn default_map_size_gib() -> u64 {
    64
}

/// Every generated record. A patch rewrites every cell except `unique`
/// ones, like Arkiv's update.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    #[serde(default)]
    pub attributes: Vec<Cell>,
    #[serde(default)]
    pub fields: Vec<Cell>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    /// Distinct values; ignored by `unique`.
    #[serde(default = "default_cardinality")]
    pub cardinality: u64,
    #[serde(default)]
    pub distribution: Distribution,
    /// Bytes of a `str` or `bytes` value.
    #[serde(default = "default_len")]
    pub len: Len,
}

fn default_cardinality() -> u64 {
    1000
}
fn default_len() -> Len {
    Len::Fixed(32)
}

/// A fixed length, or weighted ranges drawn uniformly, e.g.
/// `[{ min = 1024, max = 4096, weight = 90 }, { min = 4096, max = 102400, weight = 10 }]`.
#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Len {
    Fixed(usize),
    Ranges(Vec<LenRange>),
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LenRange {
    pub min: usize,
    pub max: usize,
    pub weight: u32,
}
fn one() -> usize {
    1
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Type {
    U64,
    Str,
    /// Field-only.
    Bytes,
}

#[derive(Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    #[default]
    Uniform,
    /// A few hot values hold most records.
    Zipf,
    /// Never repeats: one posting list per record.
    Unique,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub blocks: Vec<Blocks>,
    #[serde(default)]
    pub mix: Mix,
    /// Run after each commit in `query_at` (1-based), `query_runs` times each.
    #[serde(default)]
    pub queries: Vec<Query>,
    #[serde(default)]
    pub query_at: Vec<u64>,
    #[serde(default = "default_runs")]
    pub query_runs: usize,
}

fn default_runs() -> usize {
    50
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub name: String,
    pub filter: Filter,
    /// Count the matches instead of returning a page.
    #[serde(default)]
    pub count: bool,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    /// Return only these cells of each record.
    #[serde(default)]
    pub only: Option<Vec<String>>,
}

/// Values are ranks, not literals (see `Generator::value_at`), so a filter
/// means the same at any database size. Only `prefix` takes a literal.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Filter {
    Eq(BTreeMap<String, Rank>),
    In(BTreeMap<String, Vec<Rank>>),
    Range(BTreeMap<String, Bounds>),
    Prefix(BTreeMap<String, String>),
    Exists(String),
    And(Vec<Filter>),
    Or(Vec<Filter>),
}

/// A missing side is unbounded.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    pub gt: Option<Rank>,
    pub gte: Option<Rank>,
    pub lt: Option<Rank>,
    pub lte: Option<Rank>,
}

/// A value's rank, or `"random"`: drawn per run from the attribute's distribution.
#[derive(Deserialize, Clone, Copy)]
#[serde(untagged)]
pub enum Rank {
    Fixed(u64),
    Random(Random),
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Random {
    Random,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocks {
    pub ops: usize,
    #[serde(default = "one")]
    pub repeat: usize,
    #[serde(default)]
    pub mix: Option<Mix>,
}

/// Relative weights of the operation kinds. A patch or delete with no record
/// left to target becomes a create.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mix {
    #[serde(default)]
    pub create: u32,
    #[serde(default)]
    pub patch: u32,
    #[serde(default)]
    pub delete: u32,
}

fn default_table_log_every() -> u64 {
    100
}

impl Default for Mix {
    fn default() -> Self {
        Self {
            create: 1,
            patch: 0,
            delete: 0,
        }
    }
}
