//! Where repository files live: a local directory, or an S3-compatible bucket.
//!
//! Every write is all-or-nothing: locally a temporary file is fsynced and renamed into place;
//! an S3 PUT is atomic by definition. A crash therefore never leaves a half-written object.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub trait Backend: Send + Sync {
    fn put(&self, name: &str, data: &[u8]) -> io::Result<()>;
    fn get(&self, name: &str) -> io::Result<Vec<u8>>;
    fn get_range(&self, name: &str, offset: u64, len: u64) -> io::Result<Vec<u8>>;
    /// Names under a directory (recursively), relative to the repository root.
    fn list(&self, dir: &str) -> io::Result<Vec<String>>;
    fn delete(&self, name: &str) -> io::Result<()>;
    fn exists(&self, name: &str) -> io::Result<bool>;
    fn size(&self, name: &str) -> io::Result<u64>;
    /// Whether reading a whole pack once is cheaper than many small range reads (remote stores).
    fn prefers_whole_objects(&self) -> bool {
        false
    }
}

pub fn open_backend(url: &str) -> io::Result<Arc<dyn Backend>> {
    if let Some(rest) = url.strip_prefix("s3:") {
        Ok(Arc::new(S3::from_url(rest)?))
    } else {
        Ok(Arc::new(Local::new(url)))
    }
}

// ---------------------------------------------------------------- local directory

pub struct Local {
    root: PathBuf,
    handles: RwLock<HashMap<String, Arc<File>>>,
}

impl Local {
    pub fn new(root: impl AsRef<Path>) -> Local {
        Local { root: root.as_ref().to_path_buf(), handles: RwLock::new(HashMap::new()) }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn handle(&self, name: &str) -> io::Result<Arc<File>> {
        if let Some(f) = self.handles.read().unwrap().get(name) {
            return Ok(f.clone());
        }
        let f = Arc::new(File::open(self.path(name))?);
        let mut h = self.handles.write().unwrap();
        if h.len() > 512 {
            h.clear();
        }
        h.insert(name.to_string(), f.clone());
        Ok(f)
    }
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = f.seek_read(&mut buf[done..], off)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        done += n;
        off += n as u64;
    }
    Ok(())
}

pub fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

