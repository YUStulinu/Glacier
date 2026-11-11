use glacier::analyze::{self, Strategy};
use glacier::backend::open_backend;
use glacier::repo::{Lock, Repo};
use glacier::{backup, check, prune, restore, serve};
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const USAGE: &str = "usage: glacier COMMAND --repo URL [options]
  URL is a directory or s3:http://host:port/bucket[/prefix] (credentials from AWS_ACCESS_KEY_ID,
  AWS_SECRET_ACCESS_KEY). The password comes from GLACIER_PASSWORD or --password-file FILE.

  init      [--chunk-avg 64KiB] [--kdf-mem-mib 64] [--kdf-time 3]
  backup    PATH... [--exclude NAME]... [--tag T]... [--parent ID|--full] [--threads N] [--json]
  snapshots [--json]
  ls        SNAPSHOT [PATH] [--json]
  restore   SNAPSHOT --target DIR [--path P] [--threads N] [--json]
  dump      SNAPSHOT PATH                 write one file to stdout
  check     [--read-data] [--json]        exit status 1 if anything is wrong
  forget    [--keep-last N] [--keep-daily N] [--prune] [--json]
  prune     [--repack-below 0.5] [--json]
  stats
  unlock                                  remove stale locks
  serve     [--addr 127.0.0.1:8250] [--web DIR]
  analyze   PATH... [--avg BYTES | --fixed BYTES | --whole-file] [--exclude NAME]...  (no repository)";

struct Args(Vec<String>);

impl Args {
    fn flag(&self, name: &str) -> Option<String> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1).cloned())
    }
    fn all(&self, name: &str) -> Vec<String> {
        self.0.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
    }
    fn has(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
    fn positional(&self) -> Vec<String> {
        let switches = ["--json", "--read-data", "--prune", "--full", "--whole-file"];
        let mut out = vec![];
        let mut i = 0;
        while i < self.0.len() {
            if self.0[i].starts_with("--") {
                i += if switches.contains(&self.0[i].as_str()) { 1 } else { 2 };
            } else {
                out.push(self.0[i].clone());
                i += 1;
            }
        }
        out
    }
}

fn size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (num, mul) = if let Some(n) = s.strip_suffix("MiB") {
        (n, 1 << 20)
    } else if let Some(n) = s.strip_suffix("KiB") {
        (n, 1 << 10)
    } else {
        (s, 1)
    };
    num.trim().parse::<usize>().map(|n| n * mul).map_err(|_| format!("bad size {s:?}"))
}

fn password(a: &Args) -> Result<Vec<u8>, String> {
    if let Some(f) = a.flag("--password-file") {
        return std::fs::read_to_string(&f).map(|s| s.trim_end_matches(['\r', '\n']).as_bytes().to_vec()).map_err(|e| format!("{f}: {e}"));
    }
    std::env::var("GLACIER_PASSWORD").map(|s| s.into_bytes()).map_err(|_| "set GLACIER_PASSWORD or pass --password-file".into())
}

fn open(a: &Args) -> Result<Repo, String> {
    let url = a.flag("--repo").ok_or("--repo is required")?;
    let b = open_backend(&url).map_err(|e| e.to_string())?;
    Repo::open(b, &password(a)?).map_err(|e| e.to_string())
}

fn human(n: u64) -> String {
    let u = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut x = n as f64;
    let mut i = 0;
    while x >= 1024.0 && i < u.len() - 1 {
        x /= 1024.0;
        i += 1;
    }
    format!("{x:.1} {}", u[i])
}

