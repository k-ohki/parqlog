<script lang="ts">
  // 表示期間の選択（プリセット、日時指定、前後への移動、拡大前に戻る）
  import { PRESETS, resolve, rangeLabel, toLocalInput, fromLocalInput, type TimeRange } from "../lib/time";

  interface Props {
    range: TimeRange;
    canBack: boolean;
    onchange: (r: TimeRange) => void;
    onback: () => void;
  }
  let { range, canBack, onchange, onback }: Props = $props();

  let open = $state(false);
  let fromStr = $state("");
  let toStr = $state("");

  function openMenu() {
    const r = resolve(range);
    fromStr = toLocalInput(r.from);
    toStr = toLocalInput(r.to);
    open = !open;
  }
  function applyCustom() {
    const f = fromLocalInput(fromStr);
    const t = fromLocalInput(toStr);
    if (f != null && t != null && t > f) {
      onchange({ kind: "absolute", from: f, to: t });
      open = false;
    }
  }
  function shift(dir: -1 | 1) {
    const r = resolve(range);
    const span = r.to - r.from;
    const d = (span / 2) * dir;
    const to = Math.min(r.to + d, Date.now());
    onchange({ kind: "absolute", from: to - span, to });
  }
  function zoomOut() {
    const r = resolve(range);
    const span = r.to - r.from;
    const to = Math.min(r.to + span / 2, Date.now());
    onchange({ kind: "absolute", from: to - span * 2, to });
  }
</script>

<div class="trb">
  <button class="icon" title="前へ" onclick={() => shift(-1)}>‹</button>
  <div class="wrap">
    <button class="current" onclick={openMenu} title="表示期間を選ぶ">{rangeLabel(range)} ▾</button>
    {#if open}
      <div class="menu" role="dialog">
        <div class="presets">
          {#each PRESETS as p}
            <button class:sel={range.kind === "relative" && range.ms === p.ms} onclick={() => { onchange({ kind: "relative", ms: p.ms }); open = false; }}>
              直近{p.label}
            </button>
          {/each}
        </div>
        <div class="custom">
          <label>開始 <input type="datetime-local" bind:value={fromStr} /></label>
          <label>終了 <input type="datetime-local" bind:value={toStr} /></label>
          <button class="primary" onclick={applyCustom}>この期間を表示</button>
        </div>
      </div>
    {/if}
  </div>
  <button class="icon" title="後へ" onclick={() => shift(1)} disabled={range.kind === "relative"}>›</button>
  <button class="icon" title="期間を2倍に広げる" onclick={zoomOut}>－</button>
  <button title="グラフをドラッグして拡大した場合に、拡大前の期間へ戻る（グラフのダブルクリックでも可）" onclick={onback} disabled={!canBack}>戻る</button>
  {#if range.kind === "absolute"}
    <button title="現在時刻までの表示に戻す" onclick={() => onchange({ kind: "relative", ms: Math.max(60_000, Math.round((range.kind === "absolute" ? range.to - range.from : 3_600_000) / 60_000) * 60_000) })}>現在まで</button>
  {/if}
</div>
{#if open}<div class="shield" role="presentation" onclick={() => (open = false)}></div>{/if}

<style>
  .trb { display: flex; align-items: center; gap: 4px; }
  .wrap { position: relative; }
  .current { min-width: 220px; text-align: left; font-variant-numeric: tabular-nums; }
  .icon { width: 30px; padding: 4px 0; font-size: 16px; line-height: 1; }
  .menu { position: absolute; top: calc(100% + 4px); right: 0; z-index: 40; background: var(--panel); border: 1px solid var(--line); border-radius: 8px; box-shadow: 0 10px 30px rgba(0, 0, 0, 0.12); display: flex; gap: 12px; padding: 10px; width: 460px; }
  .presets { display: flex; flex-direction: column; gap: 2px; width: 120px; }
  .presets button { text-align: left; border: 0; background: none; padding: 4px 8px; border-radius: 4px; }
  .presets button:hover, .presets button.sel { background: var(--accent-soft); }
  .custom { flex: 1; display: flex; flex-direction: column; gap: 8px; border-left: 1px solid var(--line); padding-left: 12px; }
  .custom label { display: flex; flex-direction: column; font-size: 12px; color: var(--muted); gap: 2px; }
  .shield { position: fixed; inset: 0; z-index: 30; }
</style>
