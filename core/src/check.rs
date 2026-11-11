//! Repository verification.
//!
//! Structure (always): every index file and snapshot decrypts; every tree reachable from a
//! snapshot decrypts and matches its id; every chunk a file refers to is in the index; every pack
//! the index names exists.
//!
//! Data (`read_data`): every pack is read in full; its name must equal the BLAKE3 hash of its
//! content, its header must decrypt and agree with the index, and every blob must decrypt and
//! match its id. Any modified byte fails one of these checks.

use crate::crypto::Id;
use crate::repo::{BlobLoc, KIND_TREE, Repo, hexid, pack_header, pack_name, parse_id};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub snapshots: usize,
    pub trees: usize,
    pub data_blobs_referenced: usize,
    pub packs: usize,
    pub packs_read: usize,
    pub unreferenced_packs: Vec<String>,
    pub errors: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn check(repo: &Repo, read_data: bool) -> Report {
    let mut r = Report::default();
    // Index files were decrypted when the repository was opened; re-read to report each one.
    for n in repo.backend.list("index").unwrap_or_default() {
        match repo.backend.get(&n).map_err(|e| e.to_string()).and_then(|b| repo.keys.open(&b).map_err(|e| e.to_string())) {
            Ok(_) => {}
            Err(e) => r.errors.push(format!("{n}: {e}")),
        }
    }
    let index: HashMap<Id, BlobLoc> = repo.index.read().unwrap().clone();
    let snaps = match repo.snapshots() {
        Ok(s) => s,
        Err(e) => {
            // One bad snapshot should not hide the others: check them one by one.
            r.errors.push(e.to_string());
            let mut ok = vec![];
            for n in repo.backend.list("snapshots").unwrap_or_default() {
                if let Ok(b) = repo.backend.get(&n) {
                    match repo.keys.open(&b) {
                        Ok(p) => {
                            if let Ok(mut s) = serde_json::from_slice::<crate::repo::Snapshot>(&p) {
                                s.id = n.trim_start_matches("snapshots/").into();
                                ok.push(s);
                            }
                        }
                        Err(e) => r.errors.push(format!("{n}: {e}")),
                    }
                }
            }
            ok
        }
    };
    r.snapshots = snaps.len();
    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut data: HashSet<String> = HashSet::new();
    let mut stack: Vec<(String, String)> = snaps.iter().map(|s| (s.tree.clone(), format!("snapshot {}", &s.id[..12.min(s.id.len())]))).collect();
    while let Some((t, at)) = stack.pop() {
        if !seen_trees.insert(t.clone()) {
            continue;
        }
        match repo.load_tree(&t) {
            Ok(tree) => {
                for n in tree.nodes {
                    let here = format!("{at}/{}", n.name);
                    if let Some(s) = n.subtree {
                        stack.push((s, here.clone()));
                    }
                    for c in n.content {
                        let present = parse_id(&c).map(|id| index.contains_key(&id)).unwrap_or(false);
                        if !present {
                            r.errors.push(format!("{here}: chunk {} missing from the index", &c[..12]));
                        }
                        data.insert(c);
                    }
                }
            }
            Err(e) => r.errors.push(format!("{at}: tree {}: {e}", &t[..12.min(t.len())])),
        }
    }
    r.trees = seen_trees.len();
    r.data_blobs_referenced = data.len();
    let referenced_packs: HashSet<Id> = index.values().map(|l| l.pack).collect();
    r.packs = referenced_packs.len();
    let existing: HashSet<String> = repo.backend.list("data").unwrap_or_default().into_iter().collect();
    for p in &referenced_packs {
        if !existing.contains(&pack_name(p)) {
            r.errors.push(format!("pack {} is in the index but missing", &hexid(p)[..12]));
        }
    }
    let referenced_names: HashSet<String> = referenced_packs.iter().map(pack_name).collect();
    r.unreferenced_packs = existing.iter().filter(|n| !referenced_names.contains(*n)).cloned().collect();
    if read_data {
        let mut by_pack: HashMap<Id, Vec<(Id, BlobLoc)>> = HashMap::new();
        for (id, l) in &index {
            by_pack.entry(l.pack).or_default().push((*id, *l));
        }
        let errs = Mutex::new(vec![]);
        let packs: Vec<_> = by_pack.into_iter().filter(|(p, _)| existing.contains(&pack_name(p))).collect();
        r.packs_read = packs.len();
        packs.par_iter().for_each(|(p, blobs)| {
            let name = pack_name(p);
            let err = |m: String| errs.lock().unwrap().push(m);
            let buf = match repo.backend.get(&name) {
                Ok(b) => b,
                Err(e) => return err(format!("{name}: {e}")),
            };
            if blake3::hash(&buf).as_bytes() != p {
                err(format!("pack {}: content does not match its name (modified)", &hexid(p)[..12]));
            }
            match pack_header(&repo.keys, &buf) {
                Ok(h) => {
                    let hs: HashMap<Id, (u32, u32)> = h.iter().map(|e| (e.0, (e.2, e.3))).collect();
                    for (id, l) in blobs {
                        if hs.get(id) != Some(&(l.offset, l.len)) {
                            err(format!("pack {}: header disagrees with the index for blob {}", &hexid(p)[..12], &hexid(id)[..12]));
                        }
                    }
                }
                Err(e) => err(format!("pack {}: header: {e}", &hexid(p)[..12])),
            }
            for (id, l) in blobs {
                let Some(sealed) = buf.get(l.offset as usize..(l.offset + l.len) as usize) else {
                    err(format!("pack {}: blob {} out of range", &hexid(p)[..12], &hexid(id)[..12]));
                    continue;
                };
                match repo.keys.open(sealed) {
                    Ok(plain) if repo.keys.id_of(&plain) == *id => {}
                    Ok(_) => err(format!("blob {} ({}): content does not match its id", &hexid(id)[..12], if l.kind == KIND_TREE { "tree" } else { "data" })),
                    Err(e) => err(format!("blob {} in pack {}: {e}", &hexid(id)[..12], &hexid(p)[..12])),
                }
            }
        });
        r.errors.extend(errs.into_inner().unwrap());
    }
    r.errors.sort();
    r.errors.dedup();
    r
}
