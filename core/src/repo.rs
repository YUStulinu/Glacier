//! The repository: packs of encrypted blobs, indexes, trees, snapshots and locks.
//!
//! ```text
//! config              plain JSON: format version, repository id, chunk size
//! keys/<id>           master keys sealed with a password-derived key (Argon2id)
//! data/<xx>/<pack>    packs: sealed blobs, then a sealed header listing them, then the
//!                     header's length (u32 LE); the name is the BLAKE3 hash of the file
//! index/<id>          sealed lists of (blob id, kind, pack, offset, length)
//! snapshots/<id>      sealed JSON: time, host, paths, root tree, parent, summary
//! locks/<id>          JSON: who holds the repository, shared or exclusive
//! ```
//!
//! Writes are ordered so that a crash at any point leaves a valid repository: packs first, then
//! the index naming them, then the snapshot naming the trees. A pack with no index entry is
//! unreferenced (reported by `check`, removed by `prune`), never a broken reference.

use crate::backend::Backend;
use crate::crypto::{CryptoError, Id, KeyFile, Keys};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, RwLock};

/// The blobs of one pack: (id, kind, offset, sealed length).
pub type PackEntries = Vec<(Id, u8, u32, u32)>;

pub const PACK_TARGET: usize = 16 << 20;
pub const KIND_DATA: u8 = 0;
pub const KIND_TREE: u8 = 1;

pub fn hexid(id: &Id) -> String {
    hex::encode(id)
}

pub fn parse_id(s: &str) -> io::Result<Id> {
    hex::decode(s).ok().and_then(|v| v.try_into().ok()).ok_or_else(|| invalid(&format!("bad id {s:?}")))
}

pub fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn crypto(e: CryptoError) -> io::Error {
    invalid(&e.0)
}

pub fn pack_name(id: &Id) -> String {
    let h = hexid(id);
    format!("data/{}/{}", &h[..2], h)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlobLoc {
    pub pack: Id,
    pub kind: u8,
    pub offset: u32,
    pub len: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub version: u32,
    pub id: String,
    pub chunk_avg: usize,
    /// Keyed BLAKE3 of the other fields: the file is plain JSON, but any change to it is detected
    /// once the keys are unlocked.
    #[serde(default)]
    pub mac: String,
}

impl Config {
    fn compute_mac(&self, keys: &Keys) -> String {
        let body = format!("glacier-config\n{}\n{}\n{}", self.version, self.id, self.chunk_avg);
        hexid(&keys.id_of(body.as_bytes()))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String, // "file", "dir" or "symlink"
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_ns: i64,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub files_unchanged: u64,
    pub bytes_read: u64,
    pub chunks: u64,
    pub chunks_new: u64,
    pub bytes_new: u64,
    pub stored_new: u64,
    pub errors: Vec<String>,
    pub seconds: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    #[serde(skip)]
    pub id: String,
    pub time: String,
    pub unix: f64,
    pub hostname: String,
    pub paths: Vec<String>,
    pub tree: String,
    pub parent: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub summary: Summary,
}

struct OpenPack {
    buf: Vec<u8>,
    entries: Vec<(Id, u8, u32, u32)>,
}

pub struct Repo {
    pub backend: Arc<dyn Backend>,
    pub keys: Keys,
    pub config: Config,
    pub index: RwLock<HashMap<Id, BlobLoc>>,
    /// Blobs added in this session that may not be in a finished pack yet.
    pending: Mutex<HashSet<Id>>,
    open: Mutex<OpenPack>,
    /// Index entries of packs written in this session, saved by `flush`.
    new_entries: Mutex<Vec<(Id, PackEntries)>>,
    /// Everything this session wrote, and its partial index files.
    session_entries: Mutex<Vec<(Id, PackEntries)>>,
    session_indexes: Mutex<Vec<String>>,
    pack_cache: Mutex<Vec<(Id, Arc<Vec<u8>>)>>,
    pub index_files: RwLock<Vec<String>>,
    pub pack_bytes_written: std::sync::atomic::AtomicU64,
}

fn index_encode(entries: &[(Id, PackEntries)]) -> Vec<u8> {
    let mut b = Vec::new();
    for (pack, es) in entries {
        b.extend_from_slice(pack);
        b.extend_from_slice(&(es.len() as u32).to_le_bytes());
        for (id, kind, off, len) in es {
            b.extend_from_slice(id);
            b.push(*kind);
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&len.to_le_bytes());
        }
    }
    b
}

fn index_decode(b: &[u8]) -> io::Result<Vec<(Id, PackEntries)>> {
    let mut out = vec![];
    let mut i = 0;
    let take = |i: &mut usize, n: usize| -> io::Result<&[u8]> {
        let s = b.get(*i..*i + n).ok_or_else(|| invalid("truncated index"))?;
        *i += n;
        Ok(s)
    };
    while i < b.len() {
        let pack: Id = take(&mut i, 32)?.try_into().unwrap();
        let n = u32::from_le_bytes(take(&mut i, 4)?.try_into().unwrap());
        let mut es = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let id: Id = take(&mut i, 32)?.try_into().unwrap();
            let kind = take(&mut i, 1)?[0];
            let off = u32::from_le_bytes(take(&mut i, 4)?.try_into().unwrap());
            let len = u32::from_le_bytes(take(&mut i, 4)?.try_into().unwrap());
            es.push((id, kind, off, len));
        }
        out.push((pack, es));
    }
    Ok(out)
}

