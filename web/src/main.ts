import "./style.css";
import { crumbs, formatBytes, formatDate, fromNs, join, parseAmzDate, relativeTime, savings, sortEntries } from "./util";

interface Summary {
  files: number;
  dirs: number;
  bytes: number;
  files_unchanged: number;
  bytes_read: number;
  chunks_new: number;
  stored_new: number;
  errors: string[];
  seconds: number;
}
interface Snapshot {
  id: string;
  time: string;
  hostname: string;
  paths: string[];
  tags: string[];
  parent: string | null;
  summary: Summary;
}
interface Entry {
  name: string;
  type: string;
  size: number;
  mtime_ns: number;
  chunks: number;
  target?: string;
}
interface Job {
  name: string;
  repo: string;
  paths: string[];
  every: string;
  running: boolean;
  next_run: string;
  last_run?: { start: string; end: string; ok: boolean; error?: string; snapshot?: string; summary?: Summary };
}
interface Run {
  job: string;
  start: string;
  end: string;
  ok: boolean;
  error?: string;
  snapshot?: string;
  summary?: Summary;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);

async function get<T>(url: string): Promise<T> {
  const r = await fetch(url);
  const j = await r.json().catch(() => ({ error: r.statusText }));
  if (!r.ok) throw new Error(j.error ?? r.statusText);
  return j as T;
}

function toast(msg: string, bad = false) {
  const t = $("toast");
  t.textContent = msg;
  t.className = bad ? "bad" : "";
  t.hidden = false;
  setTimeout(() => (t.hidden = true), 6000);
}

// glacierd serves several repositories under /api/repo/<job>/; `glacier serve` alone serves one under /api/.
let jobs: Job[] = [];
let job = "";
let tab = "snapshots";
const base = () => (job ? `/api/repo/${encodeURIComponent(job)}` : "/api");

async function init() {
  try {
    const j = await get<Job[]>("/api/jobs");
    jobs = Array.isArray(j) ? j : [];
  } catch {
    jobs = []; // `glacier serve` alone: one repository, no schedules
  }
  const sel = $<HTMLSelectElement>("job");
  sel.innerHTML = jobs.map((j) => `<option>${esc(j.name)}</option>`).join("");
  sel.hidden = jobs.length === 0;
  job = jobs[0]?.name ?? "";
  sel.onchange = () => {
    job = sel.value;
    render();
  };
  renderTabs();
  render();
}

function renderTabs() {
  const tabs: [string, string][] = [["snapshots", "Snapshots"], ...(jobs.length ? [["jobs", "Schedules"] as [string, string]] : []), ["repo", "Repository"]];
  $("tabs").innerHTML = "";
  for (const [id, title] of tabs) {
    const b = document.createElement("button");
    b.textContent = title;
    b.className = id === tab ? "on" : "";
    b.onclick = () => {
      tab = id;
      renderTabs();
      render();
    };
    $("tabs").append(b);
  }
}

function render() {
  const v = $("view");
  v.innerHTML = "";
  if (tab === "jobs") jobsView(v);
  else if (tab === "repo") repoView(v);
  else snapshotsView(v);
}

// ---------------------------------------------------------------- snapshots and browsing

async function snapshotsView(v: HTMLElement) {
  v.innerHTML = `<div class="grid2"><div class="panel"><h2>Snapshots</h2><div id="snaps" class="muted">Loading…</div></div><div class="panel" id="browser"><span class="muted">Pick a snapshot to browse it.</span></div></div>`;
  let snaps: Snapshot[];
  try {
    snaps = (await get<Snapshot[]>(`${base()}/snapshots`)).reverse();
  } catch (e) {
    $("snaps").innerHTML = `<span class="bad">${esc((e as Error).message)}</span>`;
    return;
  }
  if (!snaps.length) {
    $("snaps").textContent = "No snapshots yet.";
    return;
  }
  const t = document.createElement("table");
  t.innerHTML = `<thead><tr><th>Time</th><th>Size</th><th>Added</th><th>Files</th></tr></thead>`;
  const tb = document.createElement("tbody");
  for (const s of snaps) {
    const tr = document.createElement("tr");
    tr.className = "click";
    tr.title = `${s.id}\n${s.paths.join("\n")}`;
    tr.innerHTML = `<td>${formatDate(parseAmzDate(s.time))}<div class="muted mono">${s.id.slice(0, 10)}</div></td>
      <td>${formatBytes(s.summary.bytes)}</td><td>${formatBytes(s.summary.stored_new)}<div class="muted">${savings(s.summary.bytes, s.summary.stored_new)} saved</div></td>
      <td>${s.summary.files.toLocaleString()}</td>`;
    tr.onclick = () => {
      tb.querySelectorAll("tr").forEach((x) => x.classList.remove("on"));
      tr.classList.add("on");
      browse(s, "");
    };
    tb.append(tr);
  }
  t.append(tb);
  $("snaps").innerHTML = "";
  $("snaps").append(t);
}

