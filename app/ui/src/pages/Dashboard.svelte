<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import { api, type DashboardInfo } from "../lib/api";
  import { app, toast, errorText, deviceOf } from "../lib/state.svelte";
  import { emptyDashboard, newId, newPanel, normalize, type Dashboard, type Panel } from "../lib/dashboard";
  import { resolve, type TimeRange } from "../lib/time";
  import { confirmDialog } from "../lib/tauri";
  import TimeRangeBar from "../components/TimeRangeBar.svelte";
  import TimeSeriesChart from "../components/TimeSeriesChart.svelte";
  import StatPanel from "../components/StatPanel.svelte";
  import PanelEditor from "../components/PanelEditor.svelte";
  import Modal from "../components/Modal.svelte";

  const LAST_KEY = "tsd.dashboard";
  const REFRESH = [0, 1, 2, 5, 10, 30, 60];

  let list = $state<DashboardInfo[]>([]);
  let currentId = $state("");
  let dash = $state<Dashboard | null>(null);
  let history = $state<TimeRange[]>([]);
  let now = $state(Date.now());
  let tick = $state(0);
  let saveState = $state<"saved" | "saving" | "error">("saved");
  let editing = $state<Panel | null>(null);
  let nameDialog = $state<{ mode: "new" | "rename" | "copy"; name: string } | null>(null);
  let loadError = $state("");
  let lastSaved = "";
  let saveTimer: ReturnType<typeof setTimeout> | undefined;

  const range = $derived(dash ? resolve(dash.range, now) : { from: 0, to: 0 });

  async function loadList() {
    list = await api.dashboards();
  }

  /** 初回: 装置ごとに1パネルの既定ダッシュボードを作る */
  function defaultDashboard(): Dashboard {
    const d = emptyDashboard("メイン");
    const byDev = new Map<string, number[]>();
    for (const s of app.series) {
      const k = deviceOf(s);
      if (!byDev.has(k)) byDev.set(k, []);
      byDev.get(k)!.push(s.id);
    }
    for (const [dev, ids] of [...byDev.entries()].slice(0, 12)) d.panels.push({ ...newPanel(ids.slice(0, 10), dev) });
    return d;
  }

  async function open(id: string) {
    loadError = "";
    try {
      const raw = await api.dashboard(id);
      const d = normalize(raw, id);
      currentId = id;
      dash = d;
      lastSaved = JSON.stringify(d);
      history = [];
      now = Date.now();
      localStorage.setItem(LAST_KEY, id);
    } catch (e) {
      loadError = errorText(e);
    }
  }

  async function init() {
    try {
      await loadList();
      if (list.length === 0) {
        const d = defaultDashboard();
        await api.saveDashboard("main", d);
        await loadList();
      }
      const last = localStorage.getItem(LAST_KEY);
      await open(list.some((d) => d.id === last) ? last! : list[0].id);
    } catch (e) {
      loadError = errorText(e);
    }
  }

  onMount(init);

  // 系列ブラウザから「パネルに追加」されたもの
  $effect(() => {
    const p = app.pendingPanel;
    if (p && dash) {
      untrack(() => {
        dash!.panels.push({ ...newPanel(p.series, p.title), width: 12 });
        app.pendingPanel = null;
        toast("パネルを追加しました");
      });
    }
  });

  // 変更を自動保存（800ms まとめる）
  $effect(() => {
    if (!dash) return;
    const s = JSON.stringify(dash);
    if (s === lastSaved) return;
    clearTimeout(saveTimer);
    const id = currentId;
    saveTimer = setTimeout(async () => {
      saveState = "saving";
      try {
        await api.saveDashboard(id, JSON.parse(s));
        lastSaved = s;
        saveState = "saved";
        const info = list.find((d) => d.id === id);
        if (info && info.name !== dash?.name) await loadList();
      } catch (e) {
        saveState = "error";
        toast(`保存できません: ${errorText(e)}`, "error");
      }
    }, 800);
  });

  // 自動更新（現在までの表示のときだけ）
  let refreshTimer: ReturnType<typeof setInterval> | undefined;
  $effect(() => {
    const sec = dash?.refresh ?? 0;
    const relative = dash?.range.kind === "relative";
    clearInterval(refreshTimer);
    if (sec > 0 && relative) {
      refreshTimer = setInterval(() => {
        if (document.hidden) return;
        now = Date.now();
        tick++;
      }, sec * 1000);
    }
    return () => clearInterval(refreshTimer);
  });
  onDestroy(() => {
    clearInterval(refreshTimer);
    clearTimeout(saveTimer);
  });

  function setRange(r: TimeRange, push = true) {
    if (!dash) return;
    if (push) history = [...history.slice(-30), dash.range];
    dash.range = r;
    now = Date.now();
  }
  function back() {
    const prev = history[history.length - 1];
    if (!prev || !dash) return;
    history = history.slice(0, -1);
    dash.range = prev;
    now = Date.now();
  }
  function zoom(from: number, to: number) {
    setRange({ kind: "absolute", from: Math.round(from), to: Math.round(to) });
  }

  function addPanel() {
    if (!dash) return;
    const p = newPanel();
    dash.panels.push(p);
    editing = p;
  }
  function savePanel(p: Panel) {
    if (!dash) return;
    const i = dash.panels.findIndex((x) => x.id === p.id);
    if (i >= 0) dash.panels[i] = p;
    editing = null;
  }
  function cancelEdit() {
    // 追加直後のまま閉じた空のパネルは残さない
    const p = editing;
    if (dash && p && p.series.length === 0 && p.title === "新しいパネル") dash.panels = dash.panels.filter((x) => x.id !== p.id);
    editing = null;
  }
  function duplicate(p: Panel) {
    if (!dash) return;
    const i = dash.panels.indexOf(p);
    dash.panels.splice(i + 1, 0, { ...JSON.parse(JSON.stringify(p)), id: newId(), title: `${p.title}（コピー）` });
  }
  async function remove(p: Panel) {
    if (!dash) return;
    if (!(await confirmDialog(`パネル「${p.title}」を削除しますか？`))) return;
    dash.panels = dash.panels.filter((x) => x.id !== p.id);
  }
  function move(p: Panel, d: -1 | 1) {
    if (!dash) return;
    const i = dash.panels.indexOf(p);
    const j = i + d;
    if (j < 0 || j >= dash.panels.length) return;
    const arr = dash.panels.slice();
    [arr[i], arr[j]] = [arr[j], arr[i]];
    dash.panels = arr;
  }
  function cycleWidth(p: Panel) {
    const order: Panel["width"][] = [4, 6, 8, 12];
    p.width = order[(order.indexOf(p.width) + 1) % order.length];
  }

  async function submitName() {
    if (!nameDialog || !nameDialog.name.trim()) return;
    const name = nameDialog.name.trim();
    try {
      if (nameDialog.mode === "rename" && dash) {
        dash.name = name;
      } else {
        const id = newId("d");
        const d = nameDialog.mode === "copy" && dash ? { ...JSON.parse(JSON.stringify(dash)), name } : emptyDashboard(name);
        await api.saveDashboard(id, d);
        await loadList();
        await open(id);
      }
      nameDialog = null;
    } catch (e) {
      toast(errorText(e), "error");
    }
  }
  async function deleteDashboard() {
    if (!dash || list.length <= 1) return;
    if (!(await confirmDialog(`ダッシュボード「${dash.name}」を削除しますか？`))) return;
    await api.deleteDashboard(currentId);
    await loadList();
    await open(list[0].id);
  }
