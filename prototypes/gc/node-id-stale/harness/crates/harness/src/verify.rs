//! Verification is outside the measured branch time. Full scans run in a
//! separate correctness campaign so they do not warm benchmark data caches.
use std::{io::Write, sync::{Mutex,OnceLock}};
use anyhow::Result;
use golemdb_storage::{Store,scan_prefix,Table};
use golemdb_merkle::{HashProvider,Keccak256Hasher};

fn hex(bytes:&[u8])->String {bytes.iter().map(|b|format!("{b:02x}")).collect()}
pub fn audit(final_scan:bool)->Result<()> {
    let store=crate::BENCH_STORE.get().unwrap();
    let tx=store.begin_read()?;
    let head=golemdb_branch::read_head_state(&tx)?;
    static LOG:OnceLock<Mutex<std::fs::File>>=OnceLock::new();
    let log=LOG.get_or_init(||Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open("results/roots.csv").unwrap()));
    writeln!(log.lock().unwrap(),"{},{},{}",head.commit_id.get(),hex(&head.state_root),hex(&head.index_root))?;
    if std::env::var_os("BENCH_AUDIT").is_some() || final_scan {
        let (nodes,counts)=(0usize,0usize); // Native control: graph checked by its own existing suite.
        let mut state=Vec::new();
        let mut cells=0usize;
        for row in scan_prefix(&tx,Table("Cell"),vec![])? {
            let(k,v)=row?;
            state.extend_from_slice(&(k.len() as u64).to_be_bytes());state.extend_from_slice(&k);
            state.extend_from_slice(&(v.len() as u64).to_be_bytes());state.extend_from_slice(&v);cells+=1;
        }
        let digest=Keccak256Hasher.hash(&state);
        let index=golemdb_index::Index::new(&Keccak256Hasher);
        let mut postings=Vec::new();let mut terms=0usize;
        for row in scan_prefix(&tx,Table("Index"),vec![])? {
            let(k,_)=row?;let term=golemdb_index::IndexTerm::decode(&k)?;
            let bitmap=index.bitmap(&tx,&term)?.expect("term row");
            postings.extend_from_slice(&(k.len() as u64).to_be_bytes());postings.extend_from_slice(&k);
            postings.extend_from_slice(&bitmap.treemap().len().to_be_bytes());
            for id in bitmap.treemap().iter() {postings.extend_from_slice(&id.to_be_bytes());}
            terms+=1;
        }
        let index_digest=Keccak256Hasher.hash(&postings);
        let mut out=std::fs::OpenOptions::new().create(true).append(true).open("results/audit.csv")?;
        writeln!(out,"{},{},{},{},{},{},{},{}",head.commit_id.get(),nodes,counts,cells,terms,hex(&digest),hex(&index_digest),if final_scan {"final"}else{"checkpoint"})?;
        if final_scan {
            let mut out=std::fs::File::create("results/physical.csv")?;
            writeln!(out,"table,rows,key_bytes,value_bytes")?;
            for table in ["CellTrie","IndexTrie","BitmapTrie","BitmapContainer","Stale", "NodeRefs","GcReleases","Roots","Cell","Index"] {
                let (mut rows,mut keys,mut values)=(0u64,0u64,0u64);
                for row in scan_prefix(&tx,Table(table),vec![])? {let(k,v)=row?;rows+=1;keys+=k.len()as u64;values+=v.len()as u64;}
                writeln!(out,"{table},{rows},{keys},{values}")?;
            }
        }
    }
    Ok(())
}