async function browse(s: Snapshot, path: string) {
  const box = $("browser");
  box.innerHTML = `<span class="muted">Loading…</span>`;
  let entries: Entry[];
  try {
    entries = sortEntries(await get<Entry[]>(`${base()}/snapshots/${s.id}/tree?path=${encodeURIComponent(path)}`));
  } catch (e) {
    box.innerHTML = `<span class="bad">${esc((e as Error).message)}</span>`;
    return;
  }
  box.innerHTML = "";
  const head = document.createElement("div");
  head.className = "crumbs";
  const root = document.createElement("button");
  root.className = "link";
  root.textContent = `snapshot ${s.id.slice(0, 10)}`;
  root.onclick = () => browse(s, "");
  head.append(root);
  for (const [name, p] of crumbs(path)) {
    head.append(" / ");
    const b = document.createElement("button");
    b.className = "link";
    b.textContent = name;
    b.onclick = () => browse(s, p);
    head.append(b);
  }
  box.append(head);
  const t = document.createElement("table");
  t.innerHTML = `<thead><tr><th>Name</th><th>Size</th><th>Modified</th><th></th></tr></thead>`;
  const tb = document.createElement("tbody");
  for (const e of entries) {
    const tr = document.createElement("tr");
    const p = join(path, e.name);
    tr.innerHTML = `<td>${e.type === "dir" ? "📁 " : e.type === "symlink" ? "↪ " : ""}${esc(e.name)}</td><td>${e.type === "file" ? formatBytes(e.size) : ""}</td>
      <td class="muted">${e.mtime_ns ? formatDate(fromNs(e.mtime_ns)) : ""}</td><td></td>`;
    if (e.type === "dir") {
      tr.className = "click";
      tr.onclick = () => browse(s, p);
    } else if (e.type === "file") {
      const a = document.createElement("a");
      a.href = `${base()}/snapshots/${s.id}/file?path=${encodeURIComponent(p)}`;
      a.textContent = "download";
      a.setAttribute("download", e.name);
      tr.lastElementChild!.append(a);
    }
    tb.append(tr);
  }
  t.append(tb);
  box.append(t);
  // Restore this directory to a folder on the machine running Glacier.
  const form = document.createElement("div");
  form.className = "row";
  form.innerHTML = `<span class="muted">Restore ${path ? `“${esc(path)}”` : "everything"} to</span><input id="target" size="36" placeholder="absolute path, e.g. C:\\restore" /><button class="act" id="go">Restore</button>`;
  box.append(form);
  $("go").onclick = async () => {
    const target = $<HTMLInputElement>("target").value.trim();
    if (!target) return toast("Enter a target folder.", true);
    $("go").setAttribute("disabled", "");
    try {
      const r = await fetch(`${base()}/restore`, { method: "POST", body: JSON.stringify({ snapshot: s.id, path, target }) });
      const j = await r.json();
      if (!r.ok) throw new Error(j.error);
      toast(`Restored ${j.files} files (${formatBytes(j.bytes)}) in ${j.seconds.toFixed(1)} s${j.errors.length ? `, ${j.errors.length} errors` : ""}.`, j.errors.length > 0);
    } catch (e) {
      toast((e as Error).message, true);
    } finally {
      $("go").removeAttribute("disabled");
    }
  };
}

// ---------------------------------------------------------------- schedules

