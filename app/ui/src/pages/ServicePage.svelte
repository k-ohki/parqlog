<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { api } from "../lib/api";
  import { app, refreshStatus, toast, errorText } from "../lib/state.svelte";
  import { isTauri, native, confirmDialog, type ServiceInfo } from "../lib/tauri";
  import { fmtBytes, fmtDateTime, fmtNum, fmtAgo } from "../lib/time";

  let svc = $state<ServiceInfo | null>(null);
  let busy = $state("");
  let logs = $state("");
  let logAuto = $state(false);
  let backupDest = $state("");

  const JOB_LABEL: Record<string, string> = {
    rollup_1m: "1分集計",
    rollup_1h: "1時間集計",
    flush_daily: "日次フラッシュ",
    flush_monthly: "月次フラッシュ",
    compact_16bit: "16bit 再圧縮",
    retention: "保持期間管理",
  };
  const JOB_ORDER = Object.keys(JOB_LABEL);
  const WM_LABEL: Record<string, string> = {
    raw_accept_from: "受付開始（これより前は拒否）",
    raw_flushed_until: "生データ確定（Parquet）",
    raw_compacted_until: "16bit 再圧縮済み",
    m1_until: "1分集計済み",
    h1_until: "1時間集計済み",
    m1_flushed_until: "1分集計確定（Parquet）",
  };

  async function loadSvc() {
    if (!isTauri) return;
    try {
      svc = await native.serviceStatus();
    } catch (e) {
      toast(errorText(e), "error");
    }
  }
  async function loadLogs() {
    if (!isTauri) return;
    try {
      logs = await native.readLogTail(400);
    } catch (e) {
      logs = errorText(e);
    }
  }

  onMount(() => {
    loadSvc();
    loadLogs();
  });
  const t = setInterval(() => {
    loadSvc();
    if (logAuto) loadLogs();
  }, 3000);
  onDestroy(() => clearInterval(t));

  async function run(label: string, f: () => Promise<void>, confirmMsg?: string) {
    if (confirmMsg && !(await confirmDialog(confirmMsg))) return;
    busy = label;
    try {
      await f();
      toast(`${label}しました`);
    } catch (e) {
      toast(`${label}できません: ${errorText(e)}`, "error");
    } finally {
      busy = "";
      await loadSvc();
      setTimeout(refreshStatus, 800);
      loadLogs();
    }
  }

  async function backup() {
    if (!backupDest.trim()) return;
    busy = "バックアップ";
    try {
      const r = await api.backup(backupDest.trim());
      toast(`バックアップしました（${r.files_copied} ファイルをコピー、${r.files_skipped} ファイルは変更なし）`);
    } catch (e) {
      toast(`バックアップできません: ${errorText(e)}`, "error");
    } finally {
      busy = "";
    }
  }

  const st = $derived(app.status);
  const stateLabel: Record<string, string> = {
    running: "稼働中",
    stopped: "停止",
    start_pending: "開始中…",
    stop_pending: "停止中…",
    failed: "異常終了",
    other: "不明",
    unsupported: "（このOSでは操作不可）",
  };
</script>