/// The header of a pack: its blob list, as written at the end of the file.
pub fn pack_header(keys: &Keys, pack: &[u8]) -> io::Result<Vec<(Id, u8, u32, u32)>> {
    if pack.len() < 4 {
        return Err(invalid("pack too short"));
    }
    let hl = u32::from_le_bytes(pack[pack.len() - 4..].try_into().unwrap()) as usize;
    let start = pack.len().checked_sub(4 + hl).ok_or_else(|| invalid("bad pack header length"))?;
    let plain = keys.open(&pack[start..pack.len() - 4]).map_err(crypto)?;
    let mut es = index_decode(&[&[0u8; 32][..], &((plain.len() / 41) as u32).to_le_bytes(), &plain].concat())?;
    Ok(es.pop().map(|x| x.1).unwrap_or_default())
}

impl Repo {
    pub fn init(backend: Arc<dyn Backend>, password: &[u8], chunk_avg: usize, kdf_m_kib: u32, kdf_t: u32) -> io::Result<Repo> {
        if backend.exists("config")? {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a repository already exists there"));
        }
        let keys = Keys::generate();
        let kf = keys.to_keyfile(password, kdf_m_kib, kdf_t).map_err(crypto)?;
        let kf_json = serde_json::to_vec_pretty(&kf).unwrap();
        backend.put(&format!("keys/{}", &blake3::hash(&kf_json).to_hex()[..16]), &kf_json)?;
        let mut config = Config { version: 1, id: hex::encode(crate::crypto::random_bytes::<16>()), chunk_avg, mac: String::new() };
        config.mac = config.compute_mac(&keys);
        backend.put("config", &serde_json::to_vec_pretty(&config).unwrap())?;
        Ok(Repo::with(backend, keys, config))
    }

    fn with(backend: Arc<dyn Backend>, keys: Keys, config: Config) -> Repo {
        Repo {
            backend,
            keys,
            config,
            index: RwLock::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            open: Mutex::new(OpenPack { buf: Vec::new(), entries: vec![] }),
            new_entries: Mutex::new(vec![]),
            session_entries: Mutex::new(vec![]),
            session_indexes: Mutex::new(vec![]),
            pack_cache: Mutex::new(vec![]),
            index_files: RwLock::new(vec![]),
            pack_bytes_written: Default::default(),
        }
    }

    pub fn open(backend: Arc<dyn Backend>, password: &[u8]) -> io::Result<Repo> {
        let config: Config = serde_json::from_slice(&backend.get("config").map_err(|e| io::Error::new(e.kind(), format!("no repository: {e}")))?)
            .map_err(|e| invalid(&e.to_string()))?;
        let mut last_err = invalid("no key files");
        for k in backend.list("keys")? {
            let kf: KeyFile = match serde_json::from_slice(&backend.get(&k)?) {
                Ok(x) => x,
                Err(e) => {
                    last_err = invalid(&e.to_string());
                    continue;
                }
            };
            match Keys::from_keyfile(&kf, password) {
                Ok(keys) => {
                    if config.mac != config.compute_mac(&keys) {
                        return Err(invalid("config was modified (authentication failed)"));
                    }
                    let r = Repo::with(backend, keys, config);
                    r.load_index()?;
                    return Ok(r);
                }
                Err(e) => last_err = io::Error::new(io::ErrorKind::PermissionDenied, e.0),
            }
        }
        Err(last_err)
    }