impl Backend for Local {
    fn put(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let p = self.path(name);
        let dir = p.parent().unwrap();
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".tmp-{}", hex::encode(crate::crypto::random_bytes::<8>())));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &p)?;
        sync_dir(dir);
        Ok(())
    }

    fn get(&self, name: &str) -> io::Result<Vec<u8>> {
        fs::read(self.path(name))
    }

    fn get_range(&self, name: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let f = self.handle(name)?;
        let mut buf = vec![0u8; len as usize];
        read_at(&f, &mut buf, offset)?;
        Ok(buf)
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let mut out = vec![];
        let base = self.path(dir);
        if !base.exists() {
            return Ok(out);
        }
        let mut stack = vec![base];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d)? {
                let e = e?;
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if e.file_type()?.is_dir() {
                    stack.push(p);
                } else if !name.starts_with(".tmp-") {
                    out.push(p.strip_prefix(&self.root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        self.handles.write().unwrap().remove(name);
        match fs::remove_file(self.path(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    fn exists(&self, name: &str) -> io::Result<bool> {
        Ok(self.path(name).exists())
    }

    fn size(&self, name: &str) -> io::Result<u64> {
        Ok(fs::metadata(self.path(name))?.len())
    }
}

// ---------------------------------------------------------------- S3 (path-style, Signature V4)

pub struct S3 {
    endpoint: String, // http://host:port
    host: String,
    bucket: String,
    prefix: String,
    region: String,
    access: String,
    secret: String,
    agent: ureq::Agent,
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// RFC 3986 encoding as SigV4 wants it; `/` is kept in paths.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
/// The SigV4 `Authorization` header for a request. `headers` must include `host`,
/// `x-amz-date` and `x-amz-content-sha256`; all given headers are signed.
pub fn sigv4(method: &str, path: &str, query: &[(String, String)], headers: &[(String, String)], payload_hash: &str, region: &str, access: &str, secret: &str) -> String {
    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    q.sort();
    let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_string())).collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let creq = format!("{method}\n{}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}", uri_encode(path, true));
    let amz_date = &h.iter().find(|(k, _)| k == "x-amz-date").expect("x-amz-date").1;
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let sts = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(creq.as_bytes()));
    let k = hmac(&hmac(&hmac(&hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes()), region.as_bytes()), b"s3"), b"aws4_request");
    let sig = hex::encode(hmac(&k, sts.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

/// UTC time as `YYYYMMDDTHHMMSSZ`.
pub fn amz_date(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

fn not_found(name: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, name.to_string())
}

impl S3 {
    /// `s3:http://host:port/bucket[/prefix]`; credentials from AWS_ACCESS_KEY_ID,
    /// AWS_SECRET_ACCESS_KEY and AWS_REGION (default us-east-1).
    pub fn from_url(url: &str) -> io::Result<S3> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad S3 URL {url:?}: use s3:http://host:port/bucket[/prefix]"));
        let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
        let (host, path) = rest.split_once('/').ok_or_else(bad)?;
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty() {
            return Err(bad());
        }
        let env = |k: &str| std::env::var(k).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{k} is not set")));
        let config = ureq::Agent::config_builder().http_status_as_error(false).build();
        Ok(S3 {
            endpoint: format!("{scheme}://{host}"),
            host: host.to_string(),
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            region: std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()),
            access: env("AWS_ACCESS_KEY_ID")?,
            secret: env("AWS_SECRET_ACCESS_KEY")?,
            agent: config.into(),
        })
    }

    fn key(&self, name: &str) -> String {
        if self.prefix.is_empty() { name.to_string() } else { format!("{}/{name}", self.prefix) }
    }

    fn request(&self, method: &str, key: Option<&str>, query: &[(String, String)], body: &[u8], extra: &[(String, String)]) -> io::Result<(u16, Vec<u8>)> {
        let path = match key {
            Some(k) => format!("/{}/{}", self.bucket, k),
            None => format!("/{}", self.bucket),
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let payload = sha256_hex(body);
        let mut headers = vec![("host".to_string(), self.host.clone()), ("x-amz-date".into(), amz_date(now)), ("x-amz-content-sha256".into(), payload.clone())];
        headers.extend_from_slice(extra);
        let auth = sigv4(method, &path, query, &headers, &payload, &self.region, &self.access, &self.secret);
        let qs = query.iter().map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false))).collect::<Vec<_>>().join("&");
        let url = format!("{}{}{}", self.endpoint, uri_encode(&path, true), if qs.is_empty() { String::new() } else { format!("?{qs}") });
        let mut req = ureq::http::Request::builder().method(method).uri(&url).header("Authorization", auth);
        for (k, v) in &headers {
            if k != "host" {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        let resp = self.agent.run(req.body(body.to_vec()).map_err(io::Error::other)?).map_err(io::Error::other)?;
        let status = resp.status().as_u16();
        let mut out = vec![];
        resp.into_body().into_reader().read_to_end(&mut out)?;
        Ok((status, out))
    }

    fn check(&self, name: &str, status: u16, body: &[u8]) -> io::Result<()> {
        match status {
            200..=299 => Ok(()),
            404 => Err(not_found(name)),
            _ => Err(io::Error::other(format!("S3 {status} for {name}: {}", String::from_utf8_lossy(&body[..body.len().min(300)])))),
        }
    }
}

fn xml_values<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

impl Backend for S3 {
    fn put(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let (st, body) = self.request("PUT", Some(&self.key(name)), &[], data, &[])?;
        self.check(name, st, &body)
    }

    fn get(&self, name: &str) -> io::Result<Vec<u8>> {
        let (st, body) = self.request("GET", Some(&self.key(name)), &[], &[], &[])?;
        self.check(name, st, &body)?;
        Ok(body)
    }

    fn get_range(&self, name: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let range = vec![("range".to_string(), format!("bytes={}-{}", offset, offset + len - 1))];
        let (st, body) = self.request("GET", Some(&self.key(name)), &[], &[], &range)?;
        self.check(name, st, &body)?;
        if body.len() as u64 != len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{name}: short range read")));
        }
        Ok(body)
    }

    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        let mut out = vec![];
        let prefix = format!("{}/", self.key(dir));
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type".to_string(), "2".to_string()), ("prefix".to_string(), prefix.clone())];
            if let Some(t) = &token {
                q.push(("continuation-token".into(), t.clone()));
            }
            let (st, body) = self.request("GET", None, &q, &[], &[])?;
            self.check(dir, st, &body)?;
            let xml = String::from_utf8_lossy(&body).to_string();
            let strip = if self.prefix.is_empty() { 0 } else { self.prefix.len() + 1 };
            out.extend(xml_values(&xml, "Key").into_iter().map(|k| k[strip..].to_string()));
            token = xml_values(&xml, "NextContinuationToken").first().map(|s| s.to_string());
            if token.is_none() {
                break;
            }
        }
        out.sort();
        Ok(out)
    }

    fn delete(&self, name: &str) -> io::Result<()> {
        let (st, body) = self.request("DELETE", Some(&self.key(name)), &[], &[], &[])?;
        if st == 404 { Ok(()) } else { self.check(name, st, &body) }
    }

    fn exists(&self, name: &str) -> io::Result<bool> {
        let (st, body) = self.request("HEAD", Some(&self.key(name)), &[], &[], &[])?;
        match st {
            404 => Ok(false),
            _ => self.check(name, st, &body).map(|_| true),
        }
    }

    fn size(&self, name: &str) -> io::Result<u64> {
        Ok(self.get(name)?.len() as u64)
    }

    fn prefers_whole_objects(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GET-object example from AWS's Signature Version 4 documentation.
    #[test]
    fn sigv4_matches_the_aws_example() {
        let headers = vec![
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), sha256_hex(b"")),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = sigv4("GET", "/test.txt", &[], &headers, &sha256_hex(b""), "us-east-1", "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert!(auth.ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"), "{auth}");
        assert!(auth.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }

    #[test]
    fn dates_and_encoding() {
        assert_eq!(amz_date(1369353600), "20130524T000000Z");
        assert_eq!(amz_date(951782400), "20000229T000000Z");
        assert_eq!(uri_encode("a b/c~d+e", true), "a%20b/c~d%2Be");
        assert_eq!(xml_values("<a><Key>x</Key><Key>y/z</Key></a>", "Key"), vec!["x", "y/z"]);
    }

    #[test]
    fn local_backend() {
        let d = tempfile::tempdir().unwrap();
        let b = Local::new(d.path());
        b.put("data/ab/abcd", b"hello world").unwrap();
        b.put("snapshots/1", b"s").unwrap();
        assert_eq!(b.get_range("data/ab/abcd", 6, 5).unwrap(), b"world");
        assert_eq!(b.list("data").unwrap(), vec!["data/ab/abcd"]);
        assert!(b.exists("snapshots/1").unwrap());
        b.delete("snapshots/1").unwrap();
        b.delete("snapshots/1").unwrap();
        assert!(!b.exists("snapshots/1").unwrap());
        assert_eq!(b.list("nothing").unwrap(), Vec::<String>::new());
    }
}
