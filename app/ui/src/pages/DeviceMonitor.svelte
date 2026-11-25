<script lang="ts">
  import { onDestroy } from "svelte";
  import { api, type Device } from "../lib/api";
  import { app, go, errorText } from "../lib/state.svelte";
  import { fmtAgo, fmtDateTime } from "../lib/time";

  let devices = $state<Device[]>([]);
  let error = $state("");
  let now = $state(Date.now());

  async function load() {
    try {
      devices = await api.devices();
      error = "";
    } catch (e) {
      error = errorText(e);
    }
    now = Date.now();
  }
  load();
  const t = setInterval(load, 2000);
  onDestroy(() => clearInterval(t));

  const staleCount = $derived(devices.filter((d) => d.stale).length);
  const staleSec = $derived(app.status?.config.stale_seconds ?? 30);

  function openSeries(d: Device) {
    app.deviceFilter = d.device;
    go("series");
  }
</script>

<div class="page">
  <div class="summary">
    <div class="box"><div class="n">{devices.length}</div><div class="l">装置</div></div>
    <div class="box ok"><div class="n">{devices.length - staleCount}</div><div class="l">受信中</div></div>
    <div class="box" class:bad={staleCount > 0}><div class="n">{staleCount}</div><div class="l">欠測（{staleSec}秒以上途絶）</div></div>
    <p class="note">装置は「{app.status?.config.device_tag ?? "device"}」タグで区別します（無い場合は host タグ、さらに無い場合は measurement）。</p>
  </div>
  {#if error}<div class="notice err">{error}</div>{/if}
  <div class="cards">
    {#each devices as d (d.device)}
      <button class="card" class:stale={d.stale} onclick={() => openSeries(d)} title="この装置の系列を表示">
        <div class="top">
          <span class="dot"></span>
          <span class="name">{d.device}</span>
          <span class="state">{d.stale ? "欠測" : "受信中"}</span>
        </div>
        <div class="ago">{fmtAgo(d.last_ts, now)}</div>
        <div class="meta">最終受信 {fmtDateTime(d.last_ts)}</div>
        <div class="meta">{d.series_count} 系列</div>
      </button>
    {:else}
      <p class="none">まだ装置からデータを受け取っていません。</p>
    {/each}
  </div>
</div>

<style>
  .summary { display: flex; gap: 12px; align-items: center; margin-bottom: 16px; flex-wrap: wrap; }
  .box { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 10px 18px; min-width: 120px; }
  .box .n { font-size: 26px; font-weight: 600; font-variant-numeric: tabular-nums; }
  .box .l { font-size: 12px; color: var(--muted); }
  .box.ok .n { color: var(--ok); }
  .box.bad { border-color: var(--danger); background: var(--danger-soft); }
  .box.bad .n { color: var(--danger); }
  .note { font-size: 12px; color: var(--muted); }
  .cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 12px; }
  .card { text-align: left; background: var(--panel); border: 1px solid var(--line); border-left: 4px solid var(--ok); border-radius: 8px; padding: 12px 14px; cursor: pointer; font: inherit; color: inherit; }
  .card:hover { box-shadow: 0 2px 8px rgba(0, 0, 0, 0.06); }
  .card.stale { border-left-color: var(--danger); background: var(--danger-soft); }
  .top { display: flex; align-items: center; gap: 8px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; background: var(--ok); }
  .stale .dot { background: var(--danger); animation: blink 1.2s infinite; }
  @keyframes blink { 50% { opacity: 0.3; } }
  .name { font-weight: 600; flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .state { font-size: 12px; color: var(--ok); }
  .stale .state, .stale .ago { color: var(--danger); }
  .ago { font-size: 22px; font-weight: 600; margin: 6px 0 2px; }
  .meta { font-size: 12px; color: var(--muted); }
  .none { color: var(--muted); }
</style>