    pub fn load_index(&self) -> io::Result<()> {
        let mut idx = HashMap::new();
        let names = self.backend.list("index")?;
        for n in &names {
            let plain = self.keys.open(&self.backend.get(n)?).map_err(|e| invalid(&format!("{n}: {e}")))?;
            for (pack, es) in index_decode(&plain)? {
                for (id, kind, offset, len) in es {
                    idx.insert(id, BlobLoc { pack, kind, offset, len });
                }
            }
        }
        *self.index.write().unwrap() = idx;
        *self.index_files.write().unwrap() = names;
        Ok(())
    }

    pub fn has_blob(&self, id: &Id) -> bool {
        self.index.read().unwrap().contains_key(id) || self.pending.lock().unwrap().contains(id)
    }

    /// Stores a blob unless the repository already has it. Returns the sealed size written
    /// (0 for a duplicate).
    pub fn add_blob(&self, id: Id, kind: u8, data: &[u8]) -> io::Result<u64> {
        if self.index.read().unwrap().contains_key(&id) || !self.pending.lock().unwrap().insert(id) {
            return Ok(0);
        }
        let sealed = self.keys.seal(data, true);
        let n = sealed.len() as u64;
        let full = {
            let mut p = self.open.lock().unwrap();
            let off = p.buf.len() as u32;
            p.buf.extend_from_slice(&sealed);
            p.entries.push((id, kind, off, sealed.len() as u32));
            if p.buf.len() >= PACK_TARGET { Some(std::mem::replace(&mut *p, OpenPack { buf: Vec::new(), entries: vec![] })) } else { None }
        };
        if let Some(p) = full {
            self.write_pack(p)?;
        }
        Ok(n)
    }

    fn write_pack(&self, mut p: OpenPack) -> io::Result<()> {
        if p.entries.is_empty() {
            return Ok(());
        }
        let mut header = Vec::with_capacity(p.entries.len() * 41);
        for (id, kind, off, len) in &p.entries {
            header.extend_from_slice(id);
            header.push(*kind);
            header.extend_from_slice(&off.to_le_bytes());
            header.extend_from_slice(&len.to_le_bytes());
        }
        let sealed = self.keys.seal(&header, false);
        p.buf.extend_from_slice(&sealed);
        p.buf.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
        let pack: Id = *blake3::hash(&p.buf).as_bytes();
        self.backend.put(&pack_name(&pack), &p.buf)?;
        self.pack_bytes_written.fetch_add(p.buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
        {
            let mut idx = self.index.write().unwrap();
            for &(id, kind, offset, len) in &p.entries {
                idx.insert(id, BlobLoc { pack, kind, offset, len });
            }
        }
        {
            let mut pend = self.pending.lock().unwrap();
            for (id, ..) in &p.entries {
                pend.remove(id);
            }
        }
        // Every 2 packs (32 MiB) a partial index is written, so an interrupted backup keeps what it
        // already uploaded and the next one does not upload it again. `flush` replaces the
        // partial indexes of a completed session with one.
        let batch = {
            let mut ne = self.new_entries.lock().unwrap();
            ne.push((pack, p.entries.clone()));
            if ne.len() >= 2 { Some(std::mem::take(&mut *ne)) } else { None }
        };
        self.session_entries.lock().unwrap().push((pack, p.entries));
        if let Some(b) = batch {
            let name = self.write_index(&b)?;
            self.session_indexes.lock().unwrap().push(name);
        }
        Ok(())
    }

    /// Writes the open pack and an index file for everything written in this session.
    pub fn flush(&self) -> io::Result<()> {
        let p = std::mem::replace(&mut *self.open.lock().unwrap(), OpenPack { buf: Vec::new(), entries: vec![] });
        self.write_pack(p)?;
        let rest = std::mem::take(&mut *self.new_entries.lock().unwrap());
        let partial = std::mem::take(&mut *self.session_indexes.lock().unwrap());
        let all = std::mem::take(&mut *self.session_entries.lock().unwrap());
        if partial.is_empty() {
            if !rest.is_empty() {
                self.write_index(&rest)?;
            }
        } else {
            // One index for the whole session, written before the partial ones are removed.
            self.write_index(&all)?;
            for n in &partial {
                self.backend.delete(n)?;
            }
            self.index_files.write().unwrap().retain(|f| !partial.contains(f));
        }
        Ok(())
    }

    pub fn write_index(&self, entries: &[(Id, PackEntries)]) -> io::Result<String> {
        let sealed = self.keys.seal(&index_encode(entries), true);
        let name = format!("index/{}", blake3::hash(&sealed).to_hex());
        self.backend.put(&name, &sealed)?;
        self.index_files.write().unwrap().push(name.clone());
        Ok(name)
    }

    /// Reads, decrypts and verifies a blob.
    pub fn load_blob(&self, id: &Id) -> io::Result<Vec<u8>> {
        let loc = *self.index.read().unwrap().get(id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("blob {} is not in the index", hexid(id))))?;
        let sealed = if self.backend.prefers_whole_objects() {
            let pack = self.cached_pack(&loc.pack)?;
            pack.get(loc.offset as usize..(loc.offset + loc.len) as usize).ok_or_else(|| invalid("blob outside its pack"))?.to_vec()
        } else {
            self.backend.get_range(&pack_name(&loc.pack), loc.offset as u64, loc.len as u64)?
        };
        let plain = self.keys.open(&sealed).map_err(|e| invalid(&format!("blob {}: {e}", hexid(id))))?;
        if self.keys.id_of(&plain) != *id {
            return Err(invalid(&format!("blob {}: content does not match its id", hexid(id))));
        }
        Ok(plain)
    }

