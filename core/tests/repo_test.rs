//! End-to-end: backup, incremental backup, restore, check, corruption, forget and prune.

use glacier::backend::{Backend, Local};
use glacier::repo::Repo;
use glacier::{backup, check, prune, restore};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn write_tree(root: &Path, seed: u64, files: usize) {
    let mut r = fastrand::Rng::with_seed(seed);
    let text = b"the quick brown fox jumps over the lazy dog\n";
    for i in 0..files {
        let dir = root.join(format!("d{}", i % 7)).join(format!("e{}", i % 3));
        fs::create_dir_all(&dir).unwrap();
        let n = r.usize(0..60_000);
        // Text-like content that compresses, with some random bytes.
        let mut data: Vec<u8> = (0..n).map(|k| text[k % text.len()]).collect();
        for _ in 0..n / 100 {
            let k = r.usize(0..n.max(1));
            if k < data.len() {
                data[k] = r.u8(..);
            }
        }
        fs::write(dir.join(format!("f{i}.txt")), data).unwrap();
    }
    fs::write(root.join("empty"), b"").unwrap();
}

fn read_all(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), fs::read(&p).unwrap());
            }
        }
    }
    out
}

fn open(dir: &Path) -> Repo {
    Repo::open(Arc::new(Local::new(dir)), b"pw").unwrap()
}

#[test]
fn backup_restore_incremental_check() {
    let t = tempfile::tempdir().unwrap();
    let (src, repo_dir, out) = (t.path().join("src"), t.path().join("repo"), t.path().join("out"));
    write_tree(&src, 1, 120);
    let repo = Repo::init(Arc::new(Local::new(&repo_dir)), b"pw", 4096, 64, 1).unwrap();
    let s1 = backup::backup(&repo, std::slice::from_ref(&src), &Default::default()).unwrap();
    assert_eq!(s1.summary.files, 121);
    assert!(s1.summary.stored_new < s1.summary.bytes / 2, "text should compress and dedupe: {:?}", s1.summary);
    // Change one file, add one, delete one: the second backup reads only what changed.
    let changed = src.join("d0/e0/f0.txt");
    fs::write(&changed, b"new content").unwrap();
    fs::write(src.join("d1/new.txt"), b"brand new").unwrap();
    fs::remove_file(src.join("d2/e2/f2.txt")).unwrap();
    let repo = open(&repo_dir);
    let s2 = backup::backup(&repo, std::slice::from_ref(&src), &Default::default()).unwrap();
    assert_eq!(s2.parent.as_deref(), Some(s1.id.as_str()));
    assert_eq!(s2.summary.files, 121);
    assert!(s2.summary.files_unchanged >= 118, "{:?}", s2.summary);
    assert!(s2.summary.bytes_read < 100, "{:?}", s2.summary);
    // Restore both and compare.
    let repo = open(&repo_dir);
    for (s, sub) in [(&s2, "now"), (&s1, "before")] {
        let st = restore::restore(&repo, s, "", &out.join(sub), None).unwrap();
        assert!(st.errors.is_empty(), "{:?}", st.errors);
    }
    assert_eq!(read_all(&out.join("now/src")), read_all(&src));
    let before = read_all(&out.join("before/src"));
    assert!(before.contains_key("d2/e2/f2.txt") && !before.contains_key("d1/new.txt"));
    // Modification times survive.
    let m = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
    assert_eq!(m(&out.join("now/src/d0/e0/f0.txt")), m(&changed));
    // A single subtree.
    restore::restore(&repo, &s2, "src/d3", &out.join("one"), None).unwrap();
    assert_eq!(read_all(&out.join("one/d3")).len(), read_all(&src.join("d3")).len());
    let r = check::check(&repo, true);
    assert!(r.ok(), "{:?}", r.errors);
    assert_eq!(r.snapshots, 2);
}

