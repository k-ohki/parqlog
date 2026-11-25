// アプリ全体の状態（Svelte 5 runes）
import { api, ApiError, type Series, type SeriesId, type Status } from "./api";

export type Page = "dashboard" | "series" | "devices" | "service" | "settings" | "export";

interface Toast {
  id: number;
  text: string;
  kind: "info" | "error";
}

export const app = $state({
  page: "dashboard" as Page,
  connected: true,
  connectionError: "",
  status: null as Status | null,
  series: [] as Series[],
  seriesById: {} as Record<number, Series>,
  /** 系列ブラウザで絞り込む装置（装置モニターから移動したとき） */
  deviceFilter: "",
  /** 系列ブラウザで選んだ系列を、ダッシュボードの新しいパネルとして追加する */
  pendingPanel: null as { series: SeriesId[]; title: string } | null,
  toasts: [] as Toast[],
});

let toastSeq = 0;
export function toast(text: string, kind: "info" | "error" = "info") {
  const id = ++toastSeq;
  app.toasts.push({ id, text, kind });
  setTimeout(() => {
    const i = app.toasts.findIndex((t) => t.id === id);
    if (i >= 0) app.toasts.splice(i, 1);
  }, kind === "error" ? 8000 : 3500);
}

export function errorText(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}

export function deviceOf(s: Series): string {
  const tag = app.status?.config.device_tag ?? "device";
  return s.tags[tag] ?? s.tags["host"] ?? s.measurement;
}

/** 画面に出す系列名: 「装置 項目」 */
export function seriesLabel(id: SeriesId): string {
  const s = app.seriesById[id];
  if (!s) return `#${id}`;
  return `${deviceOf(s)} ${s.field}`;
}

export function seriesUnit(id: SeriesId): string {
  return app.seriesById[id]?.unit ?? "";
}

export async function refreshSeries() {
  try {
    const list = await api.series();
    app.series = list;
    const map: Record<number, Series> = {};
    for (const s of list) map[s.id] = s;
    app.seriesById = map;
  } catch {
    /* 状態の取得側で接続エラーを表示する */
  }
}

export async function refreshStatus() {
  try {
    app.status = await api.status();
    app.connected = true;
    app.connectionError = "";
  } catch (e) {
    app.connected = false;
    app.connectionError = errorText(e);
  }
}

export function startPolling() {
  refreshStatus();
  refreshSeries();
  let n = 0;
  setInterval(() => {
    refreshStatus();
    if (++n % 6 === 0 || app.series.length === 0) refreshSeries();
  }, 5000);
}

export function go(page: Page) {
  app.page = page;
}
