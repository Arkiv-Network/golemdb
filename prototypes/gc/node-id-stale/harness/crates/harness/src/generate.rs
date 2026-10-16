//! Deterministic keys and records: same seed and stream, same operations.

use anyhow::Result;
use golemdb_api::{CellValue, RecordKey, RecordOp, op};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::Zipf;

use crate::config::{Distribution, Shape, Type};

pub struct Generator {
    cells: Vec<Cell>,
    rng: ChaCha8Rng,
    stream: u64,
    unique: u64,
}

struct Cell {
    name: String,
    attribute: bool,
    ty: Type,
    cardinality: u64,
    zipf: Option<Zipf<f64>>,
    distribution: Distribution,
    len: usize,
}

impl Generator {
    /// Each writer uses its own `stream`, so writers never generate the same key.
    pub fn new(shape: &Shape, seed: u64, stream: u64) -> Result<Self> {
        let mut cells = Vec::new();
        for (specs, attribute) in [(&shape.attributes, true), (&shape.fields, false)] {
            for spec in specs {
                for i in 0..spec.repeat {
                    let name = if spec.repeat == 1 {
                        spec.name.clone()
                    } else {
                        format!("{}_{i}", spec.name)
                    };
                    let zipf = match spec.distribution {
                        Distribution::Zipf => Some(Zipf::new(spec.cardinality as f64, 1.0)?),
                        _ => None,
                    };
                    cells.push(Cell {
                        name,
                        attribute,
                        ty: spec.ty,
                        cardinality: spec.cardinality,
                        zipf,
                        distribution: spec.distribution,
                        len: spec.len,
                    });
                }
            }
        }
        Ok(Self {
            cells,
            rng: ChaCha8Rng::seed_from_u64(mix(seed) ^ stream),
            stream,
            unique: 0,
        })
    }

    pub fn rng(&mut self) -> &mut ChaCha8Rng {
        &mut self.rng
    }

    pub fn record(&mut self, key: RecordKey) -> Result<RecordOp<op::Create>> {
        let mut op = RecordOp::create(key);
        for i in 0..self.cells.len() {
            let value = self.value(i);
            let cell = &self.cells[i];
            op = if cell.attribute {
                op.attribute(&cell.name, value)?
            } else {
                op.field(&cell.name, value)?
            };
        }
        Ok(op)
    }

    /// New value for one random cell.
    pub fn patch(&mut self, key: RecordKey) -> Result<RecordOp<op::Patch>> {
        let i = self.rng.random_range(0..self.cells.len());
        let value = self.value(i);
        let cell = &self.cells[i];
        let op = RecordOp::patch(key);
        Ok(if cell.attribute {
            op.attribute(&cell.name, value)?
        } else {
            op.field(&cell.name, value)?
        })
    }

    fn value(&mut self, i: usize) -> CellValue {
        let cell = &self.cells[i];
        let n = match (cell.distribution, &cell.zipf) {
            (Distribution::Uniform, _) => self.rng.random_range(0..cell.cardinality),
            (Distribution::Zipf, Some(zipf)) => self.rng.sample(zipf) as u64 - 1,
            _ => {
                self.unique += 1;
                (self.stream << 40) | self.unique
            }
        };
        match cell.ty {
            Type::U64 => CellValue::from_u64(n),
            Type::Str => CellValue::from_str(&format!("{n:0>width$x}", width = cell.len)),
            Type::Bytes => {
                let mut bytes = vec![0u8; cell.len];
                self.rng.fill(&mut bytes[..]);
                CellValue::from_bytes(&bytes)
            }
        }
    }
}

/// Pseudo-random 32-byte key, unique per (seed, stream, n).
pub fn key(seed: u64, stream: u64, n: u64) -> RecordKey {
    let mut state = mix(seed ^ mix(stream ^ mix(n)));
    let mut key = [0u8; 32];
    for chunk in key.chunks_exact_mut(8) {
        state = mix(state);
        chunk.copy_from_slice(&state.to_be_bytes());
    }
    RecordKey(key)
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}
