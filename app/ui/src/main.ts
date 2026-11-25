import { mount } from "svelte";
import "./app.css";
import App from "./App.svelte";
import { initApi } from "./lib/api";
import { startPolling } from "./lib/state.svelte";

initApi()
  .catch(() => "")
  .finally(() => {
    startPolling();
    mount(App, { target: document.getElementById("app")! });
  });
