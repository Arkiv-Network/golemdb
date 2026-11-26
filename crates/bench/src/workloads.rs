//! The workload: one writer committing blocks of creates, updates and deletes.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use golemdb_api::{Api, Budget, Database, RecordKey, RecordOp, SealInfo, op};
use golemdb_storage::Store;
use golemdb_storage_mdbx::MdbxStore;
use rand::Rng;

use crate::config::{Mix, Scenario};
use crate::generate::{Generator, key};
use crate::queries::{self, Runner};
use crate::{quantile, tables};

pub const COMMITS: &str = "commits.csv";
pub const COMMITS_HEADER: &str = "commit,elapsed_ms,ops,bytes,seal_ms,commit_ms,branch_ms,db_mib,bytes_written,syncs,state_root,index_root";
pub const TABLES: &str = "tables.csv";

/// A line of progress this often, so a long run can be followed.
const PROGRESS: Duration = Duration::from_secs(30);

pub struct Outcome {
    pub commits: Vec<Commit>,
    /// Each table's MiB after the last commit.
    pub tables: Vec<(String, f64)>,
    pub queries: Vec<queries::Row>,
}

pub struct Commit {
    pub no: u64,
    pub ops: usize,
    /// Cell values plus a 32-byte key per operation.
    pub bytes: usize,
    pub seal_ms: f64,
    pub commit_ms: f64,
    pub branch_ms: f64,
    pub db_mib: f64,
    /// What MDBX wrote to the file for this commit, from its page counters.
    pub written: u64,
    pub syncs: u64,
}

enum Op {
    Create(RecordOp<op::Create>),
    Patch(RecordOp<op::Patch>),
    Delete(RecordOp<op::Delete>),
}

struct Timing {
    seal: Duration,
    commit: Duration,
    sealed: SealInfo,
}

fn run_branch(db: &Database, ops: Vec<Op>) -> Result<Timing> {
    let branch = db.begin()?;
    for op in ops {
        match op {
            Op::Create(op) => db.create(branch, op).into_result().map(drop)?,
            Op::Patch(op) => db.patch(branch, op).into_result()?,
            Op::Delete(op) => db.delete(branch, op).into_result()?,
        }
    }
    let start = Instant::now();
    let sealed = db.seal(branch)?;
    let seal = start.elapsed();
    let start = Instant::now();
    db.commit(branch)?;
    Ok(Timing {
        seal,
        commit: start.elapsed(),
        sealed,
    })
}

/// Keys committed so far, to patch and delete.
struct Pool {
    seed: u64,
    next: u64,
    live: Vec<RecordKey>,
}

struct Plan {
    ops: Vec<Op>,
    bytes: usize,
    created: Vec<RecordKey>,
    deleted: Vec<usize>,
}

impl Pool {
    fn plan(&mut self, generator: &mut Generator, n: usize, mix: &Mix) -> Plan {
        let total = mix.create + mix.patch + mix.delete;
        let mut used = HashSet::new();
        generator.take_bytes();
        let mut plan = Plan {
            ops: Vec::with_capacity(n),
            bytes: 0,
            created: Vec::new(),
            deleted: Vec::new(),
        };
        for _ in 0..n {
            let roll = generator.rng().random_range(0..total);
            // Patch/delete need a committed key not already used in this branch.
            let target = (roll >= mix.create && !self.live.is_empty())
                .then(|| generator.rng().random_range(0..self.live.len()))
                .filter(|i| used.insert(*i));
            plan.ops.push(match target {
                Some(i) if roll < mix.create + mix.patch => {
                    Op::Patch(generator.patch(self.live[i]))
                }
                Some(i) => {
                    plan.deleted.push(i);
                    Op::Delete(RecordOp::delete(self.live[i], Budget::Unlimited))
                }
                None => {
                    let k = key(self.seed, self.next);
                    self.next += 1;
                    plan.created.push(k);
                    Op::Create(generator.record(k))
                }
            });
        }
        plan.bytes = generator.take_bytes() + 32 * plan.ops.len();
        plan
    }

    fn committed(&mut self, created: Vec<RecordKey>, mut deleted: Vec<usize>) {
        deleted.sort_unstable_by(|a, b| b.cmp(a)); // descending keeps swap_remove indexes valid
        for i in deleted {
            self.live.swap_remove(i);
        }
        self.live.extend(created);
    }
}

