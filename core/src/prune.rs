//! Forgetting snapshots and removing data no snapshot needs.
//!
//! `prune` computes the blobs still referenced, then:
//! - packs with no referenced blob are deleted;
//! - packs where referenced blobs make up less than `repack_below` of the bytes are rewritten
//!   with only those blobs;
//! - one new index is written for all remaining blobs before the old index files are deleted,
//!   and old packs are deleted last.
//!
//! A crash at any point leaves a valid repository: until the new index exists, the old one
//! still describes everything; afterwards, leftover old packs are only unreferenced.

use crate::crypto::Id;
use crate::repo::{BlobLoc, Repo, Snapshot, pack_name, parse_id};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;

#[derive(Debug, Default, serde::Serialize)]
pub struct PruneStats {
    pub blobs_kept: usize,
    pub blobs_removed: usize,
    pub packs_deleted: usize,
    pub packs_repacked: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Snapshots to forget under a policy: keep the newest `keep_last`, and the newest of each of the
/// last `keep_daily` days that have snapshots.
pub fn forget_plan(snaps: &[Snapshot], keep_last: usize, keep_daily: usize) -> Vec<String> {
    let mut sorted: Vec<&Snapshot> = snaps.iter().collect();
    sorted.sort_by(|a, b| b.unix.total_cmp(&a.unix));
    let mut keep: HashSet<&str> = sorted.iter().take(keep_last).map(|s| s.id.as_str()).collect();
    let mut days = BTreeMap::new();
    for s in &sorted {
        days.entry(std::cmp::Reverse((s.unix / 86400.0).floor() as i64)).or_insert(s.id.as_str());
    }
    keep.extend(days.values().take(keep_daily).copied());
    sorted.iter().filter(|s| !keep.contains(s.id.as_str())).map(|s| s.id.clone()).collect()
}

pub fn forget(repo: &Repo, ids: &[String]) -> io::Result<()> {
    for id in ids {
        repo.backend.delete(&format!("snapshots/{id}"))?;
    }
    Ok(())
}

fn referenced(repo: &Repo) -> io::Result<HashSet<Id>> {
    let mut used = HashSet::new();
    let mut stack: Vec<String> = repo.snapshots()?.into_iter().map(|s| s.tree).collect();
    while let Some(t) = stack.pop() {
        let id = parse_id(&t)?;
        if !used.insert(id) {
            continue;
        }
        for n in repo.load_tree(&t)?.nodes {
            if let Some(s) = n.subtree {
                stack.push(s);
            }
            for c in n.content {
                used.insert(parse_id(&c)?);
            }
        }
    }
    Ok(used)
}

pub fn prune(repo: &Repo, repack_below: f64) -> io::Result<PruneStats> {
    let used = referenced(repo)?;
    let index: HashMap<Id, BlobLoc> = repo.index.read().unwrap().clone();
    let mut packs: HashMap<Id, (u64, u64)> = HashMap::new(); // (used bytes, total bytes)
    for (id, l) in &index {
        let e = packs.entry(l.pack).or_default();
        e.1 += l.len as u64;
        if used.contains(id) {
            e.0 += l.len as u64;
        }
    }
    let mut st = PruneStats { bytes_before: packs.values().map(|p| p.1).sum(), ..Default::default() };
    let unreferenced_files: Vec<String> = {
        let named: HashSet<String> = packs.keys().map(pack_name).collect();
        repo.backend.list("data")?.into_iter().filter(|n| !named.contains(n)).collect()
    };
    let delete: HashSet<Id> = packs.iter().filter(|(_, (u, _))| *u == 0).map(|(p, _)| *p).collect();
    let repack: HashSet<Id> = packs.iter().filter(|(_, (u, t))| *u > 0 && (*u as f64) < repack_below * *t as f64).map(|(p, _)| *p).collect();
    // Copy the live blobs out of the packs being rewritten.
    for (id, l) in &index {
        if repack.contains(&l.pack) && used.contains(id) {
            let data = repo.load_blob(id)?;
            // Remove from the in-memory index first so add_blob stores a new copy.
            repo.index.write().unwrap().remove(id);
            repo.add_blob(*id, l.kind, &data)?;
        }
    }
    repo.flush()?; // writes the new packs (and a small index for them)
    // One index for everything that stays.
    let final_index: HashMap<Id, BlobLoc> =
        repo.index.read().unwrap().iter().filter(|(id, l)| used.contains(*id) && !delete.contains(&l.pack) && !repack.contains(&l.pack)).map(|(k, v)| (*k, *v)).collect();
    let mut by_pack: HashMap<Id, Vec<(Id, u8, u32, u32)>> = HashMap::new();
    for (id, l) in &final_index {
        by_pack.entry(l.pack).or_default().push((*id, l.kind, l.offset, l.len));
    }
    let old_index_files = repo.index_files.read().unwrap().clone();
    let entries: Vec<_> = by_pack.into_iter().collect();
    let new_index = repo.write_index(&entries)?;
    for f in old_index_files.iter().filter(|f| **f != new_index) {
        repo.backend.delete(f)?;
    }
    for p in delete.iter().chain(repack.iter()) {
        repo.backend.delete(&pack_name(p))?;
    }
    for n in &unreferenced_files {
        repo.backend.delete(n)?;
    }
    st.packs_deleted = delete.len() + unreferenced_files.len();
    st.packs_repacked = repack.len();
    st.blobs_kept = final_index.len();
    st.blobs_removed = index.len().saturating_sub(final_index.len());
    st.bytes_after = final_index.values().map(|l| l.len as u64).sum();
    *repo.index.write().unwrap() = final_index;
    *repo.index_files.write().unwrap() = vec![new_index];
    Ok(st)
}