</script>

<div class="page">
  <div class="toolbar">
    <select class="dsel" value={currentId} onchange={(e) => open((e.target as HTMLSelectElement).value)}>
      {#each list as d (d.id)}<option value={d.id}>{d.name}</option>{/each}
    </select>
    <div class="menu-btns">
      <button onclick={() => (nameDialog = { mode: "new", name: "" })}>新規</button>
      <button onclick={() => dash && (nameDialog = { mode: "copy", name: `${dash.name} のコピー` })} disabled={!dash}>複製</button>
      <button onclick={() => dash && (nameDialog = { mode: "rename", name: dash.name })} disabled={!dash}>名前変更</button>
      <button onclick={deleteDashboard} disabled={list.length <= 1}>削除</button>
    </div>
    <span class="save {saveState}">{saveState === "saving" ? "保存中…" : saveState === "error" ? "保存エラー" : "保存済み"}</span>
    <div class="spacer"></div>
    {#if dash}
      <TimeRangeBar range={dash.range} canBack={history.length > 0} onchange={(r) => setRange(r)} onback={back} />
      <label class="refresh" title="自動更新の間隔（現在までの表示のときだけ動きます）">
        更新
        <select bind:value={dash.refresh}>
          {#each REFRESH as s}<option value={s}>{s === 0 ? "停止" : `${s}秒`}</option>{/each}
        </select>
      </label>
      <button class="icon-btn" title="今すぐ更新" onclick={() => { now = Date.now(); tick++; }}>⟳</button>
      <button class="primary" onclick={addPanel}>＋ パネル</button>
    {/if}
  </div>

  {#if loadError}
    <div class="notice err">ダッシュボードを読み込めません: {loadError}</div>
  {:else if dash}
    {#if dash.panels.length === 0}
      <div class="empty">
        <p>パネルがありません。</p>
        <button class="primary" onclick={addPanel}>＋ パネルを追加</button>
      </div>
    {/if}
    <div class="grid">
      {#each dash.panels as p, i (p.id)}
        <section class="panel" style:grid-column="span {p.width}">
          <header>
            <h3 title={p.title}>{p.title}</h3>
            <div class="tools">
              <button title="前へ" onclick={() => move(p, -1)} disabled={i === 0}>↑</button>
              <button title="後ろへ" onclick={() => move(p, 1)} disabled={i === dash.panels.length - 1}>↓</button>
              <button title="幅を変える" onclick={() => cycleWidth(p)}>⇔</button>
              <button title="複製" onclick={() => duplicate(p)}>⧉</button>
              <button title="編集" onclick={() => (editing = p)}>編集</button>
              <button title="削除" onclick={() => remove(p)}>×</button>
            </div>
          </header>
          {#if p.type === "stat"}
            <StatPanel series={p.series} {tick} height={p.height} unit={p.unit} decimals={p.decimals} />
          {:else}
            <TimeSeriesChart
              series={p.series}
              from={range.from}
              to={range.to}
              {tick}
              height={p.height}
              unit={p.unit}
              decimals={p.decimals}
              yMin={p.yMin}
              yMax={p.yMax}
              onzoom={zoom}
              onzoomout={back}
            />
          {/if}
        </section>
      {/each}
    </div>
    <p class="hint">グラフを横にドラッグすると拡大、ダブルクリックで戻ります。凡例のクリックで系列の表示を切り替えられます。</p>
  {/if}
</div>

{#if editing}
  <PanelEditor panel={editing} onsave={savePanel} onclose={cancelEdit} />
{/if}

{#if nameDialog}
  <Modal title={nameDialog.mode === "rename" ? "名前の変更" : nameDialog.mode === "copy" ? "ダッシュボードの複製" : "新しいダッシュボード"} onclose={() => (nameDialog = null)}>
    <form onsubmit={(e) => { e.preventDefault(); submitName(); }}>
      <!-- svelte-ignore a11y_autofocus -->
      <input class="full" bind:value={nameDialog.name} placeholder="名前" autofocus />
    </form>
    {#snippet footer()}
      <button onclick={() => (nameDialog = null)}>キャンセル</button>
      <button class="primary" onclick={submitName}>OK</button>
    {/snippet}
  </Modal>
{/if}

<style>
  .toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin-bottom: 12px; }
  .dsel { font-weight: 600; min-width: 160px; }
  .menu-btns { display: flex; gap: 2px; }
  .menu-btns button { font-size: 12px; padding: 4px 8px; }
  .save { font-size: 11px; color: var(--muted); }
  .save.error { color: var(--danger); }
  .spacer { flex: 1; }
  .refresh { display: flex; align-items: center; gap: 4px; font-size: 12px; color: var(--muted); }
  .icon-btn { width: 32px; padding: 4px 0; }
  .grid { display: grid; grid-template-columns: repeat(12, minmax(0, 1fr)); gap: 12px; }
  .panel { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 8px 10px 8px; min-width: 0; }
  .panel header { display: flex; align-items: center; gap: 6px; margin-bottom: 4px; }
  .panel h3 { font-size: 13px; font-weight: 600; margin: 0; flex: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .tools { display: flex; gap: 1px; opacity: 0; transition: opacity 0.15s; }
  .panel:hover .tools, .panel:focus-within .tools { opacity: 1; }
  .tools button { border: 0; background: none; padding: 2px 6px; font-size: 12px; color: var(--muted); }
  .tools button:hover:not(:disabled) { background: var(--bg-soft); color: var(--text); }
  .empty { text-align: center; padding: 60px 0; color: var(--muted); }
  .hint { font-size: 11px; color: var(--muted); margin-top: 10px; }
  .full { width: 100%; }
  @media (max-width: 1100px) {
    .panel { grid-column: span 12 !important; }
  }
</style>
