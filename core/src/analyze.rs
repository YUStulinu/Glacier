//! Deduplication analysis without writing a repository, for the evaluation: how many bytes a set
//! of directories would need under different chunking strategies.
//!
//! Strategies: FastCDC with a given average size, fixed-size chunks, or whole files. Unique chunks
//! are counted by BLAKE3 hash; "compressed" is the sum of zstd-3 sizes of the unique chunks
//! (what Glacier would store, minus encryption overhead).

use crate::chunker::{Chunker, fixed_split};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub enum Strategy {
    Cdc(usize),
    Fixed(usize),
    WholeFile,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Analysis {
    pub strategy: String,
    pub files: u64,
    pub logical_bytes: u64,
    pub chunks: u64,
    pub unique_chunks: u64,
    pub unique_bytes: u64,
    pub unique_compressed_bytes: u64,
    pub dedup_ratio: f64,
    pub total_ratio: f64,
    pub seconds: f64,
}

pub fn walk(root: &Path, excludes: &[String], out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(root) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if excludes.iter().any(|x| x == &name) {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&e.path(), excludes, out),
            Ok(t) if t.is_file() => out.push(e.path()),
            _ => {}
        }
    }
}

pub fn analyze(roots: &[PathBuf], excludes: &[String], strategy: Strategy) -> io::Result<Analysis> {
    let t0 = std::time::Instant::now();
    let mut files = vec![];
    for r in roots {
        walk(r, excludes, &mut files);
    }
    let unique: Mutex<HashMap<[u8; 32], u64>> = Mutex::new(HashMap::new());
    let compressed = Mutex::new(0u64);
    let (logical, chunks) = (std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0));
    let chunker = match strategy {
        Strategy::Cdc(avg) => Some(Chunker::new(0x6c61_6369_6572, avg)),
        _ => None,
    };
    let add = |c: &[u8]| {
        let h = *blake3::hash(c).as_bytes();
        logical.fetch_add(c.len() as u64, std::sync::atomic::Ordering::Relaxed);
        chunks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let new = unique.lock().unwrap().insert(h, c.len() as u64).is_none();
        if new {
            let z = zstd::bulk::compress(c, 3).map(|z| z.len()).unwrap_or(c.len()).min(c.len());
            *compressed.lock().unwrap() += z as u64;
        }
        Ok(())
    };
    files.par_iter().try_for_each(|p| -> io::Result<()> {
        let Ok(f) = fs::File::open(p) else { return Ok(()) };
        let r = io::BufReader::with_capacity(1 << 20, f);
        match (&strategy, &chunker) {
            (Strategy::Cdc(_), Some(c)) => c.split(r, add),
            (Strategy::Fixed(n), _) => fixed_split(r, *n, add),
            _ => {
                let mut b = vec![];
                let mut r = r;
                r.read_to_end(&mut b)?;
                add(&b)
            }
        }
    })?;
    let u = unique.into_inner().unwrap();
    let logical = logical.into_inner();
    let ub: u64 = u.values().sum();
    let cb = compressed.into_inner().unwrap();
    Ok(Analysis {
        strategy: match strategy {
            Strategy::Cdc(a) => format!("fastcdc-{a}"),
            Strategy::Fixed(n) => format!("fixed-{n}"),
            Strategy::WholeFile => "whole-file".into(),
        },
        files: files.len() as u64,
        logical_bytes: logical,
        chunks: chunks.into_inner(),
        unique_chunks: u.len() as u64,
        unique_bytes: ub,
        unique_compressed_bytes: cb,
        dedup_ratio: logical as f64 / ub.max(1) as f64,
        total_ratio: logical as f64 / cb.max(1) as f64,
        seconds: t0.elapsed().as_secs_f64(),
    })
}
