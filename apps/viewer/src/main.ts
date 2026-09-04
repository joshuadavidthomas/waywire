import "./app.css";
import { mount } from "svelte";

import App from "./App.svelte";

async function start() {
  if (new URLSearchParams(location.search).has("record")) {
    const { installRecorder } = await import(
      "../../../probes/compare/recorder.mjs"
    );
    installRecorder({
      transport: "vnc",
      canvas: () => document.querySelector<HTMLCanvasElement>("canvas"),
    });
  }
  mount(App, { target: document.getElementById("app")! });
}

void start();
