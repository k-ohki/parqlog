<script lang="ts">
  // CSV の取り込み。確定済みの過去の日にも書き足せる（同じ時刻は上書き）。
  import { api } from "../lib/api";
  import { refreshSeries, toast, errorText } from "../lib/state.svelte";
  import { isTauri, native, openCsvDialog } from "../lib/tauri";
  import { fmtNum, fmtDateTime } from "../lib/time";

  const off = -new Date().getTimezoneOffset();
  const pad = (n: number) => String(Math.floor(Math.abs(n))).padStart(2, "0");
  const localOffset = `${off >= 0 ? "+" : "-"}${pad(off / 60)}:${pad(off % 60)}`;

  let path = $state("");
  let file = $state<File | null>(null);
  let layout = $state<"wide" | "long">("wide");
  let measurement = $state("");
  let tags = $state("");
  let timeColumn = $state("");
  let utcOffset = $state(localOffset);
  let busy = $state(false);
  let report = $state<any>(null);

  const name = $derived(isTauri ? path.split("/").pop() ?? "" : file?.name ?? "");
  const stem = $derived(name.replace(/\.[^.]+$/, ""));

  async function pick() {
    const p = await openCsvDialog();
    if (p) path = p;
  }

  function params(): Record<string, string> {
    const p: Record<string, string> = { layout, utc_offset: utcOffset, measurement: measurement.trim() || stem || "csv" };
    if (timeColumn.trim()) p.time_column = timeColumn.trim();
    for (const kv of tags.split(/[,、\s]+/).filter(Boolean)) {
      const [k, ...v] = kv.split("=");
      if (k && v.length) p[`tag.${k}`] = v.join("=");
    }
    return p;
  }

  async function run() {
    busy = true;
    report = null;
    try {
      report = isTauri ? await native.importCsv(path, params()) : await api.importBlob(file!, params());
      toast(`${fmtNum(report.points, 0)} 点を取り込みました`);
      refreshSeries();
    } catch (e) {
      toast(`取り込めません: ${errorText(e)}`, "error");
    } finally {
      busy = false;
    }
  }
</script>

<section class="card">
  <h2>CSV の取り込み</h2>
  <div class="row">
    {#if isTauri}
      <button onclick={pick}>ファイルを選ぶ</button>
    {:else}
      <input type="file" accept=".csv,.txt,text/csv" onchange={(e) => (file = (e.target as HTMLInputElement).files?.[0] ?? null)} />
    {/if}
    <span class="name">{name || "未選択"}</span>
  </div>
  <div class="grid">
    <label>形式
      <select bind:value={layout}>
        <option value="wide">wide（時刻, 列1, 列2, …）</option>
        <option value="long">long（時刻, 系列キー, 値）</option>
      </select>
    </label>
    {#if layout === "wide"}
      <label>measurement <input bind:value={measurement} placeholder={stem || "csv"} /></label>
      <label>タグ <input bind:value={tags} placeholder="device=dev01, line=A" /></label>
    {/if}
    <label>時刻の列 <input bind:value={timeColumn} placeholder="先頭列" /></label>
    <label>時刻のずれ（UTC から） <input bind:value={utcOffset} /></label>
  </div>
  <p class="help">
    時刻は「2026/11/25 14:00:00」などの日時、エポック秒・ミリ秒のどれでも読めます。タイムゾーンの無い日時は上の「ずれ」で解釈します（列名が _utc で終わる列は UTC）。
    過去の日にも取り込めます（同じ系列・時刻は上書き、集計も作り直し）。数値でないセルは飛ばします。
  </p>
  <button class="primary" onclick={run} disabled={busy || !name}>{busy ? "取り込み中…" : "取り込む"}</button>
  {#if report}
    <dl>
      <dt>取り込んだ点</dt><dd>{fmtNum(report.points, 0)}（{report.series} 系列）</dd>
      <dt>期間</dt><dd>{fmtDateTime(report.from_ms)} 〜 {fmtDateTime(report.to_ms)}</dd>
      <dt>うち過去の日へ</dt><dd>{fmtNum(report.backfilled_points, 0)} 点（{report.backfilled_days} 日分を作り直し）</dd>
      <dt>読めなかった行・セル</dt><dd>{report.skipped_rows} 行 / {report.skipped_cells} セル</dd>
      {#if report.rejected_old + report.rejected_future > 0}
        <dt>範囲外で捨てた点</dt><dd>{report.rejected_old + report.rejected_future}</dd>
      {/if}
      <dt>所要</dt><dd>{(report.elapsed_ms / 1000).toFixed(1)} 秒</dd>
    </dl>
  {/if}
</section>

<style>
  .card { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 12px 16px; margin-bottom: 14px; display: flex; flex-direction: column; gap: 8px; }
  h2 { font-size: 14px; margin: 0; }
  .row { display: flex; gap: 8px; align-items: center; }
  .name { font-family: var(--mono); font-size: 12px; color: var(--muted); }
  .grid { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 10px; }
  .grid input, .grid select { width: 100%; }
  label { display: flex; flex-direction: column; gap: 3px; font-size: 12px; color: var(--muted); }
  .help { font-size: 12px; color: var(--muted); margin: 0; }
  .primary { align-self: flex-start; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 3px 14px; font-size: 13px; margin: 0; }
  dt { color: var(--muted); }
  dd { margin: 0; }
</style>