pub fn run(db: &Database, store: &MdbxStore, scenario: &Scenario) -> Result<Outcome> {
    let workload = &scenario.workload;
    let mut generator = Generator::new(&scenario.shape, scenario.seed)?;
    let mut pool = Pool {
        seed: scenario.seed,
        next: 0,
        live: Vec::new(),
    };
    let mut runner = Runner::new(scenario, &generator)?;
    let mut log = csv(&scenario.out.join(COMMITS), COMMITS_HEADER)?;
    let mut table_log = csv(
        &scenario.out.join(TABLES),
        &format!("commit,{}", tables::HEADER),
    )?;
    let db_dir = scenario.db_dir.as_path();
    let storage = || -> Result<_> {
        store
            .stats()?
            .ok_or_else(|| anyhow!("MDBX reports no storage stats"))
    };
    let total: usize = workload.blocks.iter().map(|b| b.repeat).sum();
    let mut commits: Vec<Commit> = Vec::with_capacity(total);
    let mut before = storage()?;
    let mut tables_at = 0;
    let started = Instant::now();
    let (mut reported, mut reported_at) = (Instant::now(), 0);
    let mut commit_no = 0;
    for spec in &workload.blocks {
        let mix = spec.mix.as_ref().unwrap_or(&workload.mix);
        for _ in 0..spec.repeat {
            // Generating records is harness work, outside the block's time.
            let plan = pool.plan(&mut generator, spec.ops, mix);
            commit_no += 1;
            let block_start = Instant::now();
            let timing = run_branch(db, plan.ops)?;
            let took = block_start.elapsed();
            // Read between commits, outside every timer; nothing else writes in between.
            let after = storage()?;
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let commit = Commit {
                no: commit_no,
                ops: spec.ops,
                bytes: plan.bytes,
                seal_ms: ms(timing.seal),
                commit_ms: ms(timing.commit),
                branch_ms: ms(took),
                db_mib: crate::dir_size(db_dir) as f64 / (1 << 20) as f64,
                written: after.session_bytes_written - before.session_bytes_written,
                syncs: after.session_syncs - before.session_syncs,
            };
            before = after;
            writeln!(
                log,
                "{commit_no},{:.0},{},{},{:.3},{:.3},{:.3},{:.1},{},{},{},{}",
                ms(started.elapsed()),
                commit.ops,
                commit.bytes,
                commit.seal_ms,
                commit.commit_ms,
                commit.branch_ms,
                commit.db_mib,
                commit.written,
                commit.syncs,
                hex(&timing.sealed.state_root),
                hex(&timing.sealed.index_root)
            )?;
            // Flushed every commit, so a run that is stopped keeps what it measured.
            log.flush()?;
            commits.push(commit);
            if commit_no % scenario.table_log_every == 0 {
                for line in tables::snapshot(db_dir)? {
                    writeln!(table_log, "{commit_no},{line}")?;
                }
                table_log.flush()?;
                tables_at = commit_no;
            }
            pool.committed(plan.created, plan.deleted);
            // Queries run between commits, outside every block's time.
            if workload.query_at.contains(&commit_no) {
                let start = Instant::now();
                runner.run(db, &generator, commit_no, pool.live.len() as u64)?;
                println!(
                    "  queries after commit {commit_no}: {} records, {:.0} s",
                    pool.live.len(),
                    start.elapsed().as_secs_f64()
                );
            }
            if reported.elapsed() >= PROGRESS || commit_no as usize == total {
                let recent = &commits[reported_at..];
                println!(
                    "  commit {commit_no} of {total}  {:.1} min  file {:.1} GiB  block p50 {:.1} ms over the last {}",
                    started.elapsed().as_secs_f64() / 60.0,
                    recent.last().map_or(0.0, |c| c.db_mib / 1024.0),
                    quantile(recent.iter().map(|c| c.branch_ms).collect(), 0.5),
                    recent.len()
                );
                (reported, reported_at) = (Instant::now(), commits.len());
            }
        }
    }
    // Taken now, as `table_log_every` may skip the last commit.
    let last = tables::snapshot(db_dir)?;
    if tables_at != commit_no {
        for line in &last {
            writeln!(table_log, "{commit_no},{line}")?;
        }
        table_log.flush()?;
    }
    // "name,entries,depth,pages,mib" -> (name, mib)
    let tables = last
        .iter()
        .filter_map(|line| line.split_once(',').zip(line.rsplit_once(',')))
        .filter_map(|((name, _), (_, mib))| Some((name.to_owned(), mib.parse().ok()?)))
        .collect();
    Ok(Outcome {
        commits,
        tables,
        queries: runner.rows,
    })
}

pub fn csv(path: &Path, header: &str) -> Result<std::io::BufWriter<std::fs::File>> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(file, "{header}")?;
    file.flush()?;
    Ok(file)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
