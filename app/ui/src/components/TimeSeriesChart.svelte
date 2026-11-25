<script lang="ts">
  // 時系列グラフ（uPlot）。min / max の帯と平均線を重ねて描く。
  // 表示幅（px）を API に渡し、1系列あたり最大でもその点数だけを受け取る。
  import uPlot from "uplot";
  import "uplot/dist/uPlot.min.css";
  import { onDestroy } from "svelte";
  import { api, type AggResult, type SeriesId } from "../lib/api";
  import { color, alpha } from "../lib/palette";
  import { seriesLabel, seriesUnit, errorText } from "../lib/state.svelte";
  import { fmtNum, fmtDateTime, fmtDuration, TIER_LABEL } from "../lib/time";

  interface Props {
    series: SeriesId[];
    from: number;
    to: number;
    /** 値が変わるたびに再取得する（自動更新用） */
    tick?: number;
    height?: number;
    unit?: string;
    decimals?: number | null;
    yMin?: number | null;
    yMax?: number | null;
    onzoom?: (from: number, to: number) => void;
    onzoomout?: () => void;
  }
  let { series, from, to, tick = 0, height = 260, unit = "", decimals = null, yMin = null, yMax = null, onzoom, onzoomout }: Props = $props();

  let plotEl: HTMLDivElement;
  let width = $state(0);
  let result = $state<AggResult | null>(null);
  let loading = $state(false);
  let error = $state("");
  let hidden = $state<Record<number, boolean>>({});
  let cursorIdx = $state<number | null>(null);
  let plot = $state.raw<uPlot | null>(null);
  let signature = "";
  let ctrl: AbortController | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  // 範囲は描画時に参照する（uPlot の range 関数から読む）
  let curFrom = 0;
  let curTo = 0;

  const plotHeight = $derived(Math.max(100, height));

  // 範囲・幅・系列・自動更新が変わったら再取得（連続操作は 150ms まとめる）
  $effect(() => {
    const s = series.slice();
    const f = from;
    const t = to;
    const w = width;
    void tick;
    if (w < 50) return;
    clearTimeout(timer);
    timer = setTimeout(() => load(s, f, t, w), 150);
    return () => clearTimeout(timer);
  });

  // 高さや表示設定だけが変わったときは描き直すだけ
  $effect(() => {
    void [plotHeight, unit, decimals, yMin, yMax, JSON.stringify(hidden)];
    if (result) draw();
  });

  $effect(() => {
    if (plot && width > 0) plot.setSize({ width, height: plotHeight });
  });

  async function load(s: SeriesId[], f: number, t: number, w: number) {
    ctrl?.abort();
    if (s.length === 0) {
      result = null;
      destroy();
      return;
    }
    const c = new AbortController();
    ctrl = c;
    loading = true;
    try {
      const r = await api.query({ series: s, from: f, to: t, width: w }, c.signal);
      if (c.signal.aborted) return;
      curFrom = f;
      curTo = t;
      result = r;
      error = "";
      draw();
    } catch (e) {
      if ((e as Error).name === "AbortError") return;
      error = errorText(e);
    } finally {
      if (ctrl === c) loading = false;
    }
  }

  function destroy() {
    plot?.destroy();
    plot = null;
    signature = "";
  }

  function fmtY(v: number) {
    return fmtNum(v, decimals);
  }

  /** 目盛りの間隔から小数桁を決める（0.002 刻みなら3桁） */
  function decFor(incr: number): number {
    if (!Number.isFinite(incr) || incr <= 0) return 0;
    return Math.max(0, Math.min(6, -Math.floor(Math.log10(incr) + 1e-9)));
  }

  const p2 = (n: number) => String(n).padStart(2, "0");

  /** 時刻軸: 24時間表記。日付が変わる目盛りには日付も付ける */
  function xLabels(splits: number[], incr: number): string[] {
    return splits.map((v, i) => {
      const d = new Date(v);
      const md = `${d.getMonth() + 1}/${d.getDate()}`;
      if (incr >= 28 * 86_400_000) return `${d.getFullYear()}/${d.getMonth() + 1}`;
      if (incr >= 86_400_000) return i === 0 ? `${md}\n${d.getFullYear()}` : md;
      const hm = `${p2(d.getHours())}:${p2(d.getMinutes())}${incr < 60_000 ? `:${p2(d.getSeconds())}` : ""}`;
      const prev = i > 0 ? new Date(splits[i - 1]) : null;
      return !prev || prev.getDate() !== d.getDate() ? `${hm}\n${md}` : hm;
    });
  }

  function buildData(r: AggResult): uPlot.AlignedData {
    if (r.series.length === 0) return [[]];
    // 系列ごとに時刻が違ってもよいように揃える。欠測（null）は線を切り、揃えで生じた穴（undefined）はつなぐ
    const tables = r.series.map((s) => [s.t, s.min, s.max, s.avg]) as unknown as uPlot.AlignedData[];
    return uPlot.join(tables);
  }

  function draw() {
    const r = result;
    if (!r || !plotEl || width < 50) return;
    const data = buildData(r);
    const sig = JSON.stringify([r.series.map((s) => s.id), r.band, plotHeight, unit, decimals, yMin, yMax, hidden]);
    if (plot && sig === signature) {
      plot.setData(data);
      return;
    }
    destroy();

    const few = (data[0]?.length ?? 0) < 60;
    const sOpts: uPlot.Series[] = [{}];
    const bands: uPlot.Band[] = [];
    r.series.forEach((s, i) => {
      const c = color(i);
      const base = 1 + i * 3;
      const label = seriesLabel(s.id);
      const show = !hidden[s.id];
      const thin = { width: 1, points: { show: false }, spanGaps: false };
      sOpts.push({ label: `${label} 最小`, stroke: alpha(c, 0.35), show: show && r.band, value: (_u, v) => fmtY(v), ...thin });
      sOpts.push({ label: `${label} 最大`, stroke: alpha(c, 0.35), show: show && r.band, value: (_u, v) => fmtY(v), ...thin });
      sOpts.push({
        label,
        stroke: c,
        width: 1.6,
        show,
        spanGaps: false,
        points: { show: few, size: 4, fill: c },
        value: (_u, v) => fmtY(v),
      });
      if (r.band && show) bands.push({ series: [base + 1, base], fill: alpha(c, 0.13) });
    });

    const opts: uPlot.Options = {
      width,
      height: plotHeight,
      ms: 1,
      legend: { show: false },
      cursor: {
        sync: { key: "parqlog" },
        drag: { x: true, y: false, setScale: false },
        points: { size: 7 },
      },
      select: { show: true, left: 0, top: 0, width: 0, height: 0 },
      scales: {
        x: { time: true, range: () => [curFrom, curTo] },
        y: {
          range: (_u, min, max) => {
            if (min == null || max == null || !Number.isFinite(min)) return [yMin ?? 0, yMax ?? 1];
            const [a, b] = uPlot.rangeNum(min, max, 0.1, true);
            return [yMin ?? a, yMax ?? b];
          },
        },
      },
      axes: [
        {
          stroke: "#5b6472",
          grid: { stroke: "#eceff3", width: 1 },
          ticks: { stroke: "#d9dee5", width: 1 },
          size: 46,
          values: (_u, splits, _ax, _space, incr) => xLabels(splits, incr),
        },
        {
          stroke: "#5b6472",
          grid: { stroke: "#eceff3", width: 1 },
          ticks: { stroke: "#d9dee5", width: 1 },
          size: 60,
          values: (_u, vals, _ax, _space, incr) => {
            const d = decimals ?? decFor(incr);
            return vals.map((v) => v.toLocaleString("ja-JP", { minimumFractionDigits: d, maximumFractionDigits: d }));
          },
          label: unit || undefined,
          labelSize: unit ? 18 : 0,
        },
      ],
      series: sOpts,
      bands,
      hooks: {
        setSelect: [
          (u) => {
            const s = u.select;
            if (s.width > 4) {
              const a = u.posToVal(s.left, "x");
              const b = u.posToVal(s.left + s.width, "x");
              if (b - a > 1000) onzoom?.(a, b);
            }
            u.setSelect({ left: 0, top: 0, width: 0, height: 0 }, false);
          },
        ],
        setCursor: [(u) => (cursorIdx = u.cursor.idx ?? null)],
        ready: [(u) => u.over.addEventListener("dblclick", () => onzoomout?.())],
      },
    };
    plot = new uPlot(opts, data, plotEl);
    signature = sig;
  }

  function lastValue(arr: (number | null | undefined)[]): number | null {
    for (let i = arr.length - 1; i >= 0; i--) if (arr[i] != null) return arr[i] as number;
    return null;
  }

  // 凡例: カーソル位置の平均値（カーソルが無ければ最後の値）
  const legend = $derived.by(() => {
    const r = result;
    if (!r) return [];
    const data = plot?.data;
    return r.series.map((s, i) => {
      const col = data?.[3 + i * 3] as (number | null | undefined)[] | undefined;
      const v = cursorIdx != null && col ? col[cursorIdx] ?? null : col ? lastValue(col) : lastValue(s.avg);
      return { id: s.id, label: seriesLabel(s.id), unit: seriesUnit(s.id), color: color(i), value: v };
    });
  });
  const cursorTime = $derived(cursorIdx != null && plot ? (plot.data[0][cursorIdx] as number) : null);
  const empty = $derived(!!result && result.series.every((s) => s.t.length === 0));

  function toggle(id: SeriesId, ev: MouseEvent) {
    if (ev.ctrlKey || ev.metaKey) {
      // Ctrl+クリック: この系列だけ表示
      const only = Object.fromEntries(series.map((x) => [x, x !== id]));
      const already = series.every((x) => (x === id ? !hidden[x] : hidden[x]));
      hidden = already ? {} : only;
    } else {
      hidden = { ...hidden, [id]: !hidden[id] };
    }
  }

  onDestroy(() => {
    ctrl?.abort();
    clearTimeout(timer);
    destroy();
  });
