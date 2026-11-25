// Tauri 上で動いているときだけ Rust 側のコマンドを呼ぶ。ブラウザで開いた場合は使えない機能を隠す。

export const isTauri: boolean = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri) throw new Error("この操作はデスクトップアプリでのみ使えます");
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

export interface ServiceInfo {
  installed: boolean;
  state: "running" | "stopped" | "start_pending" | "stop_pending" | "failed" | "other" | "unsupported";
  pid: number | null;
}

export const native = {
  apiBase: () => call<string>("api_base"),
  serviceStatus: () => call<ServiceInfo>("service_status"),
  serviceStart: () => call<void>("service_start"),
  serviceStop: () => call<void>("service_stop"),
  serviceRestart: () => call<void>("service_restart"),
  readConfig: () => call<{ path: string; via_api: boolean; config: any | null }>("read_config"),
  writeConfig: (config: unknown) => call<{ restart_required: boolean }>("write_config", { config }),
  readLogTail: (lines = 300) => call<string>("read_log_tail", { lines }),
  importCsv: (path: string, params: Record<string, string>) => call<any>("import_csv", { path, params }),
  exportToFile: (request: unknown, path: string) =>
    call<{ bytes: number; rows: number | null }>("export_to_file", { request, path }),
};

export async function openCsvDialog(): Promise<string | null> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const r = await open({ multiple: false, filters: [{ name: "CSV", extensions: ["csv", "txt"] }] });
  return typeof r === "string" ? r : null;
}

export async function saveDialog(defaultPath: string, ext: string): Promise<string | null> {
  const { save } = await import("@tauri-apps/plugin-dialog");
  return save({ defaultPath, filters: [{ name: ext.toUpperCase(), extensions: [ext] }] });
}

export async function confirmDialog(message: string, title = "TSD"): Promise<boolean> {
  if (!isTauri) return window.confirm(message);
  const { ask } = await import("@tauri-apps/plugin-dialog");
  return ask(message, { title, kind: "warning" });
}

export async function autostart(): Promise<{ get: () => Promise<boolean>; set: (on: boolean) => Promise<void> }> {
  const m = await import("@tauri-apps/plugin-autostart");
  return { get: () => m.isEnabled(), set: (on) => (on ? m.enable() : m.disable()) };
}
