// Pure helpers for the interface (unit-tested).

export function formatBytes(n: number): string {
  if (!Number.isFinite(n)) return "–";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let x = n;
  while (Math.abs(x) >= 1024 && i < units.length - 1) {
    x /= 1024;
    i++;
  }
  return i === 0 ? `${x} B` : `${x.toFixed(x >= 100 ? 0 : 1)} ${units[i]}`;
}

/** Snapshot times are stored as `YYYYMMDDTHHMMSSZ`. */
export function parseAmzDate(s: string): Date {
  const m = s.match(/^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/);
  if (!m) return new Date(NaN);
  return new Date(Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]));
}

export function formatDate(d: Date): string {
  if (Number.isNaN(d.getTime())) return "–";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export function fromNs(ns: number): Date {
  return new Date(ns / 1e6);
}

/** Path segments for breadcrumbs: "a/b/c" -> [["a","a"],["b","a/b"],["c","a/b/c"]]. */
export function crumbs(path: string): [string, string][] {
  const parts = path.split("/").filter(Boolean);
  return parts.map((p, i) => [p, parts.slice(0, i + 1).join("/")]);
}

export function join(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

/** How much smaller a snapshot made the repository grow than the data it covers. */
export function savings(bytes: number, storedNew: number): string {
  if (bytes <= 0) return "–";
  const pct = 100 * (1 - storedNew / bytes);
  return `${pct.toFixed(pct >= 99.95 ? 2 : 1)}%`;
}

/** Sort a directory listing: directories first, then by name. */
export function sortEntries<T extends { name: string; type: string }>(entries: T[]): T[] {
  return [...entries].sort((a, b) => (a.type === "dir") === (b.type === "dir") ? a.name.localeCompare(b.name) : a.type === "dir" ? -1 : 1);
}

export function relativeTime(target: Date, now: Date): string {
  const s = Math.round((target.getTime() - now.getTime()) / 1000);
  const a = Math.abs(s);
  const v = a < 60 ? `${a} s` : a < 3600 ? `${Math.round(a / 60)} min` : a < 86400 ? `${Math.round(a / 3600)} h` : `${Math.round(a / 86400)} d`;
  return s >= 0 ? `in ${v}` : `${v} ago`;
}