#[test]
fn corruption_is_detected_and_never_restored() {
    let t = tempfile::tempdir().unwrap();
    let (src, repo_dir) = (t.path().join("src"), t.path().join("repo"));
    write_tree(&src, 2, 40);
    let repo = Repo::init(Arc::new(Local::new(&repo_dir)), b"pw", 4096, 64, 1).unwrap();
    let s = backup::backup(&repo, std::slice::from_ref(&src), &Default::default()).unwrap();
    let b = Local::new(&repo_dir);
    let mut files: Vec<String> = ["data", "index", "snapshots", "keys"].iter().flat_map(|d| b.list(d).unwrap()).collect();
    files.push("config".into());
    files.sort();
    let mut rng = fastrand::Rng::with_seed(4);
    for f in files.iter().cycle().take(files.len() * 3) {
        let path = repo_dir.join(f);
        let orig = fs::read(&path).unwrap();
        let mut bad = orig.clone();
        let k = rng.usize(..bad.len());
        bad[k] ^= 1 << rng.u8(0..8);
        fs::write(&path, &bad).unwrap();
        let detected = match Repo::open(Arc::new(Local::new(&repo_dir)), b"pw") {
            Err(_) => true, // a damaged index stops the open
            Ok(r) => {
                let rep = check::check(&r, true);
                if f.starts_with("data/") {
                    // Restoring must not produce wrong bytes: every file is either right or reported.
                    let out = t.path().join(format!("out{k}"));
                    if let Ok(st) = restore::restore(&r, &s, "", &out, None) {
                        let got = read_all(&out.join("src"));
                        let want = read_all(&src);
                        for (name, data) in &got {
                            assert_eq!(Some(data), want.get(name), "{name} restored with wrong content");
                        }
                        assert!(got.len() == want.len() || !st.errors.is_empty());
                    }
                }
                !rep.ok()
            }
        };
        assert!(detected, "a flipped bit in {f} at {k} was not detected");
        fs::write(&path, &orig).unwrap();
    }
}

#[test]
fn interrupted_backup_leaves_a_valid_repository_and_prune_cleans_up() {
    let t = tempfile::tempdir().unwrap();
    let (src, repo_dir) = (t.path().join("src"), t.path().join("repo"));
    write_tree(&src, 3, 60);
    let repo = Repo::init(Arc::new(Local::new(&repo_dir)), b"pw", 4096, 64, 1).unwrap();
    let s1 = backup::backup(&repo, std::slice::from_ref(&src), &Default::default()).unwrap();
    // A backup that wrote packs and died before its index and snapshot.
    let mut rng = fastrand::Rng::with_seed(5);
    let noise: Vec<u8> = (0..20_000_000).map(|_| rng.u8(..)).collect(); // incompressible: fills a pack
    {
        let r = open(&repo_dir);
        for c in noise.chunks(50_000) {
            r.add_blob(r.keys.id_of(c), glacier::repo::KIND_DATA, c).unwrap();
        }
        // Dropped without flush: full packs were written, no index names them.
    }
    let r = open(&repo_dir);
    let rep = check::check(&r, true);
    assert!(rep.ok(), "{:?}", rep.errors);
    assert!(!rep.unreferenced_packs.is_empty());
    // Forget the first snapshot after a second one, then prune.
    for i in 0..30 {
        fs::remove_file(src.join(format!("d{}/e{}/f{i}.txt", i % 7, i % 3))).unwrap();
    }
    let s2 = backup::backup(&r, std::slice::from_ref(&src), &Default::default()).unwrap();
    prune::forget(&r, std::slice::from_ref(&s1.id)).unwrap();
    let st = prune::prune(&r, 0.8).unwrap();
    assert!(st.blobs_removed > 0 && st.packs_deleted >= rep.unreferenced_packs.len(), "{st:?}");
    let r = open(&repo_dir);
    let rep = check::check(&r, true);
    assert!(rep.ok() && rep.unreferenced_packs.is_empty(), "{rep:?}");
    let out = t.path().join("out");
    restore::restore(&r, &s2, "", &out, None).unwrap();
    assert_eq!(read_all(&out.join("src")), read_all(&src));
}

#[test]
fn forget_policy() {
    let mk = |id: &str, unix: f64| glacier::repo::Snapshot {
        id: id.into(),
        time: String::new(),
        unix,
        hostname: String::new(),
        paths: vec![],
        tree: String::new(),
        parent: None,
        tags: vec![],
        summary: Default::default(),
    };
    let day = 86400.0;
    let snaps = vec![mk("a", 0.0), mk("b", 100.0), mk("c", day), mk("d", 2.0 * day), mk("e", 2.0 * day + 5.0)];
    let mut f = prune::forget_plan(&snaps, 1, 2);
    f.sort();
    assert_eq!(f, vec!["a", "b", "d"]); // keeps e (last) and the newest of the last two days (e, c)
}