</script>

<div class="chart">
  <div class="plot" bind:this={plotEl} bind:clientWidth={width} style:height="{plotHeight}px">
    {#if series.length === 0}
      <div class="msg">系列が選ばれていません。パネルの「編集」から追加してください</div>
    {:else if error}
      <div class="msg err">{error}</div>
    {:else if empty}
      <div class="msg">この期間のデータはありません</div>
    {/if}
    {#if loading}<div class="spin" title="読み込み中"></div>{/if}
  </div>
  {#if result && result.series.length > 0}
    <div class="legend">
      {#each legend as l (l.id)}
        <button class="item" class:off={hidden[l.id]} onclick={(e) => toggle(l.id, e)} title="クリックで表示切替、Ctrl+クリックでこの系列だけ表示">
          <span class="sw" style:background={l.color}></span>
          <span class="name">{l.label}</span>
          <span class="val">{fmtNum(l.value, decimals)}{l.unit ? ` ${l.unit}` : ""}</span>
        </button>
      {/each}
      <span class="meta">
        {#if cursorTime != null}{fmtDateTime(cursorTime)} · {/if}
        {TIER_LABEL[result.tier]} · {fmtDuration(result.bucket_ms)}ごと · {result.elapsed_ms.toFixed(0)}ms
      </span>
    </div>
  {/if}
</div>

<style>
  .chart { display: flex; flex-direction: column; min-width: 0; }
  .plot { position: relative; width: 100%; }
  .msg {
    position: absolute; inset: 0; display: flex; align-items: center; justify-content: center;
    color: var(--muted); font-size: 13px; text-align: center; padding: 0 16px; z-index: 1; pointer-events: none;
  }
  .msg.err { color: var(--danger); }
  .spin {
    position: absolute; top: 6px; right: 8px; width: 12px; height: 12px; border-radius: 50%;
    border: 2px solid var(--line); border-top-color: var(--accent); animation: r 0.8s linear infinite; z-index: 2;
  }
  @keyframes r { to { transform: rotate(360deg); } }
  .legend { display: flex; flex-wrap: wrap; gap: 2px 12px; padding: 4px 4px 0; font-size: 12px; align-items: center; }
  .item { display: inline-flex; align-items: center; gap: 5px; border: 0; background: none; padding: 1px 2px; cursor: pointer; color: var(--text); font: inherit; }
  .item.off { opacity: 0.35; }
  .sw { width: 10px; height: 3px; border-radius: 2px; }
  .val { font-variant-numeric: tabular-nums; color: var(--muted); }
  .meta { margin-left: auto; color: var(--muted); font-size: 11px; white-space: nowrap; }
  :global(.u-select) { background: rgba(37, 99, 235, 0.12); }
</style>
