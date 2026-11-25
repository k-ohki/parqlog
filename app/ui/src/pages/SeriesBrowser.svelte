<script lang="ts">
  import { onDestroy } from "svelte";
  import { api, type SeriesId } from "../lib/api";
  import { app, deviceOf, go, refreshSeries, seriesLabel, toast, errorText } from "../lib/state.svelte";
  import { fmtAgo, fmtDateTime, fmtNum, HOUR } from "../lib/time";
  import TimeSeriesChart from "../components/TimeSeriesChart.svelte";
  import Modal from "../components/Modal.svelte";
  import { confirmDialog } from "../lib/tauri";

  let q = $state("");
  let device = $state(app.deviceFilter);
  let selected = $state<SeriesId[]>([]);
  let now = $state(Date.now());
  let previewTo = $state(Date.now());
  let editingUnit = $state<{ id: SeriesId; value: string } | null>(null);
  let renaming = $state<{ id: SeriesId; key: string; error: string } | null>(null);
  let busy = $state(false);

  $effect(() => {
    app.deviceFilter = device;
  });

  const devices = $derived([...new Set(app.series.map(deviceOf))].sort());
  const rows = $derived.by(() => {
    const words = q.toLowerCase().split(/\s+/).filter(Boolean);
    return app.series.filter((s) => (!device || deviceOf(s) === device) && words.every((w) => s.key.toLowerCase().includes(w)));
  });
  const selSet = $derived(new Set(selected));
  const staleMs = $derived((app.status?.config.stale_seconds ?? 30) * 1000);

  // 最新値を5秒ごとに更新
  const timer = setInterval(() => {
    now = Date.now();
    refreshSeries();
  }, 5000);
  onDestroy(() => clearInterval(timer));

  function toggle(id: SeriesId) {
    selected = selSet.has(id) ? selected.filter((x) => x !== id) : [...selected, id].slice(-20);
  }
  function toggleAll() {
    const ids = rows.map((r) => r.id);
    const all = ids.every((id) => selSet.has(id));
    selected = all ? selected.filter((id) => !ids.includes(id)) : [...new Set([...selected, ...ids])].slice(0, 20);
  }
  function addPanel() {
    const title = device && selected.every((id) => deviceOf(app.seriesById[id]) === device) ? device : seriesLabel(selected[0]) + (selected.length > 1 ? " ほか" : "");
    app.pendingPanel = { series: selected.slice(), title };
    go("dashboard");
  }
  async function rename() {
    const r = renaming;
    if (!r) return;
    busy = true;
    try {
      await api.renameSeries(r.id, r.key);
      renaming = null;
      toast("系列の名前を変更しました");
      await refreshSeries();
    } catch (e) {
      r.error = errorText(e);
    } finally {
      busy = false;
    }
  }

  async function removeSelected() {
    const names = selected.slice(0, 5).map(seriesLabel).join("、") + (selected.length > 5 ? ` ほか ${selected.length - 5} 件` : "");
    if (!(await confirmDialog(`${selected.length} 系列（${names}）を削除しますか？\n一覧とグラフから消え、元に戻せません。`))) return;
    busy = true;
    let n = 0;
    try {
      for (const id of selected) {
        await api.deleteSeries(id);
        n++;
      }
      toast(`${n} 系列を削除しました`);
    } catch (e) {
      toast(`削除できません: ${errorText(e)}`, "error");
    } finally {
      selected = [];
      busy = false;
      await refreshSeries();
    }
  }

  async function saveUnit() {
    if (!editingUnit) return;
    const { id, value } = editingUnit;
    editingUnit = null;
    try {
      await api.setUnit(id, value.trim() || null);
      await refreshSeries();
    } catch (e) {
      toast(errorText(e), "error");
    }
  }
</script>

