// ダッシュボード定義（サーバーには JSON のまま保存する）
import type { TimeRange } from "./time";
import { HOUR } from "./time";
import type { SeriesId } from "./api";

export type PanelType = "timeseries" | "stat";

export interface Panel {
  id: string;
  type: PanelType;
  title: string;
  series: SeriesId[];
  /** 12分割のうち何列使うか */
  width: 4 | 6 | 8 | 12;
  height: number;
  unit?: string;
  decimals?: number | null;
  yMin?: number | null;
  yMax?: number | null;
}

export interface Dashboard {
  version: 1;
  name: string;
  range: TimeRange;
  /** 自動更新の間隔（秒）。0 で停止 */
  refresh: number;
  panels: Panel[];
}

export function newId(prefix = "p"): string {
  return `${prefix}${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
}

export function emptyDashboard(name: string): Dashboard {
  return { version: 1, name, range: { kind: "relative", ms: HOUR }, refresh: 5, panels: [] };
}

export function newPanel(series: SeriesId[] = [], title = "新しいパネル"): Panel {
  return { id: newId(), type: "timeseries", title, series, width: 6, height: 280, decimals: null, yMin: null, yMax: null };
}

/** 読み込んだ JSON を補正する（古い形式や手で編集されたものに備える） */
export function normalize(raw: any, fallbackName: string): Dashboard {
  const d = emptyDashboard(raw?.name ?? fallbackName);
  if (raw?.range?.kind === "relative" && raw.range.ms > 0) d.range = { kind: "relative", ms: raw.range.ms };
  if (raw?.range?.kind === "absolute" && raw.range.to > raw.range.from) d.range = { kind: "absolute", from: raw.range.from, to: raw.range.to };
  if (typeof raw?.refresh === "number") d.refresh = raw.refresh;
  if (Array.isArray(raw?.panels)) {
    d.panels = raw.panels.map((p: any) => ({
      ...newPanel(),
      ...p,
      id: p.id ?? newId(),
      series: Array.isArray(p.series) ? p.series.filter((x: any) => Number.isInteger(x)) : [],
      width: [4, 6, 8, 12].includes(p.width) ? p.width : 6,
      height: Math.min(900, Math.max(140, Number(p.height) || 280)),
    }));
  }
  return d;
}
