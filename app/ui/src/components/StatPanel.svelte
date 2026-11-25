<script lang="ts">
  // 最新値の数字表示。受信が途絶えた系列は赤で示す。
  import { onDestroy } from "svelte";
  import { api, type Latest, type SeriesId } from "../lib/api";
  import { app, seriesLabel, seriesUnit, errorText } from "../lib/state.svelte";
  import { fmtNum, fmtAgo, fmtDateTime } from "../lib/time";
  import { color } from "../lib/palette";

  interface Props {
    series: SeriesId[];
    tick?: number;
    height?: number;
    unit?: string;
    decimals?: number | null;
  }
  let { series, tick = 0, height = 160, unit = "", decimals = null }: Props = $props();

  let latest = $state<Record<number, Latest>>({});
  let error = $state("");
  let now = $state(Date.now());

  async function load(ids: SeriesId[]) {
    if (ids.length === 0) return;
    try {
      const list = await api.latest(ids);
      latest = Object.fromEntries(list.map((l) => [l.id, l]));
      error = "";
    } catch (e) {
      error = errorText(e);
    }
    now = Date.now();
  }

  $effect(() => {
    void tick;
    load(series.slice());
  });

  // 自動更新が止まっていても経過時間の表示は進める
  const clock = setInterval(() => (now = Date.now()), 1000);
  onDestroy(() => clearInterval(clock));

  const staleMs = $derived((app.status?.config.stale_seconds ?? 30) * 1000);
  const big = $derived(series.length <= 2);
</script>

<div class="stat" style:min-height="{height}px">
  {#if series.length === 0}
    <div class="msg">系列が選ばれていません</div>
  {:else if error}
    <div class="msg err">{error}</div>
  {:else}
    <div class="grid" class:big>
      {#each series as id, i (id)}
        {@const l = latest[id]}
        {@const stale = !l || now - l.ts > staleMs}
        <div class="cell" class:stale title={l ? fmtDateTime(l.ts) : "受信なし"}>
          <div class="label"><span class="sw" style:background={color(i)}></span>{seriesLabel(id)}</div>
          <div class="value">
            {fmtNum(l?.value, decimals)}<span class="unit">{unit || seriesUnit(id)}</span>
          </div>
          <div class="age">{fmtAgo(l?.ts, now)}</div>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .stat { display: flex; align-items: stretch; }
  .msg { margin: auto; color: var(--muted); font-size: 13px; }
  .msg.err { color: var(--danger); }
  .grid { flex: 1; display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 8px; align-content: center; }
  .grid.big { grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); }
  .cell { padding: 6px 10px; border-radius: 6px; background: var(--bg-soft); min-width: 0; }
  .cell.stale { background: var(--danger-soft); }
  .cell.stale .value { color: var(--danger); }
  .label { font-size: 12px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; display: flex; align-items: center; gap: 5px; }
  .sw { width: 8px; height: 8px; border-radius: 2px; flex: none; }
  .value { font-size: 26px; font-weight: 600; font-variant-numeric: tabular-nums; line-height: 1.25; white-space: nowrap; }
  .big .value { font-size: 38px; }
  .unit { font-size: 0.45em; font-weight: 500; color: var(--muted); margin-left: 4px; }
  .age { font-size: 11px; color: var(--muted); }
</style>
