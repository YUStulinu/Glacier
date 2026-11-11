//! HTTP API for browsing and restoring (used by the web interface and the Go service).
//!
//! | Endpoint | |
//! |---|---|
//! | `GET /api/snapshots` | all snapshots, newest last |
//! | `GET /api/snapshots/<id>/tree?path=a/b` | the entries of a directory |
//! | `GET /api/snapshots/<id>/file?path=a/b/c` | a file's content (verified chunk by chunk) |
//! | `POST /api/restore` | `{"snapshot", "path", "target"}`: restore to a directory on this machine |
//! | `GET /api/stats` | repository statistics |
//! | `GET /api/check?read_data=1` | run `check` |

use crate::repo::Repo;
use crate::{check, restore};
use serde_json::json;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

type Resp = Response<io::Cursor<Vec<u8>>>;

fn json_resp(code: u16, v: serde_json::Value) -> Resp {
    Response::from_data(serde_json::to_vec(&v).unwrap()).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn err(code: u16, e: impl std::fmt::Display) -> Resp {
    json_resp(code, json!({"error": e.to_string()}))
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let h = |c: u8| (c as char).to_digit(16);
                match (h(b[i + 1]), h(b[i + 2])) {
                    (Some(x), Some(y)) => {
                        out.push((x * 16 + y) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn param(qs: &str, key: &str) -> Option<String> {
    qs.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| decode(v))
}

pub struct State {
    pub repo: Repo,
    pub web: Option<PathBuf>,
    /// Restores and checks run one at a time.
    pub busy: Mutex<()>,
}

impl State {
    fn refresh_index(&self) -> io::Result<()> {
        let names = self.repo.backend.list("index")?;
        if names != *self.repo.index_files.read().unwrap() {
            self.repo.load_index()?;
        }
        Ok(())
    }

    fn handle(&self, req: &mut tiny_http::Request) -> Resp {
        let url = req.url().to_string();
        let (path, qs) = url.split_once('?').unwrap_or((&url, ""));
        if let Err(e) = self.refresh_index() {
            return err(500, e);
        }
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (req.method(), parts.as_slice()) {
            (Method::Get, ["api", "snapshots"]) => match self.repo.snapshots() {
                Ok(s) => json_resp(200, json!(s.iter().map(|s| { let mut v = serde_json::to_value(s).unwrap(); v["id"] = json!(s.id); v }).collect::<Vec<_>>())),
                Err(e) => err(500, e),
            },
            (Method::Get, ["api", "snapshots", id, what @ ("tree" | "file")]) => {
                let snap = match self.repo.find_snapshot(id) {
                    Ok(s) => s,
                    Err(e) => return err(404, e),
                };
                let p = param(qs, "path").unwrap_or_default();
                let node = match restore::find_node(&self.repo, &snap, &p) {
                    Ok(n) => n,
                    Err(e) => return err(404, e),
                };
                if *what == "tree" {
                    let Some(sub) = &node.subtree else { return err(400, "not a directory") };
                    match self.repo.load_tree(sub) {
                        Ok(t) => json_resp(200, json!(t.nodes.iter().map(|n| json!({
                            "name": n.name, "type": n.kind, "size": n.size, "mtime_ns": n.mtime_ns, "chunks": n.content.len(), "target": n.target,
                        })).collect::<Vec<_>>())),
                        Err(e) => err(500, e),
                    }
                } else {
                    if node.kind != "file" {
                        return err(400, "not a file");
                    }
                    let mut buf = Vec::with_capacity(node.size as usize);
                    match restore::dump(&self.repo, &node, &mut buf) {
                        Ok(_) => Response::from_data(buf)
                            .with_header(Header::from_bytes("Content-Type", "application/octet-stream").unwrap())
                            .with_header(Header::from_bytes("Content-Disposition", format!("attachment; filename=\"{}\"", node.name.replace('"', ""))).unwrap()),
                        Err(e) => err(500, e),
                    }
                }
            }
            (Method::Post, ["api", "restore"]) => {
                let mut body = String::new();
                if let Err(e) = req.as_reader().take(1 << 20).read_to_string(&mut body) {
                    return err(400, e);
                }
                let v: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(e) => return err(400, e),
                };
                let (Some(s), Some(t)) = (v["snapshot"].as_str(), v["target"].as_str()) else { return err(400, "snapshot and target are required") };
                let target = PathBuf::from(t);
                if !target.is_absolute() {
                    return err(400, "target must be an absolute path");
                }
                let _g = self.busy.lock().unwrap();
                let snap = match self.repo.find_snapshot(s) {
                    Ok(s) => s,
                    Err(e) => return err(404, e),
                };
                match restore::restore(&self.repo, &snap, v["path"].as_str().unwrap_or(""), &target, None) {
                    Ok(st) => json_resp(200, json!(st)),
                    Err(e) => err(500, e),
                }
            }
            (Method::Get, ["api", "stats"]) => match self.repo.stats() {
                Ok(s) => json_resp(200, s),
                Err(e) => err(500, e),
            },
            (Method::Get, ["api", "check"]) => {
                let _g = self.busy.lock().unwrap();
                json_resp(200, json!(check::check(&self.repo, param(qs, "read_data").as_deref() == Some("1"))))
            }
            (Method::Get, ["api", ..]) => err(404, "no such endpoint"),
            (Method::Get, _) => self.static_file(path),
            _ => err(404, "not found"),
        }
    }

    fn static_file(&self, path: &str) -> Resp {
        let Some(root) = &self.web else { return err(404, "not found") };
        let rel = path.trim_start_matches('/');
        if rel.split('/').any(|c| c == "..") {
            return err(400, "bad path");
        }
        let mut p = root.join(if rel.is_empty() { "index.html" } else { rel });
        if !p.is_file() {
            p = root.join("index.html");
        }
        match std::fs::read(&p) {
            Ok(b) => {
                let ct = match p.extension().and_then(|e| e.to_str()) {
                    Some("html") => "text/html; charset=utf-8",
                    Some("js") => "text/javascript",
                    Some("css") => "text/css",
                    Some("svg") => "image/svg+xml",
                    _ => "application/octet-stream",
                };
                Response::from_data(b).with_header(Header::from_bytes("Content-Type", ct).unwrap())
            }
            Err(e) => err(404, e),
        }
    }
}

pub fn serve(state: Arc<State>, addr: &str) -> io::Result<()> {
    let server = Arc::new(Server::http(addr).map_err(|e| io::Error::other(e.to_string()))?);
    eprintln!("glacier serving on http://{addr}");
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (server, state) = (server.clone(), state.clone());
            std::thread::spawn(move || {
                for mut req in server.incoming_requests() {
                    let resp = state.handle(&mut req);
                    let _ = req.respond(resp);
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}
