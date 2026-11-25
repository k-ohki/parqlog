<script lang="ts">
  import type { SeriesId } from "../lib/api";
  import { api } from "../lib/api";
  import { toast, errorText } from "../lib/state.svelte";
  import { isTauri, native, saveDialog } from "../lib/tauri";
  import { DAY, HOUR, MIN, SEC, fmtBytes, fmtNum, toLocalInput, fromLocalInput } from "../lib/time";
  import SeriesPicker from "../components/SeriesPicker.svelte";
  import ImportCard from "../components/ImportCard.svelte";

  let selected = $state<SeriesId[]>([]);
  let fromStr = $state(toLocalInput(Date.now() - DAY));
  let toStr = $state(toLocalInput(Date.now()));
  let tier = $state<"auto" | "raw" | "1m" | "1h">("auto");
  let format = $state<"csv" | "parquet">("csv");
  let busy = $state(false);

  const from = $derived(fromLocalInput(fromStr));
  const to = $derived(fromLocalInput(toStr));
  const span = $derived(from != null && to != null ? to - from : 0);
  const effTier = $derived(tier !== "auto" ? tier : span <= DAY ? "raw" : span <= 31 * DAY ? "1m" : "1h");
  const res = $derived(effTier === "raw" ? SEC : effTier === "1m" ? MIN : HOUR);
  const estRows = $derived(Math.max(0, Math.round((span / res) * selected.length)));
  const estBytes = $derived(estRows * (format === "csv" ? (effTier === "raw" ? 50 : 80) : 5));

  function quick(ms: number) {
    const now = Date.now();
    fromStr = toLocalInput(now - ms);
    toStr = toLocalInput(now);
  }

  async function run() {
    if (from == null || to == null || to <= from || selected.length === 0) return;
    const body = { series: selected, from, to, tier: effTier, format };
    busy = true;
    try {
      const stamp = fromStr.slice(0, 10).replace(/-/g, "");
      const name = `parqlog-${stamp}-${effTier}.${format}`;
      if (isTauri) {
        const path = await saveDialog(name, format);
        if (!path) return;
        const r = await native.exportToFile(body, path);
        toast(`保存しました: ${r.rows != null ? fmtNum(r.rows, 0) + " 行、" : ""}${fmtBytes(r.bytes)}`);
      } else {
        const { blob, name: n } = await api.exportBlob(body);
        const a = document.createElement("a");
        a.href = URL.createObjectURL(blob);
        a.download = n;
        a.click();
        URL.revokeObjectURL(a.href);
      }
    } catch (e) {
      toast(`エクスポートできません: ${errorText(e)}`, "error");
    } finally {
      busy = false;
    }
  }
</script>

<ImportCard />
<div class="page cols">
  <section class="card">
    <h2>書き出し: 系列</h2>
    <SeriesPicker bind:selected max={200} height={460} />
  </section>
  <section class="card side">
    <h2>期間</h2>
    <label>開始 <input type="datetime-local" bind:value={fromStr} /></label>
    <label>終了 <input type="datetime-local" bind:value={toStr} /></label>
    <div class="quick">
      <button onclick={() => quick(HOUR)}>1時間</button>
      <button onclick={() => quick(DAY)}>1日</button>
      <button onclick={() => quick(7 * DAY)}>7日</button>
      <button onclick={() => quick(30 * DAY)}>30日</button>
      <button onclick={() => quick(365 * DAY)}>1年</button>
    </div>
    <h2>粒度</h2>
    <div class="radios">
      {#each [["auto", "自動"], ["raw", "生データ（1秒）"], ["1m", "1分集計"], ["1h", "1時間集計"]] as [v, l]}
        <label class="r"><input type="radio" bind:group={tier} value={v} /> {l}</label>
      {/each}
    </div>
    <p class="help">集計では min / max / avg / count の列を出します。時刻は UTC です。</p>
    <h2>形式</h2>
    <div class="radios">
      <label class="r"><input type="radio" bind:group={format} value="csv" /> CSV（Excel 向け）</label>
      <label class="r"><input type="radio" bind:group={format} value="parquet" /> Parquet（分析ツール向け）</label>
    </div>
    <div class="est">
      見込み: 約 {fmtNum(estRows, 0)} 行 / {fmtBytes(estBytes)}
      {#if format === "csv" && estRows > 1_048_000}<div class="warn">Excel で開ける行数（約104万行）を超えます</div>{/if}
    </div>
    <button class="primary" onclick={run} disabled={busy || selected.length === 0 || span <= 0}>{busy ? "書き出し中…" : "書き出す"}</button>
  </section>
</div>

<style>
  .cols { display: grid; grid-template-columns: 1fr 340px; gap: 14px; align-items: start; }
  .card { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 12px 16px; }
  .side { display: flex; flex-direction: column; gap: 8px; }
  h2 { font-size: 14px; margin: 6px 0 2px; }
  label { display: flex; flex-direction: column; gap: 3px; font-size: 12px; color: var(--muted); }
  .r { flex-direction: row; align-items: center; gap: 6px; color: var(--text); font-size: 13px; }
  .radios { display: flex; flex-direction: column; gap: 4px; }
  .quick { display: flex; gap: 4px; flex-wrap: wrap; }
  .quick button { font-size: 12px; padding: 3px 8px; }
  .help { font-size: 12px; color: var(--muted); margin: 0; }
  .est { font-size: 13px; background: var(--bg-soft); border-radius: 6px; padding: 8px 10px; }
  .warn { color: var(--warn); font-size: 12px; margin-top: 4px; }
</style>