<div class="page">
  <div class="toolbar">
    <input type="search" class="q" placeholder="検索（例: dev01 f03）" bind:value={q} />
    <select bind:value={device}>
      <option value="">すべての装置</option>
      {#each devices as d}<option value={d}>{d}</option>{/each}
    </select>
    <span class="count">{rows.length} / {app.series.length} 系列</span>
    <div class="spacer"></div>
    <span class="count">{selected.length} 件選択</span>
    <button onclick={() => (selected = [])} disabled={selected.length === 0}>選択解除</button>
    <button class="danger" onclick={removeSelected} disabled={selected.length === 0 || busy}>削除</button>
    <button class="primary" onclick={addPanel} disabled={selected.length === 0}>ダッシュボードに追加</button>
  </div>

  {#if selected.length > 0}
    <section class="preview">
      <div class="ph">プレビュー（直近1時間）<button class="link" onclick={() => (previewTo = Date.now())}>更新</button></div>
      <TimeSeriesChart series={selected} from={previewTo - HOUR} to={previewTo} height={200} />
    </section>
  {/if}

  <div class="table-wrap">
    <table>
      <thead>
        <tr>
          <th class="c"><input type="checkbox" checked={rows.length > 0 && rows.every((r) => selSet.has(r.id))} onchange={toggleAll} /></th>
          <th>装置</th>
          <th>項目</th>
          <th>系列キー</th>
          <th>単位</th>
          <th class="r">最新値</th>
          <th>受信</th>
          <th class="r">ID</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each rows as s (s.id)}
          {@const stale = s.last_ts == null || now - s.last_ts > staleMs}
          <tr class:sel={selSet.has(s.id)} onclick={() => toggle(s.id)}>
            <td class="c"><input type="checkbox" checked={selSet.has(s.id)} onclick={(e) => e.stopPropagation()} onchange={() => toggle(s.id)} /></td>
            <td>{deviceOf(s)}</td>
            <td>{s.field}</td>
            <td class="key" title={s.key}>{s.key}</td>
            <td onclick={(e) => e.stopPropagation()}>
              {#if editingUnit?.id === s.id}
                <!-- svelte-ignore a11y_autofocus -->
                <input class="unit" bind:value={editingUnit.value} autofocus onblur={saveUnit} onkeydown={(e) => e.key === "Enter" && saveUnit()} />
              {:else}
                <button class="link" onclick={() => (editingUnit = { id: s.id, value: s.unit ?? "" })}>{s.unit || "設定"}</button>
              {/if}
            </td>
            <td class="r num">{fmtNum(s.last_value)}</td>
            <td class:stale title={fmtDateTime(s.last_ts)}>{fmtAgo(s.last_ts, now)}</td>
            <td class="r muted">{s.id}</td>
            <td onclick={(e) => e.stopPropagation()}>
              <button class="link" onclick={() => (renaming = { id: s.id, key: s.key, error: "" })}>名前変更</button>
            </td>
          </tr>
        {:else}
          <tr><td colspan="9" class="none">{app.series.length === 0 ? "まだ系列がありません。装置からデータを送ると自動で登録されます。" : "該当する系列がありません"}</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
</div>

{#if renaming}
  <Modal title="系列の名前変更" onclose={() => (renaming = null)}>
    <form onsubmit={(e) => { e.preventDefault(); rename(); }}>
      <label class="rk">系列キー（measurement,タグ=値,... 項目名）
        <!-- svelte-ignore a11y_autofocus -->
        <input bind:value={renaming.key} autofocus />
      </label>
    </form>
    <p class="hint">データ（ID）はそのまま引き継ぎます。送信側が古い名前で送り続けると、古い名前の系列が新しく作られるので、送信側の設定も直してください。</p>
    {#if renaming.error}<p class="err">{renaming.error}</p>{/if}
    {#snippet footer()}
      <button onclick={() => (renaming = null)}>キャンセル</button>
      <button class="primary" onclick={rename} disabled={busy}>変更</button>
    {/snippet}
  </Modal>
{/if}

<style>
  .rk { display: flex; flex-direction: column; gap: 4px; font-size: 12px; color: var(--muted); }
  .rk input { width: 100%; font-family: var(--mono); }
  .hint { font-size: 12px; color: var(--muted); }
  .err { font-size: 12px; color: var(--danger); }
  .toolbar { display: flex; gap: 8px; align-items: center; margin-bottom: 12px; flex-wrap: wrap; }
  .q { width: 280px; }
  .count { font-size: 12px; color: var(--muted); }
  .spacer { flex: 1; }
  .preview { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px; margin-bottom: 12px; }
  .ph { font-size: 12px; color: var(--muted); margin-bottom: 4px; display: flex; gap: 8px; }
  .table-wrap { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; overflow: auto; }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th { position: sticky; top: 0; background: var(--bg-soft); text-align: left; font-weight: 600; font-size: 12px; color: var(--muted); padding: 7px 8px; border-bottom: 1px solid var(--line); }
  td { padding: 5px 8px; border-bottom: 1px solid var(--line-soft); white-space: nowrap; }
  tbody tr { cursor: pointer; }
  tbody tr:hover { background: var(--bg-soft); }
  tr.sel { background: var(--accent-soft) !important; }
  .c { width: 28px; text-align: center; }
  .r { text-align: right; }
  .num { font-variant-numeric: tabular-nums; }
  .key { font-family: var(--mono); font-size: 11px; color: var(--muted); max-width: 360px; overflow: hidden; text-overflow: ellipsis; }
  .stale { color: var(--danger); }
  .muted { color: var(--muted); }
  .unit { width: 80px; padding: 2px 4px; }
  .none { text-align: center; color: var(--muted); padding: 30px; }
</style>