async function jobsView(v: HTMLElement) {
  v.innerHTML = `<div class="panel"><h2>Scheduled backups</h2><div id="jobs"></div></div><div class="panel"><h2>Recent runs</h2><div id="runs"></div></div>`;
  const now = new Date();
  const js = await get<Job[]>("/api/jobs");
  const t = document.createElement("table");
  t.innerHTML = `<thead><tr><th>Job</th><th>Paths</th><th>Every</th><th>Last run</th><th>Next run</th><th></th></tr></thead>`;
  const tb = document.createElement("tbody");
  for (const j of js) {
    const tr = document.createElement("tr");
    const last = j.last_run
      ? `<span class="${j.last_run.ok ? "ok" : "bad"}">${j.last_run.ok ? "ok" : "failed"}</span> ${relativeTime(new Date(j.last_run.end), now)}`
      : `<span class="muted">never</span>`;
    const next = j.running ? "running…" : new Date(j.next_run).getTime() <= now.getTime() ? "now" : relativeTime(new Date(j.next_run), now);
    tr.innerHTML = `<td><strong>${esc(j.name)}</strong><div class="muted mono">${esc(j.repo)}</div></td><td class="mono">${j.paths.map(esc).join("<br>")}</td>
      <td>${esc(j.every)}</td><td>${last}</td><td>${next}</td><td></td>`;
    const b = document.createElement("button");
    b.className = "act";
    b.textContent = "Run now";
    b.onclick = async () => {
      await fetch(`/api/jobs/${encodeURIComponent(j.name)}/run`, { method: "POST" });
      toast(`Backup “${j.name}” started.`);
      setTimeout(render, 1500);
    };
    tr.lastElementChild!.append(b);
    tb.append(tr);
  }
  t.append(tb);
  $("jobs").append(t);
  const runs = await get<Run[]>("/api/runs");
  $("runs").innerHTML = runs.length
    ? `<table><thead><tr><th>Job</th><th>Started</th><th>Took</th><th>Result</th><th>Data</th><th>Stored</th></tr></thead><tbody>${runs
        .slice(0, 50)
        .map((r) => {
          const took = (new Date(r.end).getTime() - new Date(r.start).getTime()) / 1000;
          return `<tr><td>${esc(r.job)}</td><td>${formatDate(new Date(r.start))}</td><td>${took.toFixed(1)} s</td>
            <td class="${r.ok ? "ok" : "bad"}">${r.ok ? `snapshot <span class="mono">${(r.snapshot ?? "").slice(0, 10)}</span>` : esc(r.error ?? "failed")}</td>
            <td>${r.summary ? formatBytes(r.summary.bytes) : ""}</td><td>${r.summary ? formatBytes(r.summary.stored_new) : ""}</td></tr>`;
        })
        .join("")}</tbody></table>`
    : `<span class="muted">No runs yet.</span>`;
}

// ---------------------------------------------------------------- repository

async function repoView(v: HTMLElement) {
  v.innerHTML = `<div class="panel"><h2>Repository</h2><div id="stats" class="muted">Loading…</div></div>
    <div class="panel"><h2>Check</h2><p class="muted">Verifies every snapshot, tree and index entry; reading the data also decrypts and verifies every stored chunk.</p>
    <div class="row"><button class="act" id="check">Check structure</button><button class="act" id="checkdata">Check and read all data</button></div><div id="checkout"></div></div>`;
  const st = await get<Record<string, number>>(`${base()}/stats`);
  const stat = (k: string, val: string) => `<div class="stat"><div class="k">${k}</div><div class="v">${val}</div></div>`;
  $("stats").className = "stats";
  $("stats").innerHTML =
    stat("Packs", st.packs.toLocaleString()) + stat("Stored", formatBytes(st.pack_bytes)) + stat("Data chunks", st.data_blobs.toLocaleString()) + stat("Trees", st.tree_blobs.toLocaleString()) + stat("Index files", String(st.index_files));
  const run = async (data: boolean) => {
    $("checkout").innerHTML = `<p class="muted">Checking…</p>`;
    const r = await get<{ snapshots: number; trees: number; packs: number; packs_read: number; errors: string[]; unreferenced_packs: string[] }>(`${base()}/check${data ? "?read_data=1" : ""}`);
    $("checkout").innerHTML = `<p class="${r.errors.length ? "bad" : "ok"}">${r.errors.length ? `${r.errors.length} errors` : "No errors"}: ${r.snapshots} snapshots, ${r.trees} trees, ${r.packs} packs${data ? ` (${r.packs_read} read)` : ""}${r.unreferenced_packs.length ? `, ${r.unreferenced_packs.length} unreferenced packs` : ""}.</p>
      ${r.errors.slice(0, 50).map((e) => `<div class="mono bad">${esc(e)}</div>`).join("")}`;
  };
  $("check").onclick = () => run(false);
  $("checkdata").onclick = () => run(true);
}

init();
