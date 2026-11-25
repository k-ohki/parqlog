<script lang="ts">
  import { app, go, type Page } from "./lib/state.svelte";
  import { fmtNum } from "./lib/time";
  import Dashboard from "./pages/Dashboard.svelte";
  import SeriesBrowser from "./pages/SeriesBrowser.svelte";
  import DeviceMonitor from "./pages/DeviceMonitor.svelte";
  import ServicePage from "./pages/ServicePage.svelte";
  import SettingsPage from "./pages/SettingsPage.svelte";
  import ExportPage from "./pages/ExportPage.svelte";

  const NAV: { page: Page; label: string; icon: string }[] = [
    { page: "dashboard", label: "ダッシュボード", icon: "M3 3h8v8H3zM13 3h8v5h-8zM13 10h8v11h-8zM3 13h8v8H3z" },
    { page: "series", label: "系列ブラウザ", icon: "M4 6h16M4 12h16M4 18h10" },
    { page: "devices", label: "装置モニター", icon: "M3 5h18v11H3zM8 20h8M12 16v4" },
    { page: "export", label: "CSV", icon: "M12 3v12M7 10l5 5 5-5M4 20h16" },
    { page: "service", label: "サービス管理", icon: "M12 3v4M12 17v4M3 12h4M17 12h4M6 6l2.5 2.5M15.5 15.5L18 18M6 18l2.5-2.5M15.5 8.5L18 6" },
    { page: "settings", label: "設定", icon: "M4 7h10M18 7h2M4 17h4M12 17h8M14 5v4M8 15v4" },
  ];
  const TITLES: Record<Page, string> = {
    dashboard: "ダッシュボード", series: "系列ブラウザ", devices: "装置モニター", export: "CSV の取り込み・書き出し", service: "サービス管理", settings: "設定",
  };
  const st = $derived(app.status);
  const level = $derived(!app.connected ? "down" : st && st.warnings.length > 0 ? "warn" : "ok");
</script>

<div class="shell">
  <nav>
    <div class="brand"><span class="logo"></span>TSD</div>
    {#each NAV as n}
      <button class:active={app.page === n.page} onclick={() => go(n.page)}>
        <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d={n.icon} /></svg>
        <span>{n.label}</span>
        {#if n.page === "devices" && st && st.devices.stale > 0}<span class="pill">{st.devices.stale}</span>{/if}
      </button>
    {/each}
    <div class="grow"></div>
    <button class="status {level}" onclick={() => go("service")} title={st?.warnings.join("\n") ?? app.connectionError}>
      <span class="dot"></span>
      <span>
        {#if !app.connected}サービスに接続できません
        {:else if st}{fmtNum(st.ingest.points_per_sec, 0)} 点/秒{st.warnings.length ? ` · 注意 ${st.warnings.length}` : ""}
        {:else}接続中…{/if}
      </span>
    </button>
  </nav>
  <main>
    <h1>{TITLES[app.page]}</h1>
    {#if !app.connected && app.page !== "service" && app.page !== "settings"}
      <div class="notice err">サービスに接続できません（{app.connectionError}）。「サービス管理」で状態を確認してください。</div>
    {/if}
    {#if app.page === "dashboard"}<Dashboard />
    {:else if app.page === "series"}<SeriesBrowser />
    {:else if app.page === "devices"}<DeviceMonitor />
    {:else if app.page === "export"}<ExportPage />
    {:else if app.page === "service"}<ServicePage />
    {:else if app.page === "settings"}<SettingsPage />{/if}
  </main>
</div>

<div class="toasts">
  {#each app.toasts as t (t.id)}<div class="toast {t.kind}">{t.text}</div>{/each}
</div>

<style>
  .shell { display: flex; height: 100vh; }
  nav { width: 196px; flex: none; background: #111827; color: #cbd5e1; display: flex; flex-direction: column; padding: 12px 8px; gap: 2px; }
  .brand { display: flex; align-items: center; gap: 8px; font-weight: 700; color: #fff; font-size: 16px; padding: 4px 10px 14px; letter-spacing: 0.04em; }
  .logo { width: 18px; height: 18px; border-radius: 50%; background: conic-gradient(#22a05a 0 70%, #2563eb 0); }
  nav button { display: flex; align-items: center; gap: 10px; background: none; border: 0; color: inherit; padding: 8px 10px; border-radius: 6px; font: inherit; font-size: 13px; text-align: left; cursor: pointer; }
  nav button:hover { background: #1f2937; color: #fff; }
  nav button.active { background: #2563eb; color: #fff; }
  .pill { margin-left: auto; background: var(--danger); color: #fff; border-radius: 99px; font-size: 11px; padding: 0 7px; }
  .grow { flex: 1; }
  .status { font-size: 12px !important; }
  .status .dot { width: 9px; height: 9px; border-radius: 50%; flex: none; }
  .status.ok .dot { background: #22c55e; }
  .status.warn .dot { background: #f59e0b; }
  .status.down .dot { background: #ef4444; }
  main { flex: 1; overflow: auto; padding: 16px 20px 30px; min-width: 0; }
  h1 { font-size: 18px; margin: 0 0 12px; }
  .toasts { position: fixed; right: 16px; bottom: 16px; display: flex; flex-direction: column; gap: 8px; z-index: 100; }
  .toast { background: #111827; color: #fff; padding: 10px 14px; border-radius: 8px; font-size: 13px; max-width: 420px; box-shadow: 0 6px 20px rgba(0, 0, 0, 0.2); }
  .toast.error { background: var(--danger); }
</style>
