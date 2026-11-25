// 表示期間と時刻の書式（表示はすべてローカル時刻）

export const SEC = 1000;
export const MIN = 60 * SEC;
export const HOUR = 60 * MIN;
export const DAY = 24 * HOUR;

export type TimeRange = { kind: "relative"; ms: number } | { kind: "absolute"; from: number; to: number };

export const PRESETS: { label: string; ms: number }[] = [
  { label: "5分", ms: 5 * MIN },
  { label: "15分", ms: 15 * MIN },
  { label: "1時間", ms: HOUR },
  { label: "6時間", ms: 6 * HOUR },
  { label: "24時間", ms: DAY },
  { label: "7日", ms: 7 * DAY },
  { label: "30日", ms: 30 * DAY },
  { label: "90日", ms: 90 * DAY },
  { label: "1年", ms: 365 * DAY },
  { label: "5年", ms: 5 * 365 * DAY },
];

export function resolve(r: TimeRange, now = Date.now()): { from: number; to: number } {
  return r.kind === "relative" ? { from: now - r.ms, to: now } : { from: r.from, to: r.to };
}

export function rangeLabel(r: TimeRange): string {
  if (r.kind === "relative") {
    const p = PRESETS.find((p) => p.ms === r.ms);
    return `直近${p ? p.label : fmtDuration(r.ms)}`;
  }
  const a = new Date(r.from);
  const b = new Date(r.to);
  const sameDay = a.toDateString() === b.toDateString();
  const short = (d: Date) => `${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const year = a.getFullYear() !== new Date().getFullYear() ? `${a.getFullYear()}/` : "";
  return `${year}${short(a)} 〜 ${sameDay ? `${pad(b.getHours())}:${pad(b.getMinutes())}` : short(b)}`;
}

const pad = (n: number, w = 2) => String(n).padStart(w, "0");

export function fmtDateTime(ms: number | null | undefined, seconds = true): string {
  if (ms == null) return "—";
  const d = new Date(ms);
  const s = `${d.getFullYear()}/${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return seconds ? `${s}:${pad(d.getSeconds())}` : s;
}

/** <input type="datetime-local"> 用 */
export function toLocalInput(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function fromLocalInput(s: string): number | null {
  const t = new Date(s).getTime();
  return Number.isFinite(t) ? t : null;
}

export function fmtDuration(ms: number): string {
  const a = Math.abs(ms);
  if (a < MIN) return `${Math.round(a / SEC)}秒`;
  if (a < HOUR) return `${+(a / MIN).toFixed(1)}分`;
  if (a < DAY) return `${+(a / HOUR).toFixed(1)}時間`;
  return `${+(a / DAY).toFixed(1)}日`;
}

export function fmtAgo(ms: number | null | undefined, now = Date.now()): string {
  if (ms == null) return "受信なし";
  const d = now - ms;
  if (d < 2 * SEC) return "たった今";
  return `${fmtDuration(d)}前`;
}

export function fmtBytes(b: number): string {
  if (b < 1024) return `${b} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let v = b / 1024;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 2 : 1)} ${u[i]}`;
}

export function fmtNum(v: number | null | undefined, decimals?: number | null): string {
  if (v == null || !Number.isFinite(v)) return "—";
  if (decimals != null) return v.toLocaleString("ja-JP", { minimumFractionDigits: decimals, maximumFractionDigits: decimals });
  const a = Math.abs(v);
  const d = a >= 1000 ? 0 : a >= 100 ? 1 : a >= 1 ? 2 : 4;
  return v.toLocaleString("ja-JP", { maximumFractionDigits: d });
}

export const TIER_LABEL: Record<string, string> = { raw: "生データ", "1m": "1分集計", "1h": "1時間集計" };