    /// A whole pack from a small most-recently-used cache (for remote backends, where one GET of
    /// 16 MiB is far cheaper than hundreds of range requests).
    fn cached_pack(&self, pack: &Id) -> io::Result<Arc<Vec<u8>>> {
        {
            let mut c = self.pack_cache.lock().unwrap();
            if let Some(i) = c.iter().position(|(p, _)| p == pack) {
                let e = c.remove(i);
                let data = e.1.clone();
                c.push(e);
                return Ok(data);
            }
        }
        let data = Arc::new(self.backend.get(&pack_name(pack))?);
        let mut c = self.pack_cache.lock().unwrap();
        if c.len() >= 12 {
            c.remove(0);
        }
        c.push((*pack, data.clone()));
        Ok(data)
    }

    pub fn save_tree(&self, t: &Tree) -> io::Result<(Id, u64)> {
        let data = serde_json::to_vec(t).unwrap();
        let id = self.keys.id_of(&data);
        let n = self.add_blob(id, KIND_TREE, &data)?;
        Ok((id, n))
    }

    pub fn load_tree(&self, id: &str) -> io::Result<Tree> {
        let b = self.load_blob(&parse_id(id)?)?;
        serde_json::from_slice(&b).map_err(|e| invalid(&format!("tree {id}: {e}")))
    }

    pub fn save_snapshot(&self, s: &Snapshot) -> io::Result<String> {
        let sealed = self.keys.seal(&serde_json::to_vec(s).unwrap(), true);
        let id = blake3::hash(&sealed).to_hex().to_string();
        self.backend.put(&format!("snapshots/{id}"), &sealed)?;
        Ok(id)
    }

    pub fn snapshots(&self) -> io::Result<Vec<Snapshot>> {
        let mut out = vec![];
        for n in self.backend.list("snapshots")? {
            let plain = self.keys.open(&self.backend.get(&n)?).map_err(|e| invalid(&format!("{n}: {e}")))?;
            let mut s: Snapshot = serde_json::from_slice(&plain).map_err(|e| invalid(&format!("{n}: {e}")))?;
            s.id = n.trim_start_matches("snapshots/").to_string();
            out.push(s);
        }
        out.sort_by(|a, b| a.unix.total_cmp(&b.unix));
        Ok(out)
    }

    /// A snapshot by id or unique id prefix, or "latest".
    pub fn find_snapshot(&self, id: &str) -> io::Result<Snapshot> {
        let all = self.snapshots()?;
        if id == "latest" {
            return all.into_iter().last().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no snapshots"));
        }
        let m: Vec<_> = all.into_iter().filter(|s| s.id.starts_with(id)).collect();
        match m.len() {
            1 => Ok(m.into_iter().next().unwrap()),
            0 => Err(io::Error::new(io::ErrorKind::NotFound, format!("no snapshot {id}"))),
            _ => Err(invalid(&format!("{id} matches several snapshots"))),
        }
    }

