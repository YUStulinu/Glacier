//! Restore and browsing: walk a snapshot's trees, fetch and verify each chunk, write the files.

use crate::repo::{Node, Repo, Snapshot, parse_id};
use rayon::prelude::*;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// The node at `path` ("a/b/c", "" for the root) of a snapshot.
pub fn find_node(repo: &Repo, snap: &Snapshot, path: &str) -> io::Result<Node> {
    let mut node = Node { name: String::new(), kind: "dir".into(), subtree: Some(snap.tree.clone()), ..Default::default() };
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let sub = node.subtree.clone().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path}: not a directory")))?;
        node = repo
            .load_tree(&sub)?
            .nodes
            .into_iter()
            .find(|n| n.name == part)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path}: no such file or directory")))?;
    }
    Ok(node)
}

/// Writes a file's content to `out`, verifying every chunk.
pub fn dump(repo: &Repo, node: &Node, out: &mut dyn Write) -> io::Result<u64> {
    let mut n = 0;
    for c in &node.content {
        let b = repo.load_blob(&parse_id(c)?)?;
        out.write_all(&b)?;
        n += b.len() as u64;
    }
    if n != node.size {
        return Err(crate::repo::invalid(&format!("{}: restored {n} bytes, expected {}", node.name, node.size)));
    }
    Ok(n)
}

#[derive(Debug, Default, serde::Serialize)]
pub struct RestoreStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: Vec<String>,
    pub seconds: f64,
}

fn collect(repo: &Repo, node: &Node, at: &Path, files: &mut Vec<(PathBuf, Node)>, dirs: &mut Vec<(PathBuf, i64)>, links: &mut Vec<(PathBuf, String)>) -> io::Result<()> {
    match node.kind.as_str() {
        "dir" => {
            dirs.push((at.to_path_buf(), node.mtime_ns));
            if let Some(t) = &node.subtree {
                for c in repo.load_tree(t)?.nodes {
                    if c.name.is_empty() || c.name == "." || c.name == ".." || c.name.contains(['/', '\\']) {
                        return Err(crate::repo::invalid(&format!("unsafe name {:?} in a tree", c.name)));
                    }
                    collect(repo, &c, &at.join(&c.name), files, dirs, links)?;
                }
            }
        }
        "file" => files.push((at.to_path_buf(), node.clone())),
        "symlink" => links.push((at.to_path_buf(), node.target.clone().unwrap_or_default())),
        k => return Err(crate::repo::invalid(&format!("unknown node type {k}"))),
    }
    Ok(())
}

fn set_mtime(path: &Path, ns: i64) {
    if ns <= 0 {
        return;
    }
    let t = UNIX_EPOCH + Duration::from_nanos(ns as u64);
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_times(fs::FileTimes::new().set_modified(t));
    }
}

/// Restores `path` of a snapshot ("" for everything) under `target`. Files that fail
/// verification are reported and removed, never left with wrong content.
pub fn restore(repo: &Repo, snap: &Snapshot, path: &str, target: &Path, threads: Option<usize>) -> io::Result<RestoreStats> {
    let t0 = Instant::now();
    let node = find_node(repo, snap, path)?;
    let base = if node.name.is_empty() { target.to_path_buf() } else { target.join(&node.name) };
    let (mut files, mut dirs, mut links) = (vec![], vec![], vec![]);
    collect(repo, &node, &base, &mut files, &mut dirs, &mut links)?;
    // Restore in storage order (by the pack and offset of each file's first chunk), so that
    // neighbouring files read neighbouring data.
    {
        let idx = repo.index.read().unwrap();
        let key = |n: &Node| n.content.first().and_then(|c| parse_id(c).ok()).and_then(|id| idx.get(&id).map(|l| (l.pack, l.offset))).unwrap_or(([0; 32], 0));
        files.sort_by_cached_key(|(_, n)| key(n));
    }
    for (d, _) in &dirs {
        fs::create_dir_all(d)?;
    }
    let bytes = AtomicU64::new(0);
    let errors = Mutex::new(vec![]);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads.unwrap_or(0)).build().map_err(io::Error::other)?;
    pool.install(|| {
        files.par_iter().for_each(|(p, n)| {
            let r = (|| -> io::Result<u64> {
                let mut f = io::BufWriter::with_capacity(1 << 20, fs::File::create(p)?);
                let k = dump(repo, n, &mut f)?;
                f.flush()?;
                Ok(k)
            })();
            match r {
                Ok(k) => {
                    bytes.fetch_add(k, Relaxed);
                    set_mtime(p, n.mtime_ns);
                    if n.readonly
                        && let Ok(m) = fs::metadata(p)
                    {
                        let mut perm = m.permissions();
                        perm.set_readonly(true);
                        let _ = fs::set_permissions(p, perm);
                    }
                }
                Err(e) => {
                    let _ = fs::remove_file(p);
                    errors.lock().unwrap().push(format!("{}: {e}", p.display()));
                }
            }
        })
    });
    for (p, t) in &links {
        #[cfg(unix)]
        let r = std::os::unix::fs::symlink(t, p);
        #[cfg(windows)]
        let r = std::os::windows::fs::symlink_file(t, p);
        if let Err(e) = r {
            errors.lock().unwrap().push(format!("{}: symlink: {e}", p.display()));
        }
    }
    // Directory times last, after their contents were written.
    for (d, ns) in dirs.iter().rev() {
        if *ns > 0
            && let Ok(f) = fs::File::open(d)
        {
            let _ = f.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_nanos(*ns as u64)));
        }
    }
    Ok(RestoreStats { files: files.len() as u64, dirs: dirs.len() as u64, bytes: bytes.into_inner(), errors: errors.into_inner().unwrap(), seconds: t0.elapsed().as_secs_f64() })
}
