<script lang="ts">
  // 系列の複数選択（検索と装置での絞り込み付き）
  import type { SeriesId } from "../lib/api";
  import { app, deviceOf } from "../lib/state.svelte";
  import { fmtNum } from "../lib/time";

  interface Props {
    selected: SeriesId[];
    max?: number;
    height?: number;
  }
  let { selected = $bindable(), max = 50, height = 300 }: Props = $props();

  let q = $state("");
  let device = $state("");

  const devices = $derived([...new Set(app.series.map(deviceOf))].sort());
  const list = $derived.by(() => {
    const words = q.toLowerCase().split(/\s+/).filter(Boolean);
    return app.series.filter(
      (s) => (!device || deviceOf(s) === device) && words.every((w) => s.key.toLowerCase().includes(w)),
    );
  });
  const set = $derived(new Set(selected));

  function toggle(id: SeriesId) {
    if (set.has(id)) selected = selected.filter((x) => x !== id);
    else if (selected.length < max) selected = [...selected, id];
  }
  function selectAll() {
    const add = list.map((s) => s.id).filter((id) => !set.has(id));
    selected = [...selected, ...add].slice(0, max);
  }
  function clearShown() {
    const shown = new Set(list.map((s) => s.id));
    selected = selected.filter((id) => !shown.has(id));
  }
</script>

<div class="picker">
  <div class="bar">
    <input type="search" placeholder="系列を検索（例: dev01 temp）" bind:value={q} />
    <select bind:value={device}>
      <option value="">すべての装置</option>
      {#each devices as d}<option value={d}>{d}</option>{/each}
    </select>
    <button class="link" onclick={selectAll} disabled={list.length === 0}>表示中を全選択</button>
    <button class="link" onclick={clearShown}>解除</button>
  </div>
  <div class="list" style:height="{height}px">
    {#each list as s (s.id)}
      <label class="row" class:on={set.has(s.id)}>
        <input type="checkbox" checked={set.has(s.id)} onchange={() => toggle(s.id)} />
        <span class="dev">{deviceOf(s)}</span>
        <span class="field">{s.field}</span>
        <span class="key" title={s.key}>{s.key}</span>
        <span class="last">{fmtNum(s.last_value)} {s.unit ?? ""}</span>
      </label>
    {:else}
      <div class="none">該当する系列がありません</div>
    {/each}
  </div>
  <div class="count">{selected.length} 件選択{selected.length >= max ? `（上限 ${max}）` : ""}</div>
</div>

<style>
  .bar { display: flex; gap: 6px; margin-bottom: 6px; align-items: center; }
  .bar input[type="search"] { flex: 1; }
  .list { overflow: auto; border: 1px solid var(--line); border-radius: 6px; }
  .row { display: grid; grid-template-columns: 22px 90px 110px 1fr 100px; gap: 6px; align-items: center; padding: 4px 8px; font-size: 13px; cursor: pointer; border-bottom: 1px solid var(--line-soft); }
  .row:hover { background: var(--bg-soft); }
  .row.on { background: var(--accent-soft); }
  .dev, .field { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .key { color: var(--muted); font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; font-family: var(--mono); }
  .last { text-align: right; font-variant-numeric: tabular-nums; color: var(--muted); }
  .none { padding: 20px; text-align: center; color: var(--muted); }
  .count { font-size: 12px; color: var(--muted); margin-top: 4px; }
</style>