fn run(args: Vec<String>) -> Result<i32, String> {
    let cmd = args.first().ok_or(USAGE)?.clone();
    let a = Args(args[1..].to_vec());
    let e = |e: io::Error| e.to_string();
    let as_json = a.has("--json");
    match cmd.as_str() {
        "init" => {
            let url = a.flag("--repo").ok_or("--repo is required")?;
            let avg = size(&a.flag("--chunk-avg").unwrap_or_else(|| "64KiB".into()))?;
            if !avg.is_power_of_two() || avg < 4096 {
                return Err("--chunk-avg must be a power of two, at least 4KiB".into());
            }
            let m: u32 = a.flag("--kdf-mem-mib").and_then(|x| x.parse().ok()).unwrap_or(64);
            let t: u32 = a.flag("--kdf-time").and_then(|x| x.parse().ok()).unwrap_or(3);
            let r = Repo::init(open_backend(&url).map_err(e)?, &password(&a)?, avg, m * 1024, t).map_err(e)?;
            println!("created repository {} at {url} (chunks of about {})", r.config.id, human(avg as u64));
        }
        "backup" => {
            let repo = open(&a)?;
            let paths: Vec<PathBuf> = a.positional().into_iter().map(PathBuf::from).collect();
            if paths.is_empty() {
                return Err("nothing to back up".into());
            }
            let _lock = Lock::acquire(repo.backend.clone(), false).map_err(e)?;
            let opts = backup::Options {
                excludes: a.all("--exclude"),
                tags: a.all("--tag"),
                parent: if a.has("--full") { Some(String::new()) } else { a.flag("--parent") },
                threads: a.flag("--threads").and_then(|x| x.parse().ok()),
            };
            let s = backup::backup(&repo, &paths, &opts).map_err(e)?;
            if as_json {
                println!("{}", json!({"id": s.id, "parent": s.parent, "summary": s.summary}));
            } else {
                let m = &s.summary;
                println!(
                    "snapshot {} saved: {} files, {} dirs, {} ({} unchanged files skipped)\n  read {}, {} new chunks ({} before compression, {} stored), {:.1} s",
                    &s.id[..12],
                    m.files,
                    m.dirs,
                    human(m.bytes),
                    m.files_unchanged,
                    human(m.bytes_read),
                    m.chunks_new,
                    human(m.bytes_new),
                    human(m.stored_new),
                    m.seconds
                );
                for x in &m.errors {
                    eprintln!("  warning: {x}");
                }
            }
        }
        "snapshots" => {
            let repo = open(&a)?;
            let s = repo.snapshots().map_err(e)?;
            if as_json {
                println!("{}", json!(s.iter().map(|s| { let mut v = serde_json::to_value(s).unwrap(); v["id"] = json!(s.id); v }).collect::<Vec<_>>()));
            } else {
                for s in s {
                    println!("{}  {}  {:>10}  {}  {}", &s.id[..12], s.time, human(s.summary.bytes), s.hostname, s.paths.join(", "));
                }
            }
        }
        "ls" => {
            let repo = open(&a)?;
            let pos = a.positional();
            let snap = repo.find_snapshot(pos.first().ok_or("which snapshot?")?).map_err(e)?;
            let node = restore::find_node(&repo, &snap, pos.get(1).map(|s| s.as_str()).unwrap_or("")).map_err(e)?;
            let nodes = match &node.subtree {
                Some(t) => repo.load_tree(t).map_err(e)?.nodes,
                None => vec![node],
            };
            if as_json {
                println!("{}", serde_json::to_string(&nodes).unwrap());
            } else {
                for n in nodes {
                    println!("{:7} {:>10}  {}", n.kind, if n.kind == "file" { human(n.size) } else { String::new() }, n.name);
                }
            }
        }
        "restore" => {
            let repo = open(&a)?;
            let pos = a.positional();
            let snap = repo.find_snapshot(pos.first().ok_or("which snapshot?")?).map_err(e)?;
            let target = PathBuf::from(a.flag("--target").ok_or("--target is required")?);
            let st = restore::restore(&repo, &snap, &a.flag("--path").unwrap_or_default(), &target, a.flag("--threads").and_then(|x| x.parse().ok())).map_err(e)?;
            if as_json {
                println!("{}", json!(st));
            } else {
                println!("restored {} files, {} dirs, {} in {:.1} s", st.files, st.dirs, human(st.bytes), st.seconds);
                for x in &st.errors {
                    eprintln!("  error: {x}");
                }
            }
            return Ok(if st.errors.is_empty() { 0 } else { 1 });
        }
        "dump" => {
            let repo = open(&a)?;
            let pos = a.positional();
            let snap = repo.find_snapshot(pos.first().ok_or("which snapshot?")?).map_err(e)?;
            let node = restore::find_node(&repo, &snap, pos.get(1).ok_or("which file?")?).map_err(e)?;
            let mut out = io::BufWriter::new(io::stdout().lock());
            restore::dump(&repo, &node, &mut out).map_err(e)?;
            out.flush().map_err(e)?;
        }
        "check" => {
            let repo = match open(&a) {
                Ok(r) => r,
                Err(msg) => {
                    // A repository that cannot even be opened (damaged index or key) fails the check.
                    if as_json {
                        println!("{}", json!({"errors": [msg]}));
                    } else {
                        println!("error: {msg}");
                    }
                    return Ok(1);
                }
            };
            let r = check::check(&repo, a.has("--read-data"));
            if as_json {
                println!("{}", json!(r));
            } else {
                println!("{} snapshots, {} trees, {} data chunks referenced, {} packs ({} read)", r.snapshots, r.trees, r.data_blobs_referenced, r.packs, r.packs_read);
                if !r.unreferenced_packs.is_empty() {
                    println!("{} unreferenced packs (left by an interrupted backup; `prune` removes them)", r.unreferenced_packs.len());
                }
                for x in &r.errors {
                    println!("error: {x}");
                }
                println!("{}", if r.ok() { "no errors" } else { "REPOSITORY DAMAGED" });
            }
            return Ok(if r.ok() { 0 } else { 1 });
        }
        "forget" => {
            let repo = open(&a)?;
            let _lock = Lock::acquire(repo.backend.clone(), true).map_err(e)?;
            let keep_last = a.flag("--keep-last").and_then(|x| x.parse().ok()).unwrap_or(usize::MAX);
            let keep_daily = a.flag("--keep-daily").and_then(|x| x.parse().ok()).unwrap_or(0);
            let ids = prune::forget_plan(&repo.snapshots().map_err(e)?, keep_last, keep_daily);
            prune::forget(&repo, &ids).map_err(e)?;
            let st = if a.has("--prune") { Some(prune::prune(&repo, 0.5).map_err(e)?) } else { None };
            if as_json {
                println!("{}", json!({"forgotten": ids, "prune": st}));
            } else {
                println!("forgot {} snapshots", ids.len());
                if let Some(st) = st {
                    println!("pruned: {} blobs removed, {} packs deleted, {} repacked, {} -> {}", st.blobs_removed, st.packs_deleted, st.packs_repacked, human(st.bytes_before), human(st.bytes_after));
                }
            }
        }
        "prune" => {
            let repo = open(&a)?;
            let _lock = Lock::acquire(repo.backend.clone(), true).map_err(e)?;
            let below: f64 = a.flag("--repack-below").and_then(|x| x.parse().ok()).unwrap_or(0.5);
            let st = prune::prune(&repo, below).map_err(e)?;
            if as_json {
                println!("{}", json!(st));
            } else {
                println!("{} blobs removed, {} packs deleted, {} repacked, {} -> {}", st.blobs_removed, st.packs_deleted, st.packs_repacked, human(st.bytes_before), human(st.bytes_after));
            }
        }
        "stats" => {
            let repo = open(&a)?;
            println!("{}", serde_json::to_string_pretty(&repo.stats().map_err(e)?).unwrap());
        }
        "unlock" => {
            let url = a.flag("--repo").ok_or("--repo is required")?;
            let b = open_backend(&url).map_err(e)?;
            let mut n = 0;
            for l in b.list("locks").map_err(e)? {
                b.delete(&l).map_err(e)?;
                n += 1;
            }
            println!("removed {n} locks");
        }
        "serve" => {
            let repo = open(&a)?;
            let state = Arc::new(serve::State { repo, web: a.flag("--web").map(PathBuf::from), busy: Mutex::new(()) });
            serve::serve(state, &a.flag("--addr").unwrap_or_else(|| "127.0.0.1:8250".into())).map_err(e)?;
        }
        "analyze" => {
            let roots: Vec<PathBuf> = a.positional().into_iter().map(PathBuf::from).collect();
            let strategy = if a.has("--whole-file") {
                Strategy::WholeFile
            } else if let Some(f) = a.flag("--fixed") {
                Strategy::Fixed(size(&f)?)
            } else {
                Strategy::Cdc(size(&a.flag("--avg").unwrap_or_else(|| "64KiB".into()))?)
            };
            let r = analyze::analyze(&roots, &a.all("--exclude"), strategy).map_err(e)?;
            println!("{}", serde_json::to_string(&r).unwrap());
        }
        _ => return Err(USAGE.into()),
    }
    Ok(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => std::process::exit(code),
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    }
}
