// parqlog の HTTP API クライアント
import { isTauri, native } from "./tauri";

export type SeriesId = number;

export interface Series {
  id: SeriesId;
  key: string;
  measurement: string;
  field: string;
  tags: Record<string, string>;
  unit: string | null;
  created_at: number;
  last_ts: number | null;
  last_value: number | null;
}

export interface SeriesAgg {
  id: SeriesId;
  t: number[];
  min: (number | null)[];
  max: (number | null)[];
  avg: (number | null)[];
}

export type Tier = "raw" | "1m" | "1h";

export interface AggResult {
  tier: Tier;
  bucket_ms: number;
  band: boolean;
  from_ms: number;
  to_ms: number;
  series: SeriesAgg[];
  elapsed_ms: number;
}

export interface Latest {
  id: SeriesId;
  ts: number;
  value: number;
}

export interface Device {
  device: string;
  series_count: number;
  last_ts: number | null;
  age_ms: number | null;
  stale: boolean;
}

export interface JobState {
  name: string;
  last_run: number | null;
  last_ok: number | null;
  last_work: number | null;
  last_duration_ms: number | null;
  last_error: string | null;
  runs: number;
}

export interface Status {
  version: string;
  now: number;
  uptime_s: number;
  ingest: {
    points_per_sec: number;
    received_points: number;
    written_points: number;
    rejected_old: number;
    rejected_future: number;
    parse_errors: number;
    skipped_fields: number;
    buffer_len: number;
    write_errors: number;
    last_write_error: string | null;
  };
  store: {
    series_count: number;
    raw_hot_rows: number;
    r1m_hot_rows: number;
    r1h_rows: number;
    db_file_bytes: number;
    watermarks: Record<string, number>;
    jobs: JobState[];
    written_points: number;
    rejected_points: number;
  };
  disk: { data_bytes: number; raw_bytes: number; m1_bytes: number; file_count: number; free_bytes: number; total_bytes: number };
  devices: { total: number; stale: number };
  warnings: string[];
  config: {
    listen: string;
    ingest_listen: string;
    data_dir: string;
    stale_seconds: number;
    device_tag: string;
    config_path: string;
    log_dir: string;
  };
}

export interface DashboardInfo {
  id: string;
  name: string;
  updated_at: number;
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

let base = "";

const DASH_KEY = "parqlog.dashboards";
function dashStore(): Record<string, any> {
  try {
    return JSON.parse(localStorage.getItem(DASH_KEY) ?? "{}") ?? {};
  } catch {
    return {};
  }
}

/** API の URL を決める: Tauri は設定ファイルから、ブラウザは配信元（開発時は ?api= か既定値） */
export async function initApi(): Promise<string> {
  const q = new URLSearchParams(location.search).get("api");
  if (q) base = q.replace(/\/$/, "");
  else if (isTauri) base = await native.apiBase();
  else if (location.port === "1420" || location.port === "5173") base = "http://127.0.0.1:8086";
  else base = location.origin;
  return base;
}

export function apiBase(): string {
  return base;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(base + path, init);
  } catch (e) {
    if ((e as Error).name === "AbortError") throw e;
    throw new ApiError(0, "サービスに接続できません");
  }
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      const j = await res.json();
      if (j?.message) msg = j.message;
    } catch {
      /* 本文なし */
    }
    throw new ApiError(res.status, msg);
  }
  if (res.status === 204) return undefined as T;
  return res.json() as Promise<T>;
}

const json = (method: string, body: unknown): RequestInit => ({
  method,
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(body),
});

export const api = {
  query: (p: { series: SeriesId[]; from: number; to: number; width: number; tier?: Tier | "auto" }, signal?: AbortSignal) =>
    req<AggResult>(
      `/api/query?series=${p.series.join(",")}&from=${Math.floor(p.from)}&to=${Math.ceil(p.to)}&width=${Math.max(10, Math.round(p.width))}&tier=${p.tier ?? "auto"}`,
      { signal },
    ),
  series: (params: Record<string, string> = {}) => req<Series[]>(`/api/series?${new URLSearchParams(params)}`),
  setUnit: (id: SeriesId, unit: string | null) => req<void>(`/api/series/${id}`, json("PUT", { unit })),
  /** 系列キー（名前）を変える。データはそのまま */
  renameSeries: (id: SeriesId, key: string) => req<Series>(`/api/series/${id}`, json("PUT", { key })),
  deleteSeries: (id: SeriesId) => req<void>(`/api/series/${id}`, { method: "DELETE" }),
  latest: (ids: SeriesId[] = []) => req<Latest[]>(`/api/latest?series=${ids.join(",")}`),
  devices: () => req<Device[]>("/api/devices"),
  status: () => req<Status>("/api/status"),
  // ダッシュボードは画面側（このアプリ）に保存する。本格的な可視化は Grafana で行う
  dashboards: async (): Promise<DashboardInfo[]> =>
    Object.entries(dashStore())
      .map(([id, d]: [string, any]) => ({ id, name: d?.name ?? id, updated_at: d?.updated_at ?? 0 }))
      .sort((a, b) => a.name.localeCompare(b.name, "ja")),
  dashboard: async (id: string): Promise<any> => {
    const d = dashStore()[id];
    if (!d) throw new ApiError(404, "ダッシュボードがありません");
    return d;
  },
  saveDashboard: async (id: string, body: any): Promise<void> => {
    const all = dashStore();
    all[id] = { ...body, updated_at: Date.now() };
    localStorage.setItem(DASH_KEY, JSON.stringify(all));
  },
  deleteDashboard: async (id: string): Promise<void> => {
    const all = dashStore();
    delete all[id];
    localStorage.setItem(DASH_KEY, JSON.stringify(all));
  },
  backup: (dest: string) => req<any>("/api/backup", json("POST", { dest })),
  config: () => req<{ path: string; config: any }>("/api/config"),
  saveConfig: (cfg: unknown) => req<{ saved: boolean; restart_required: boolean }>("/api/config", json("PUT", cfg)),
  /** ブラウザからの CSV 取り込み（Tauri では Rust 側がファイルを送る） */
  importBlob: (file: Blob, params: Record<string, string>) =>
    req<any>(`/api/import/csv?${new URLSearchParams(params)}`, { method: "POST", headers: { "Content-Type": "text/csv" }, body: file }),
  /** ブラウザでのエクスポート（Tauri では Rust 側がファイルへ直接書く） */
  exportBlob: async (body: unknown): Promise<{ blob: Blob; name: string }> => {
    const res = await fetch(base + "/api/export", json("POST", body));
    if (!res.ok) {
      const j = await res.json().catch(() => null);
      throw new ApiError(res.status, j?.message ?? res.statusText);
    }
    const cd = res.headers.get("Content-Disposition") ?? "";
    const name = /filename="([^"]+)"/.exec(cd)?.[1] ?? "export";
    return { blob: await res.blob(), name };
  },
};
