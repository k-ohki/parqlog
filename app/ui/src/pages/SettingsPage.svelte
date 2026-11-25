<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../lib/api";
  import { toast, errorText, refreshStatus } from "../lib/state.svelte";
  import { isTauri, native, confirmDialog, autostart } from "../lib/tauri";

  let cfg = $state<any>(null);
  let original = "";
  let path = $state("");
  let viaApi = $state(true);
  let error = $state("");
  let saving = $state(false);
  let auto = $state<boolean | null>(null);
  let shownToken = $state<Record<number, boolean>>({});

  async function load() {
    error = "";
    try {
      if (isTauri) {
        const r = await native.readConfig();
        path = r.path;
        viaApi = r.via_api;
        cfg = r.config;
        if (!cfg) error = "設定ファイルがありません。先に「サービス管理」からサービスを登録してください。";
      } else {
        const r = await api.config();
        path = r.path;
        cfg = r.config;
      }
      original = JSON.stringify(cfg);
    } catch (e) {
      error = errorText(e);
    }
    if (isTauri) {
      try {
        auto = await (await autostart()).get();
      } catch {
        auto = null;
      }
    }
  }
  onMount(load);

  const dirty = $derived(cfg != null && JSON.stringify(cfg) !== original);
  const years = (d: number) => `約 ${(d / 365).toFixed(1)} 年`;

  function newToken(): string {
    const b = new Uint8Array(24);
    crypto.getRandomValues(b);
    return btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
  function addToken() {
    cfg.auth.tokens.push({ name: `token${cfg.auth.tokens.length + 1}`, token: newToken() });
    shownToken[cfg.auth.tokens.length - 1] = true;
  }
  async function removeToken(i: number) {
    if (await confirmDialog(`トークン「${cfg.auth.tokens[i].name}」を削除しますか？このトークンを使っている送信元は書き込めなくなります。`)) {
      cfg.auth.tokens.splice(i, 1);
    }
  }
  async function copy(text: string) {
    await navigator.clipboard.writeText(text);
    toast("コピーしました");
  }

  function telegraf(tok: { token: string }): string {
    const port = (cfg.http.ingest_listen || cfg.http.listen).split(":").pop();
    const host = cfg.http.ingest_listen ? "<この PC の IP アドレス>" : "127.0.0.1";
    return `[[outputs.influxdb_v2]]
  urls = ["http://${host}:${port}"]
  token = "${tok.token}"
  organization = "tsd"
  bucket = "tsd"`;
  }

  async function save() {
    saving = true;
    try {
      const plain = JSON.parse(JSON.stringify(cfg));
      // 数値に戻す（入力欄は文字列になることがある）
      for (const sec of ["retention", "monitor", "storage", "http"]) {
        for (const [k, v] of Object.entries(plain[sec])) {
          if (typeof original === "string" && typeof JSON.parse(original)[sec]?.[k] === "number") plain[sec][k] = Number(v);
        }
      }
      let restart = true;
      if (isTauri) restart = (await native.writeConfig(plain)).restart_required;
      else restart = (await api.saveConfig(plain)).restart_required;
      original = JSON.stringify(cfg);
      if (isTauri && restart && viaApi) {
        if (await confirmDialog("設定を保存しました。反映するにはサービスの再起動が必要です。今すぐ再起動しますか？")) {
          await native.serviceRestart();
          toast("サービスを再起動しました");
          setTimeout(refreshStatus, 1000);
        }
      } else {
        toast(restart ? "保存しました（サービスの再起動で反映されます）" : "保存しました");
      }
    } catch (e) {
      toast(`保存できません: ${errorText(e)}`, "error");
    } finally {
      saving = false;
    }
  }

  async function setAuto(on: boolean) {
    try {
      await (await autostart()).set(on);
      auto = on;
    } catch (e) {
      toast(errorText(e), "error");
    }
  }
</script>

<div class="page">
  {#if error}<div class="notice err">{error}</div>{/if}
  {#if cfg}
    <div class="head">
      <span class="path">{path}{isTauri && !viaApi ? "（サービス停止中のためファイルを直接読みました。保存時に管理者の認証が出ます）" : ""}</span>
      <div class="grow"></div>
      <button onclick={load} disabled={saving}>元に戻す</button>
      <button class="primary" onclick={save} disabled={!dirty || saving}>{saving ? "保存中…" : "保存"}</button>
    </div>

    <div class="cols">
      <section class="card">
        <h2>通信</h2>
        <label>API の待ち受け（画面とローカルの送信元）<input bind:value={cfg.http.listen} placeholder="127.0.0.1:8086" /></label>
        <p class="help">既定はこの PC からのみ。Telegraf は <code>http://127.0.0.1:8086</code> に送れます。</p>
        <label>LAN 向け取り込みポート（空欄で無効）<input bind:value={cfg.http.ingest_listen} placeholder="0.0.0.0:8087" /></label>
        <p class="help">他の PC から送る場合に設定します。書き込み専用で、トークンが必須です。ファイアウォール（ufw など）でこのポートを開けてください。</p>
        <label class="chk"><input type="checkbox" bind:checked={cfg.auth.require_token_on_loopback} /> この PC からの書き込みにもトークンを求める</label>
      </section>

      <section class="card">
        <h2>トークン</h2>
        {#each cfg.auth.tokens as t, i}
          <div class="tok">
            <input class="tname" bind:value={t.name} />
            <input class="tval" type={shownToken[i] ? "text" : "password"} bind:value={t.token} readonly />
            <button onclick={() => (shownToken[i] = !shownToken[i])}>{shownToken[i] ? "隠す" : "表示"}</button>
            <button onclick={() => copy(t.token)}>コピー</button>
            <button class="danger" onclick={() => removeToken(i)}>削除</button>
          </div>
          {#if shownToken[i]}
            <div class="snippet">
              <div class="sh">Telegraf の設定例 <button class="link" onclick={() => copy(telegraf(t))}>コピー</button></div>
              <pre>{telegraf(t)}</pre>
            </div>
          {/if}
        {:else}
          <p class="help">トークンはありません。</p>
        {/each}
        <button onclick={addToken}>＋ トークンを発行</button>
      </section>

      <section class="card">
        <h2>保存と保持期間</h2>
        <label>データ保存先<input bind:value={cfg.storage.data_dir} /></label>
        <p class="help">変更しても既存のデータは移動しません。サービスを止めてフォルダごと移してから変更してください。</p>
        <div class="g3">
          <label>生データ（日）<input type="number" min="1" bind:value={cfg.retention.raw_days} /><small>{years(cfg.retention.raw_days)}</small></label>
          <label>1分集計（日）<input type="number" min="1" bind:value={cfg.retention.m1_days} /><small>{years(cfg.retention.m1_days)}</small></label>
          <label>1時間集計（日）<input type="number" min="1" bind:value={cfg.retention.h1_days} /><small>{years(cfg.retention.h1_days)}</small></label>
          <label>遅れデータの猶予（日）<input type="number" min="0" max="30" bind:value={cfg.retention.grace_days} /></label>
          <label>16bit 化までの日数<input type="number" min="0" bind:value={cfg.retention.compact_after_days} /><small>0 で無効</small></label>
          <label>DuckDB のメモリ上限<input bind:value={cfg.storage.memory_limit} placeholder="1GB" /></label>
        </div>
        <p class="help">値は 32bit で保存します。16bit 化した日は、系列ごとにその日の最小〜最大を 65,536 段階で表します（誤差はその日の値幅 ÷ 131,070 以下）。</p>
      </section>

      <section class="card">
        <h2>監視</h2>
        <div class="g3">
          <label>装置を表すタグ<input bind:value={cfg.monitor.device_tag} /></label>
          <label>欠測とみなす秒数<input type="number" min="1" bind:value={cfg.monitor.stale_seconds} /></label>
          <label>ディスク警告（GB）<input type="number" min="0" bind:value={cfg.monitor.disk_warn_gb} /></label>
        </div>
      </section>

      {#if isTauri}
        <section class="card">
          <h2>このアプリ</h2>
          <label class="chk">
            <input type="checkbox" checked={!!auto} disabled={auto == null} onchange={(e) => setAuto((e.target as HTMLInputElement).checked)} />
            ログオン時にタスクトレイで起動する
          </label>
          <p class="help">ウィンドウを閉じてもタスクトレイに残ります。終了はトレイのメニューから。収集はサービス（systemd）が行うため、アプリを終了しても止まりません。</p>
        </section>
      {/if}
    </div>
  {/if}
</div>

<style>
  .head { display: flex; gap: 8px; align-items: center; margin-bottom: 12px; }
  .path { font-family: var(--mono); font-size: 12px; color: var(--muted); }
  .grow { flex: 1; }
  .cols { display: grid; grid-template-columns: repeat(auto-fit, minmax(420px, 1fr)); gap: 14px; align-items: start; }
  .card { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 12px 16px; display: flex; flex-direction: column; gap: 8px; }
  h2 { font-size: 14px; margin: 0 0 4px; }
  label { display: flex; flex-direction: column; gap: 3px; font-size: 12px; color: var(--muted); }
  label small { color: var(--muted); }
  .chk { flex-direction: row; align-items: center; gap: 6px; color: var(--text); font-size: 13px; }
  .help { font-size: 12px; color: var(--muted); margin: -2px 0 4px; }
  .g3 { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 10px; }
  .g3 input, label > input { width: 100%; }
  .tok { display: flex; gap: 4px; align-items: center; }
  .tname { width: 110px; }
  .tval { flex: 1; font-family: var(--mono); font-size: 12px; }
  .snippet { background: var(--bg-soft); border-radius: 6px; padding: 6px 10px; }
  .sh { font-size: 12px; color: var(--muted); display: flex; gap: 8px; }
  pre { margin: 4px 0 0; font-size: 12px; font-family: var(--mono); white-space: pre-wrap; }
  code { font-family: var(--mono); }
</style>
