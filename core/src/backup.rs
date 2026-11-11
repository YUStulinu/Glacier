//! Backup: scan the sources, chunk new or changed files in parallel, store new chunks, build the
//! trees bottom-up and write the snapshot last.
//!
//! Incremental: with a parent snapshot, a file whose size and modification time match the
//! parent's entry is not read again; its chunk list is reused.

use crate::chunker::Chunker;
use crate::repo::{KIND_DATA, Node, Repo, Snapshot, Summary, Tree, hexid, hostname, now_unix};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Instant, UNIX_EPOCH};

#[derive(Default, Clone)]
pub struct Options {
    /// File or directory names to skip (exact names, or `*.ext`).
    pub excludes: Vec<String>,
    pub tags: Vec<String>,
    /// Parent snapshot id; `None` picks the newest snapshot of the same paths, `Some("")`
    /// forces a full read.
    pub parent: Option<String>,
    pub threads: Option<usize>,
}

enum Item {
    Dir { name: String, mtime_ns: i64, children: Vec<Item> },
    File { name: String, idx: usize },
    Symlink { name: String, target: String, mtime_ns: i64 },
}

struct FileJob {
    path: PathBuf,
    size: u64,
    mtime_ns: i64,
    readonly: bool,
    reuse: Option<Vec<String>>,
}

fn mtime_ns(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

fn excluded(name: &str, ex: &[String]) -> bool {
    ex.iter().any(|e| match e.strip_prefix('*') {
        Some(suffix) => name.ends_with(suffix),
        None => name == e,
    })
}

struct Scan<'a> {
    repo: &'a Repo,
    ex: &'a [String],
    jobs: Vec<FileJob>,
    errors: Vec<String>,
    dirs: u64,
}

impl Scan<'_> {
    /// The parent's entries for a directory, by name.
    fn parent_entries(&self, tree: Option<&str>) -> HashMap<String, Node> {
        tree.and_then(|t| self.repo.load_tree(t).ok()).map(|t| t.nodes.into_iter().map(|n| (n.name.clone(), n)).collect()).unwrap_or_default()
    }

    fn entry(&mut self, path: &Path, name: String, parent: Option<&Node>) -> Option<Item> {
        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                self.errors.push(format!("{}: {e}", path.display()));
                return None;
            }
        };
        let ft = md.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(path).map(|t| t.to_string_lossy().to_string()).unwrap_or_default();
            return Some(Item::Symlink { name, target, mtime_ns: mtime_ns(&md) });
        }
        if ft.is_dir() {
            self.dirs += 1;
            let ptree = parent.filter(|p| p.kind == "dir").and_then(|p| p.subtree.clone());
            let pentries = self.parent_entries(ptree.as_deref());
            let mut names: Vec<String> = match fs::read_dir(path) {
                Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect(),
                Err(e) => {
                    self.errors.push(format!("{}: {e}", path.display()));
                    vec![]
                }
            };
            names.sort();
            let mut children = vec![];
            for n in names {
                if excluded(&n, self.ex) {
                    continue;
                }
                if let Some(it) = self.entry(&path.join(&n), n.clone(), pentries.get(&n)) {
                    children.push(it);
                }
            }
            return Some(Item::Dir { name, mtime_ns: mtime_ns(&md), children });
        }
        let size = md.len();
        let mt = mtime_ns(&md);
        let reuse = parent.filter(|p| p.kind == "file" && p.size == size && p.mtime_ns == mt).map(|p| p.content.clone());
        self.jobs.push(FileJob { path: path.to_path_buf(), size, mtime_ns: mt, readonly: md.permissions().readonly(), reuse });
        Some(Item::File { name, idx: self.jobs.len() - 1 })
    }
}

struct Built {
    content: Vec<String>,
    ok: bool,
}

fn build(repo: &Repo, item: &Item, jobs: &[FileJob], built: &[Built], stored: &AtomicU64) -> io::Result<Option<Node>> {
    Ok(Some(match item {
        Item::Symlink { name, target, mtime_ns } => Node { name: name.clone(), kind: "symlink".into(), target: Some(target.clone()), mtime_ns: *mtime_ns, ..Default::default() },
        Item::File { name, idx } => {
            let (j, b) = (&jobs[*idx], &built[*idx]);
            if !b.ok {
                return Ok(None);
            }
            Node { name: name.clone(), kind: "file".into(), size: j.size, mtime_ns: j.mtime_ns, readonly: j.readonly, content: b.content.clone(), ..Default::default() }
        }
        Item::Dir { name, mtime_ns, children } => {
            let mut nodes = vec![];
            for c in children {
                if let Some(n) = build(repo, c, jobs, built, stored)? {
                    nodes.push(n);
                }
            }
            let (id, n) = repo.save_tree(&Tree { nodes })?;
            stored.fetch_add(n, Relaxed);
            Node { name: name.clone(), kind: "dir".into(), mtime_ns: *mtime_ns, subtree: Some(hexid(&id)), ..Default::default() }
        }
    }))
}

