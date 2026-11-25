<script lang="ts">
  import Modal from "./Modal.svelte";
  import SeriesPicker from "./SeriesPicker.svelte";
  import type { Panel } from "../lib/dashboard";

  interface Props {
    panel: Panel;
    onsave: (p: Panel) => void;
    onclose: () => void;
  }
  let { panel, onsave, onclose }: Props = $props();

  // 編集用のコピー（開いた時点の内容で固定する）
  // svelte-ignore state_referenced_locally
  let p = $state<Panel>(JSON.parse(JSON.stringify(panel)));
  const numOrNull = (v: unknown) => (v === "" || v == null || Number.isNaN(Number(v)) ? null : Number(v));

  function save() {
    onsave({
      ...p,
      title: p.title.trim() || "無題",
      decimals: numOrNull(p.decimals),
      yMin: numOrNull(p.yMin),
      yMax: numOrNull(p.yMax),
      height: Math.min(900, Math.max(140, Number(p.height) || 280)),
    });
  }
</script>

<Modal title="パネルの編集" {onclose} wide>
  <div class="form">
    <label>タイトル <input bind:value={p.title} /></label>
    <label>種類
      <select bind:value={p.type}>
        <option value="timeseries">時系列グラフ</option>
        <option value="stat">最新値（数字）</option>
      </select>
    </label>
    <label>幅
      <select bind:value={p.width}>
        <option value={4}>1/3</option>
        <option value={6}>1/2</option>
        <option value={8}>2/3</option>
        <option value={12}>全幅</option>
      </select>
    </label>
    <label>高さ (px) <input type="number" min="140" max="900" step="20" bind:value={p.height} /></label>
    <label>単位 <input bind:value={p.unit} placeholder="系列の単位を使う" /></label>
    <label>小数桁 <input type="number" min="0" max="6" bind:value={p.decimals} placeholder="自動" /></label>
    {#if p.type === "timeseries"}
      <label>縦軸の最小 <input type="number" bind:value={p.yMin} placeholder="自動" /></label>
      <label>縦軸の最大 <input type="number" bind:value={p.yMax} placeholder="自動" /></label>
    {/if}
  </div>
  <h3>系列</h3>
  <SeriesPicker bind:selected={p.series} max={p.type === "stat" ? 24 : 20} height={260} />
  {#snippet footer()}
    <button onclick={onclose}>キャンセル</button>
    <button class="primary" onclick={save}>適用</button>
  {/snippet}
</Modal>

<style>
  .form { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 10px 12px; }
  .form input, .form select { width: 100%; }
  .form label:first-child { grid-column: span 2; }
  label { display: flex; flex-direction: column; gap: 3px; font-size: 12px; color: var(--muted); }
  h3 { font-size: 13px; margin: 16px 0 6px; }
</style>
