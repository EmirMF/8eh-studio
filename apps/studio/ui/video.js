(() => {
  const destinations = ["website", "tiktok"];
  const pending = new Map();
  let connected = false;
  let updating = false;
  let snapshot = null;
  let connectionError = "";
  let selectedOutput = "";
  let selectedCanvas = "";

  function fillSelect(id, items, value, placeholder) {
    const select = $(id);
    const choices = [[placeholder, ""], ...items];
    const changed = select.options.length !== choices.length || choices.some(([label, key], index) =>
      select.options[index]?.value !== key || select.options[index]?.text !== label
    );
    if (changed) select.replaceChildren(...choices.map(([label, key]) => new Option(label, key)));
    if (select.value !== value) select.value = items.some(([, key]) => key === value) ? value : "";
  }

  function render() {
    $("obs-connect").disabled = updating || connected;
    $("obs-port").disabled = updating || connected;
    $("obs-password").disabled = updating || connected;
    $("obs-disconnect").disabled = updating || !connected;
    const aitum = snapshot?.aitum_available;
    if (snapshot) {
      fillSelect("obs-tiktok-output", snapshot.tiktok_outputs.map((name) => [name, name]), selectedOutput, "Pilih output");
      fillSelect("obs-vertical-canvas", snapshot.vertical_canvases.map((canvas) => [canvas.name, canvas.uuid]), selectedCanvas, "Pilih canvas");
    }
    $("obs-tiktok-output").disabled = updating || !aitum || Boolean(snapshot?.tiktok?.active) || pending.has("tiktok");
    $("obs-vertical-canvas").disabled = updating || !aitum || Boolean(snapshot?.tiktok?.active) || pending.has("tiktok");
    feedback("obs-feedback", connectionError || (snapshot?.aitum_error
      ? "OBS terhubung. Aktifkan Aitum Stream Suite untuk mengontrol TikTok."
      : snapshot ? "OBS terhubung." : "Aktifkan WebSocket Server di OBS, lalu hubungkan di sini."),
    connectionError ? "warn" : snapshot ? "ok" : "");

    for (const destination of destinations) {
      const output = snapshot?.[destination];
      const operation = pending.get(destination);
      const state = !snapshot ? "Tidak diketahui"
        : !output ? "Belum dikonfigurasi"
        : operation ? (operation.start ? "Memulai…" : "Menghentikan…")
        : output.reconnecting ? "Menghubungkan ulang"
        : output.active ? (output.bytes > 0 ? "Output aktif" : "Menghubungkan…")
        : "Berhenti";
      const status = $("obs-" + destination + "-status");
      if (status.textContent !== state) status.textContent = state;
      const misconfigured = destination === "tiktok" && !$("obs-vertical-canvas").value;
      $("obs-" + destination + "-start").disabled =
        updating || !output || output.active || output.reconnecting || !!operation || misconfigured;
      $("obs-" + destination + "-stop").disabled =
        updating || !output || !(output.active || output.reconnecting) || !!operation;
      const sceneId = "obs-" + destination + "-scene";
      if (snapshot) {
        const scenes = snapshot[destination + "_scenes"];
        fillSelect(sceneId, scenes.map((name) => [name, name]), snapshot[destination + "_scene"] || "", "Pilih scene");
      }
      $(sceneId).disabled = updating || !snapshot?.[destination + "_scenes"].length;
    }
  }

  function updateStatus(next) {
    snapshot = next;
    connectionError = "";
    for (const [destination, operation] of pending) {
      const output = snapshot[destination];
      const confirmed = output && (operation.start
        ? output.active && output.bytes > 0 && !output.reconnecting
        : !output.active && !output.reconnecting);
      if (confirmed) {
        pending.delete(destination);
        feedback("obs-" + destination + "-feedback", operation.start ? "Output aktif." : "Output berhenti.", "ok");
      } else if (Date.now() >= operation.deadline) {
        pending.delete(destination);
        feedback("obs-" + destination + "-feedback", "OBS belum mengonfirmasi perubahan output. Periksa status di OBS.", "warn");
      }
    }
  }

  let pollInFlight = null;
  async function fetchStatus() {
    try {
      if ($("tab-video").hidden) {
        await invoke("obs_disconnect");
        connected = false;
        snapshot = null;
        pending.clear();
        return;
      }
      updateStatus(await invoke("obs_status"));
    } catch (error) {
      snapshot = null;
      connectionError = String(error);
      for (const destination of pending.keys()) {
        feedback("obs-" + destination + "-feedback", "Koneksi putus sebelum perubahan dikonfirmasi.", "warn");
      }
      pending.clear();
    } finally {
      render();
    }
  }

  function poll() {
    if (!connected || updating || pollInFlight) return;
    pollInFlight = fetchStatus().finally(() => { pollInFlight = null; });
    return pollInFlight;
  }

  function change(work, feedbackId = "obs-feedback") {
    return async () => {
      if (updating) return;
      updating = true;
      render();
      try {
        await pollInFlight;
        await work();
      } catch (error) {
        if (feedbackId === "obs-feedback") connectionError = String(error);
        else feedback(feedbackId, String(error), "err");
      } finally {
        updating = false;
        render();
      }
      if (snapshot && connected) await poll();
    };
  }

  $("obs-connect").onclick = change(async () => {
    const port = $("obs-port").valueAsNumber;
    if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("Masukkan port antara 1 dan 65535.");
    const config = await invoke("get_config");
    selectedOutput = config.obs_tiktok_output || "";
    selectedCanvas = config.obs_vertical_canvas || "";
    updateStatus(await invoke("obs_connect", { port, password: $("obs-password").value }));
    $("obs-password").value = "";
    connected = true;
  });
  $("obs-disconnect").onclick = change(async () => {
    await invoke("obs_disconnect");
    connected = false;
    snapshot = null;
    connectionError = "";
    pending.clear();
  });

  function saveTikTokSettings() {
    const output = $("obs-tiktok-output").value;
    const canvas = $("obs-vertical-canvas").value;
    return change(async () => {
      await invoke("obs_save_tiktok", { output: output || null, canvas: canvas || null });
      selectedOutput = output;
      selectedCanvas = canvas;
      feedback("obs-tiktok-feedback", "Pengaturan output tersimpan.", "ok");
    }, "obs-tiktok-feedback")();
  }
  $("obs-tiktok-output").onchange = saveTikTokSettings;
  $("obs-vertical-canvas").onchange = saveTikTokSettings;

  for (const destination of destinations) {
    const feedbackId = "obs-" + destination + "-feedback";
    const scene = $("obs-" + destination + "-scene");
    scene.onchange = () => {
      const name = scene.value;
      if (!name) return render();
      return change(async () => {
        await invoke("obs_set_scene", { destination, scene: name });
        feedback(feedbackId, "");
      }, feedbackId)();
    };
    for (const start of [true, false]) {
      $("obs-" + destination + (start ? "-start" : "-stop")).onclick = change(async () => {
        await invoke("obs_set_stream", { destination, start });
        pending.set(destination, { start, deadline: Date.now() + 15000 });
        feedback(feedbackId, "Menunggu konfirmasi OBS…");
      }, feedbackId);
    }
  }

  invoke("get_config").then((config) => {
    if (config.obs_port && $("obs-port").value === "4455") $("obs-port").value = config.obs_port;
  }).catch(() => {});
  setInterval(poll, 1500);
  render();
})();