<div class="page cols">
  <div class="col">
    <section class="card">
      <h2>サービス</h2>
      {#if isTauri}
        {#if svc && svc.state === "unsupported"}
          <p class="muted">systemctl が使えないため、サービスの操作はできません。</p>
        {:else if svc}
          <div class="svc">
            <span class="badge {svc.installed ? (svc.state === 'failed' ? 'stopped' : svc.state) : 'none'}">{svc.installed ? stateLabel[svc.state] : "未登録"}</span>
            {#if svc.pid}<span class="muted">PID {svc.pid}</span>{/if}
          </div>
          <div class="btns">
            {#if svc.installed}
              <button onclick={() => run("開始", native.serviceStart)} disabled={!!busy || svc.state === "running"}>開始</button>
              <button onclick={() => run("停止", native.serviceStop, "サービスを停止すると、その間のデータは受信できません（送信側が再送しない場合は欠測になります）。停止しますか？")} disabled={!!busy || svc.state === "stopped"}>停止</button>
              <button onclick={() => run("再起動", native.serviceRestart)} disabled={!!busy}>再起動</button>
            {:else}
              <p class="muted">tsd.service が登録されていません（README の手順で systemd に登録してください）。</p>
            {/if}
          </div>
          {#if busy}<p class="muted">{busy}中…</p>{/if}
        {/if}
      {:else}
        <p class="muted">サービスの開始・停止はデスクトップアプリから行えます。</p>
      {/if}
      {#if st}
        <dl>
          <dt>バージョン</dt><dd>{st.version}</dd>
          <dt>稼働時間</dt><dd>{fmtNum(st.uptime_s / 3600, 1)} 時間</dd>
          <dt>API</dt><dd>{st.config.listen}</dd>
          <dt>LAN 取り込み</dt><dd>{st.config.ingest_listen || "無効"}</dd>
          <dt>設定ファイル</dt><dd class="path">{st.config.config_path}</dd>
          <dt>データフォルダ</dt><dd class="path">{st.config.data_dir}</dd>
        </dl>
      {:else if !app.connected}
        <p class="err">サービスに接続できません: {app.connectionError}</p>
      {/if}
    </section>

    {#if st}
      <section class="card">
        <h2>取り込み</h2>
        <div class="kpis">
          <div><b>{fmtNum(st.ingest.points_per_sec, 0)}</b><span>点/秒</span></div>
          <div><b>{fmtNum(st.ingest.written_points, 0)}</b><span>書き込み（起動後）</span></div>
          <div><b>{st.store.series_count}</b><span>系列</span></div>
          <div class:warn={st.ingest.buffer_len > 100000}><b>{fmtNum(st.ingest.buffer_len, 0)}</b><span>未書き込み</span></div>
        </div>
        <dl>
          <dt>拒否（古すぎ）</dt><dd>{fmtNum(st.ingest.rejected_old, 0)}</dd>
          <dt>拒否（未来）</dt><dd>{fmtNum(st.ingest.rejected_future, 0)}</dd>
          <dt>構文エラー行</dt><dd>{fmtNum(st.ingest.parse_errors, 0)}</dd>
          <dt>破棄した文字列値</dt><dd>{fmtNum(st.ingest.skipped_fields, 0)}</dd>
          <dt>書き込みエラー</dt><dd class:err={st.ingest.write_errors > 0}>{st.ingest.write_errors}{st.ingest.last_write_error ? `（${st.ingest.last_write_error}）` : ""}</dd>
        </dl>
      </section>

      <section class="card">
        <h2>ディスク</h2>
        {#if st.disk.total_bytes > 0}
          {@const used = 1 - st.disk.free_bytes / st.disk.total_bytes}
          <div class="bar"><div class="fill" class:warn={st.warnings.some((w) => w.includes("ディスク"))} style:width="{used * 100}%"></div></div>
          <p class="muted">空き {fmtBytes(st.disk.free_bytes)} / {fmtBytes(st.disk.total_bytes)}</p>
        {/if}
        <dl>
          <dt>データ合計</dt><dd>{fmtBytes(st.disk.data_bytes)}（{st.disk.file_count} ファイル）</dd>
          <dt>生データ</dt><dd>{fmtBytes(st.disk.raw_bytes)}</dd>
          <dt>1分集計</dt><dd>{fmtBytes(st.disk.m1_bytes)}</dd>
          <dt>DuckDB</dt><dd>{fmtBytes(st.store.db_file_bytes)}</dd>
          <dt>直近の生データ</dt><dd>{fmtNum(st.store.raw_hot_rows, 0)} 行</dd>
          <dt>当月の1分集計</dt><dd>{fmtNum(st.store.r1m_hot_rows, 0)} 行</dd>
          <dt>1時間集計</dt><dd>{fmtNum(st.store.r1h_rows, 0)} 行</dd>
        </dl>
      </section>
    {/if}
  </div>

  <div class="col">
    {#if st}
      {#if st.warnings.length > 0}
        <section class="card warnbox">
          <h2>注意</h2>
          <ul>{#each st.warnings as w}<li>{w}</li>{/each}</ul>
        </section>
      {/if}
      <section class="card">
        <h2>定期ジョブ</h2>
        <table>
          <thead><tr><th>ジョブ</th><th>最後に処理</th><th class="r">所要</th><th>状態</th></tr></thead>
          <tbody>
            {#each [...st.store.jobs].sort((a, b) => JOB_ORDER.indexOf(a.name) - JOB_ORDER.indexOf(b.name)) as j (j.name)}
              <tr>
                <td>{JOB_LABEL[j.name] ?? j.name}</td>
                <td title={fmtDateTime(j.last_work)}>{j.last_work ? fmtAgo(j.last_work) : "—"}</td>
                <td class="r">{j.last_duration_ms != null ? `${j.last_duration_ms}ms` : "—"}</td>
                <td class:err={!!j.last_error} title={j.last_error ?? ""}>{j.last_error ? "エラー" : "正常"}</td>
              </tr>
            {/each}
          </tbody>
        </table>
        <h3>境界</h3>
        <dl>
          {#each Object.keys(WM_LABEL).filter((k) => k in st.store.watermarks).map((k) => [k, st.store.watermarks[k]] as const) as [k, v]}
            <dt>{WM_LABEL[k] ?? k}</dt><dd>{fmtDateTime(v, false)}</dd>
          {/each}
        </dl>
      </section>

      <section class="card">
        <h2>バックアップ</h2>
        <p class="muted">DB をチェックポイントしてから複製し、Parquet ファイルは差分だけコピーします。</p>
        <div class="row">
          <input class="grow" bind:value={backupDest} placeholder="バックアップ先フォルダ（例: /mnt/backup/tsd）" />
          <button onclick={backup} disabled={!!busy || !backupDest.trim()}>実行</button>
        </div>
      </section>
    {/if}

    {#if isTauri}
      <section class="card">
        <h2>ログ（journalctl -u tsd）</h2>
        <div class="row">
          <button onclick={loadLogs}>再読み込み</button>
          <label class="chk"><input type="checkbox" bind:checked={logAuto} /> 自動更新</label>
        </div>
        <pre class="log">{logs}</pre>
      </section>
    {/if}
  </div>
</div>

<style>
  .cols { display: grid; grid-template-columns: minmax(340px, 1fr) minmax(400px, 1.3fr); gap: 14px; align-items: start; }
  .col { display: flex; flex-direction: column; gap: 14px; min-width: 0; }
  .card { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 12px 16px; }
  h2 { font-size: 14px; margin: 0 0 10px; }
  h3 { font-size: 12px; margin: 14px 0 6px; color: var(--muted); }
  .svc { display: flex; gap: 10px; align-items: center; margin-bottom: 10px; }
  .badge { padding: 3px 10px; border-radius: 99px; font-size: 13px; font-weight: 600; background: var(--bg-soft); }
  .badge.running { background: var(--ok-soft); color: var(--ok); }
  .badge.stopped, .badge.none { background: var(--danger-soft); color: var(--danger); }
  .btns { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 10px; }
  dl { display: grid; grid-template-columns: max-content 1fr; gap: 4px 14px; font-size: 13px; margin: 0; }
  dt { color: var(--muted); }
  dd { margin: 0; font-variant-numeric: tabular-nums; min-width: 0; }
  .path { font-family: var(--mono); font-size: 11px; word-break: break-all; }
  .kpis { display: grid; grid-template-columns: repeat(4, 1fr); gap: 8px; margin-bottom: 12px; }
  .kpis div { background: var(--bg-soft); border-radius: 6px; padding: 8px; display: flex; flex-direction: column; }
  .kpis b { font-size: 20px; font-variant-numeric: tabular-nums; }
  .kpis span { font-size: 11px; color: var(--muted); }
  .kpis .warn b { color: var(--warn); }
  .bar { height: 10px; border-radius: 5px; background: var(--bg-soft); overflow: hidden; }
  .fill { height: 100%; background: var(--accent); }
  .fill.warn { background: var(--danger); }
  table { width: 100%; border-collapse: collapse; font-size: 13px; }
  th { text-align: left; color: var(--muted); font-weight: 500; font-size: 12px; border-bottom: 1px solid var(--line); padding: 4px 6px; }
  td { padding: 4px 6px; border-bottom: 1px solid var(--line-soft); }
  .r { text-align: right; }
  .err { color: var(--danger); }
  .muted { color: var(--muted); font-size: 12px; }
  .warnbox { border-color: var(--warn); background: var(--warn-soft); }
  .warnbox ul { margin: 0; padding-left: 18px; font-size: 13px; }
  .row { display: flex; gap: 8px; align-items: center; margin-bottom: 8px; }
  .grow { flex: 1; }
  .chk { font-size: 12px; display: flex; gap: 4px; align-items: center; }
  .log { background: #0f172a; color: #d1d5db; font-family: var(--mono); font-size: 11px; line-height: 1.45; padding: 10px; border-radius: 6px; height: 340px; overflow: auto; white-space: pre-wrap; word-break: break-all; margin: 0; }
  @media (max-width: 1100px) { .cols { grid-template-columns: 1fr; } }
</style>