    pub fn stats(&self) -> io::Result<serde_json::Value> {
        let idx = self.index.read().unwrap();
        let packs: HashSet<Id> = idx.values().map(|l| l.pack).collect();
        let stored: u64 = idx.values().map(|l| l.len as u64).sum();
        let mut pack_bytes = 0;
        for p in &packs {
            pack_bytes += self.backend.size(&pack_name(p)).unwrap_or(0);
        }
        Ok(serde_json::json!({
            "blobs": idx.len(), "data_blobs": idx.values().filter(|l| l.kind == KIND_DATA).count(),
            "tree_blobs": idx.values().filter(|l| l.kind == KIND_TREE).count(),
            "packs": packs.len(), "blob_bytes": stored, "pack_bytes": pack_bytes,
            "index_files": self.index_files.read().unwrap().len(),
        }))
    }
}

// ---------------------------------------------------------------- locks

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LockInfo {
    pub exclusive: bool,
    pub pid: u32,
    pub hostname: String,
    pub unix: u64,
}

/// A repository lock, refreshed every 5 minutes while held and considered stale after 30.
pub struct Lock {
    backend: Arc<dyn Backend>,
    name: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

pub const LOCK_STALE_SECS: u64 = 1800;

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "unknown".into())
}

impl Lock {
    pub fn acquire(backend: Arc<dyn Backend>, exclusive: bool) -> io::Result<Lock> {
        let now = now_unix();
        for n in backend.list("locks")? {
            let Ok(b) = backend.get(&n) else { continue };
            let Ok(l) = serde_json::from_slice::<LockInfo>(&b) else { continue };
            if now.saturating_sub(l.unix) > LOCK_STALE_SECS {
                continue;
            }
            if exclusive || l.exclusive {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("repository is locked by pid {} on {} ({}){}", l.pid, l.hostname, if l.exclusive { "exclusive" } else { "shared" }, " — `glacier unlock` removes stale locks"),
                ));
            }
        }
        let name = format!("locks/{}", hex::encode(crate::crypto::random_bytes::<8>()));
        let info = move |unix| serde_json::to_vec(&LockInfo { exclusive, pid: std::process::id(), hostname: hostname(), unix }).unwrap();
        backend.put(&name, &info(now))?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (backend, name, stop) = (backend.clone(), name.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut ticks = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    ticks += 1;
                    if ticks % 300 == 0 {
                        let _ = backend.put(&name, &info(now_unix()));
                    }
                }
            });
        }
        Ok(Lock { backend, name, stop })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.backend.delete(&self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Local;

    pub fn test_repo(dir: &std::path::Path) -> Repo {
        Repo::init(Arc::new(Local::new(dir)), b"pw", 4096, 64, 1).unwrap()
    }

    #[test]
    fn blobs_packs_index_reopen() {
        let d = tempfile::tempdir().unwrap();
        let r = test_repo(d.path());
        let mut ids = vec![];
        for i in 0..2000u32 {
            let data: Vec<u8> = (0..10_000).map(|k| ((k * 7 + i as usize * 13) % 251) as u8).collect();
            let id = r.keys.id_of(&data);
            r.add_blob(id, KIND_DATA, &data).unwrap();
            assert_eq!(r.add_blob(id, KIND_DATA, &data).unwrap(), 0, "duplicate stored");
            ids.push((id, data));
        }
        r.flush().unwrap();
        let r2 = Repo::open(Arc::new(Local::new(d.path())), b"pw").unwrap();
        for (id, data) in ids.iter().step_by(97) {
            assert_eq!(&r2.load_blob(id).unwrap(), data);
        }
        assert!(Repo::open(Arc::new(Local::new(d.path())), b"nope").is_err());
        assert!(Repo::init(Arc::new(Local::new(d.path())), b"pw", 4096, 64, 1).is_err());
        // Pack headers agree with the index.
        let packs = r2.backend.list("data").unwrap();
        let hdr = pack_header(&r2.keys, &r2.backend.get(&packs[0]).unwrap()).unwrap();
        assert!(!hdr.is_empty());
        let loc = r2.index.read().unwrap()[&hdr[0].0];
        assert_eq!((loc.offset, loc.len), (hdr[0].2, hdr[0].3));
    }

    #[test]
    fn locks_conflict() {
        let d = tempfile::tempdir().unwrap();
        let b: Arc<dyn Backend> = Arc::new(Local::new(d.path()));
        let s1 = Lock::acquire(b.clone(), false).unwrap();
        let _s2 = Lock::acquire(b.clone(), false).unwrap();
        assert!(Lock::acquire(b.clone(), true).is_err());
        drop(s1);
        drop(_s2);
        let x = Lock::acquire(b.clone(), true).unwrap();
        assert!(Lock::acquire(b.clone(), false).is_err());
        drop(x);
        assert!(b.list("locks").unwrap().is_empty());
    }
}