pub fn backup(repo: &Repo, paths: &[PathBuf], opts: &Options) -> io::Result<Snapshot> {
    let t0 = Instant::now();
    let abs: Vec<PathBuf> = paths.iter().map(std::path::absolute).collect::<io::Result<_>>()?;
    let path_strs: Vec<String> = abs.iter().map(|p| p.to_string_lossy().to_string()).collect();
    // The parent: the newest snapshot of the same paths from this host.
    let parent = match &opts.parent {
        Some(p) if p.is_empty() => None,
        Some(p) => Some(repo.find_snapshot(p)?),
        None => repo.snapshots()?.into_iter().rev().find(|s| s.paths == path_strs && s.hostname == hostname()),
    };
    let proot: HashMap<String, Node> = match &parent {
        Some(p) => repo.load_tree(&p.tree)?.nodes.into_iter().map(|n| (n.name.clone(), n)).collect(),
        None => HashMap::new(),
    };
    let mut scan = Scan { repo, ex: &opts.excludes, jobs: vec![], errors: vec![], dirs: 0 };
    let mut roots = vec![];
    let mut seen = std::collections::HashSet::new();
    for p in &abs {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string_lossy().replace([':', '\\', '/'], "_"));
        if !seen.insert(name.clone()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("two sources are both named {name:?}")));
        }
        if let Some(it) = scan.entry(p, name.clone(), proot.get(&name)) {
            roots.push(it);
        }
    }
    let chunker = Chunker::new(repo.keys.chunker_seed, repo.config.chunk_avg);
    let (bytes_read, chunks, chunks_new, bytes_new, stored) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let errors = Mutex::new(std::mem::take(&mut scan.errors));
    let work = |j: &FileJob| -> Built {
        if let Some(c) = &j.reuse
            && c.iter().all(|h| crate::repo::parse_id(h).map(|id| repo.has_blob(&id)).unwrap_or(false))
        {
            chunks.fetch_add(c.len() as u64, Relaxed);
            return Built { content: c.clone(), ok: true };
        }
        let mut content = vec![];
        let res = fs::File::open(&j.path).and_then(|f| {
            chunker.split(io::BufReader::with_capacity(1 << 20, f), |chunk| {
                let id = repo.keys.id_of(chunk);
                let n = repo.add_blob(id, KIND_DATA, chunk)?;
                bytes_read.fetch_add(chunk.len() as u64, Relaxed);
                chunks.fetch_add(1, Relaxed);
                if n > 0 {
                    chunks_new.fetch_add(1, Relaxed);
                    bytes_new.fetch_add(chunk.len() as u64, Relaxed);
                    stored.fetch_add(n, Relaxed);
                }
                content.push(hexid(&id));
                Ok(())
            })
        });
        match res {
            Ok(()) => Built { content, ok: true },
            Err(e) => {
                errors.lock().unwrap().push(format!("{}: {e}", j.path.display()));
                Built { content: vec![], ok: false }
            }
        }
    };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(opts.threads.unwrap_or(0)).build().map_err(io::Error::other)?;
    let built: Vec<Built> = pool.install(|| scan.jobs.par_iter().map(work).collect());
    // A failure to store (as opposed to read) a file is fatal: the repository write failed.
    let mut nodes = vec![];
    for r in &roots {
        if let Some(n) = build(repo, r, &scan.jobs, &built, &stored)? {
            nodes.push(n);
        }
    }
    let (root, n) = repo.save_tree(&Tree { nodes })?;
    stored.fetch_add(n, Relaxed);
    repo.flush()?;
    let files_ok: Vec<&FileJob> = scan.jobs.iter().zip(&built).filter(|(_, b)| b.ok).map(|(j, _)| j).collect();
    let summary = Summary {
        files: files_ok.len() as u64,
        dirs: scan.dirs,
        bytes: files_ok.iter().map(|j| j.size).sum(),
        files_unchanged: scan.jobs.iter().filter(|j| j.reuse.is_some()).count() as u64,
        bytes_read: bytes_read.into_inner(),
        chunks: chunks.into_inner(),
        chunks_new: chunks_new.into_inner(),
        bytes_new: bytes_new.into_inner(),
        stored_new: stored.into_inner(),
        errors: errors.into_inner().unwrap(),
        seconds: t0.elapsed().as_secs_f64(),
    };
    let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let mut snap = Snapshot {
        id: String::new(),
        time: crate::backend::amz_date(now_unix()),
        unix: now.as_secs_f64(),
        hostname: hostname(),
        paths: path_strs,
        tree: hexid(&root),
        parent: parent.map(|p| p.id),
        tags: opts.tags.clone(),
        summary,
    };
    snap.id = repo.save_snapshot(&snap)?;
    Ok(snap)
}
