<script lang="ts">
  import type { Snippet } from "svelte";
  interface Props {
    title: string;
    onclose: () => void;
    children: Snippet;
    footer?: Snippet;
    wide?: boolean;
  }
  let { title, onclose, children, footer, wide = false }: Props = $props();
  function key(e: KeyboardEvent) {
    if (e.key === "Escape") onclose();
  }
</script>

<svelte:window onkeydown={key} />
<div class="backdrop" role="presentation" onmousedown={(e) => e.target === e.currentTarget && onclose()}>
  <div class="modal" class:wide role="dialog" aria-modal="true" aria-label={title}>
    <header><h2>{title}</h2><button class="x" onclick={onclose} aria-label="閉じる">×</button></header>
    <div class="body">{@render children()}</div>
    {#if footer}<footer>{@render footer()}</footer>{/if}
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(15, 23, 42, 0.35); display: flex; align-items: center; justify-content: center; z-index: 50; }
  .modal { background: var(--panel); border-radius: 10px; width: min(560px, 94vw); max-height: 90vh; display: flex; flex-direction: column; box-shadow: 0 20px 50px rgba(0, 0, 0, 0.2); }
  .modal.wide { width: min(880px, 96vw); }
  header { display: flex; align-items: center; padding: 14px 18px 8px; }
  h2 { font-size: 16px; margin: 0; flex: 1; }
  .x { border: 0; background: none; font-size: 22px; line-height: 1; cursor: pointer; color: var(--muted); }
  .body { padding: 8px 18px 14px; overflow: auto; }
  footer { display: flex; justify-content: flex-end; gap: 8px; padding: 12px 18px; border-top: 1px solid var(--line); }
</style>
