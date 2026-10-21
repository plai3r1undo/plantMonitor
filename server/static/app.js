"use strict";

const REFRESH_MS = 60_000;
const GAP_SECS = 45 * 60; // break chart lines when readings are further apart

const STATUS_LABEL = {
  ok: "Healthy",
  attention: "Needs attention",
  offline: "Offline",
  no_data: "No data",
};

// Charts in the detail dialog: reading field, title, unit, check metric.
const CHARTS = [
  { field: "soil_moisture", title: "Soil moisture", unit: "%", range: "soil_moisture" },
  { field: "lux", title: "Light", unit: "lx", range: "light_lux" },
  { field: "temperature", title: "Temperature", unit: "°C", range: "temperature" },
  { field: "humidity", title: "Humidity", unit: "%", range: "humidity" },
];

// Species range fields, in display order.
const RANGES = [
  { key: "temperature", label: "Temperature (°C)", step: "0.5" },
  { key: "humidity", label: "Humidity (%)", step: "1" },
  { key: "light_lux", label: "Light (lx)", step: "100" },
  { key: "soil_moisture", label: "Soil moisture (%)", step: "1" },
];

const VIEWS = ["dashboard", "plants", "species", "settings"];

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const state = {
  user: null,
  plants: [],
  detail: null, // { plant, hours, readings }
  editingPlant: null,
  editingSpecies: null,
  plantSpecies: [], // species offered in the plant dialog
};
let refreshTimer = null;

// ------------------------------------------------------------------ helpers

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

function fmt(v, unit) {
  if (v === null || v === undefined) return "—";
  const digits = Math.abs(v) >= 100 ? 0 : 1;
  return `${v.toFixed(digits)} ${unit}`.trim();
}

function ago(ts) {
  if (!ts) return "never";
  const s = Math.max(0, Date.now() / 1000 - ts);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}

function date(ts) {
  return ts ? new Date(ts * 1000).toLocaleString() : "—";
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

let toastTimer = null;
function toast(msg, isError = false) {
  const t = $("#toast");
  t.textContent = msg;
  t.classList.toggle("error", isError);
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), isError ? 6000 : 3000);
}

function showError(e) {
  if (e.status === 401) return; // handled by showing the login screen
  $("#error").textContent = e.message;
  $("#error").hidden = false;
}

// ------------------------------------------------------------------ API

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(method, url, body) {
  const opts = {
    method,
    cache: "no-store",
    credentials: "same-origin",
    // Required by the server on state-changing requests (CSRF protection).
    headers: { "X-Requested-With": "plantmonitor" },
  };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(url, opts);
  } catch {
    throw new HttpError(0, "Could not reach the server");
  }
  const text = await res.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!res.ok) {
    if (res.status === 401 && url !== "/api/login") showLogin();
    const msg = (data && data.error) || (typeof data === "string" && data) || `HTTP ${res.status}`;
    throw new HttpError(res.status, msg);
  }
  return data;
}

// ------------------------------------------------------------------ login / navigation

function showLogin() {
  state.user = null;
  clearInterval(refreshTimer);
  refreshTimer = null;
  $$("dialog[open]").forEach((d) => d.close());
  $("#app-view").hidden = true;
  $("#login-view").hidden = false;
  $("#login-form [name=username]").focus();
}

function showApp(user, weakPassword) {
  state.user = user;
  $("#user").textContent = user;
  $("#weak-password").hidden = !weakPassword;
  $("#login-view").hidden = true;
  $("#app-view").hidden = false;
  route();
  if (!refreshTimer) {
    refreshTimer = setInterval(() => {
      if (currentView() === "dashboard") loadDashboard().catch(showError);
    }, REFRESH_MS);
  }
}

$("#login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  const btn = $("button[type=submit]", e.target);
  btn.disabled = true;
  try {
    const r = await api("POST", "/api/login", { username: f.get("username"), password: f.get("password") });
    e.target.reset();
    $("#login-error").hidden = true;
    showApp(r.user, r.weak_password);
  } catch (err) {
    $("#login-error").textContent = err.message;
    $("#login-error").hidden = false;
  } finally {
    btn.disabled = false;
  }
});

$("#logout").addEventListener("click", async () => {
  try {
    await api("POST", "/api/logout");
  } finally {
    showLogin();
  }
});

function currentView() {
  const v = location.hash.slice(1);
  return VIEWS.includes(v) ? v : "dashboard";
}

const LOADERS = {
  dashboard: () => loadDashboard(),
  plants: () => loadPlants(),
  species: () => loadSpecies(),
  settings: () => loadSettings(),
};

async function route() {
  const v = currentView();
  for (const name of VIEWS) $(`#view-${name}`).hidden = name !== v;
  $$("nav a").forEach((a) => a.classList.toggle("active", a.dataset.view === v));
  $("#error").hidden = true;
  try {
    await LOADERS[v]();
  } catch (e) {
    showError(e);
  }
}

window.addEventListener("hashchange", () => {
  if (state.user) route();
});

// Generic dialog closing: ✕ / Cancel buttons and backdrop clicks.
for (const dialog of $$("dialog")) {
  dialog.addEventListener("click", (e) => {
    if (e.target.closest(".close, .cancel")) return dialog.close();
    if (e.target !== dialog) return;
    const r = dialog.getBoundingClientRect();
    const outside = e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom;
    if (outside) dialog.close();
  });
}

// ------------------------------------------------------------------ dashboard

/** Position elements carrying data-left / data-width (CSP forbids inline styles). */
function applyBars(root) {
  for (const el of $$("[data-left]", root)) {
    el.style.left = `${el.dataset.left}%`;
    if (el.dataset.width) el.style.width = `${el.dataset.width}%`;
  }
}

function rangeBar(check) {
  // Scale shows the optimal band with 50 % padding either side, widened to
  // include the current value.
  const pad = (check.max - check.min) * 0.5 || 1;
  let lo = check.min - pad;
  let hi = check.max + pad;
  if (check.value !== null) {
    lo = Math.min(lo, check.value);
    hi = Math.max(hi, check.value);
  }
  lo = Math.max(0, lo);
  const pct = (v) => (((v - lo) / (hi - lo)) * 100).toFixed(2);
  const band = `<div class="band" data-left="${pct(check.min)}" data-width="${pct(check.max) - pct(check.min)}"></div>`;
  const marker = check.value === null ? "" : `<div class="marker ${check.status}" data-left="${pct(check.value)}"></div>`;
  return `<div class="bar">${band}${marker}</div>`;
}

function plantCard(p) {
  const metrics = p.checks.map((c) => `
    <div class="metric" title="Optimal: ${c.min}–${c.max} ${esc(c.unit)}">
      <div class="metric-row">
        <span>${esc(c.label)}</span>
        <span class="metric-value ${c.status}">${fmt(c.value, c.unit)}</span>
      </div>
      ${rangeBar(c)}
    </div>`).join("");
  const advice = p.checks.filter((c) => c.advice).map((c) => `<li>${esc(c.advice)}</li>`).join("");
  const sub = [p.species_info.name, p.location, p.dormancy_active && "Dormant"].filter(Boolean).map(esc).join(" · ");

  return `
    <article class="card ${p.status}" tabindex="0" role="button" data-id="${p.id}">
      <div class="card-head">
        <h3>${esc(p.name)}</h3>
        <span class="badge ${p.status}">${STATUS_LABEL[p.status] ?? esc(p.status)}</span>
      </div>
      <p class="sub">${sub}</p>
      ${metrics}
      ${advice ? `<ul class="advice">${advice}</ul>` : ""}
      <div class="card-foot">Last reading ${ago(p.last_seen)} · ${esc(p.device_id)}${p.alerts ? "" : " · alerts off"}</div>
    </article>`;
}

function renderDevices(devices) {
  const unassigned = devices.filter((d) => d.plant_id === null);
  $("#devices-section").hidden = unassigned.length === 0;
  $("#devices").innerHTML = unassigned.map((d) => `
    <tr>
      <td><code>${esc(d.device_id)}</code></td>
      <td>${ago(d.last_seen)}</td>
      <td>${fmt(d.latest.temperature, "°C")}</td>
      <td>${fmt(d.latest.humidity, "%")}</td>
      <td>${fmt(d.latest.lux, "lx")}</td>
      <td>${d.latest.soil_raw ?? "—"}</td>
      <td>${d.latest.rssi != null ? esc(d.latest.rssi) + " dBm" : "—"}</td>
      <td class="actions"><button type="button" class="small" data-add-device="${esc(d.device_id)}">Add as plant</button></td>
    </tr>`).join("");
}

async function loadDashboard() {
  const [plants, devices] = await Promise.all([api("GET", "/api/plants"), api("GET", "/api/devices")]);
  state.plants = plants;
  $("#cards").innerHTML = plants.map(plantCard).join("");
  applyBars($("#cards"));
  $("#empty").hidden = plants.length > 0;
  renderDevices(devices);
  const attention = plants.filter((p) => p.status === "attention").length;
  const offline = plants.filter((p) => p.status === "offline").length;
  $("#summary").textContent =
    `${plants.length} plants · ${attention} need attention · ${offline} offline · updated ${new Date().toLocaleTimeString()}`;
}

$("#refresh").addEventListener("click", () => loadDashboard().catch(showError));
$("#cards").addEventListener("click", (e) => {
  const card = e.target.closest(".card");
  if (card) openDetail(Number(card.dataset.id));
});
$("#cards").addEventListener("keydown", (e) => {
  const card = e.target.closest(".card");
  if (card && (e.key === "Enter" || e.key === " ")) {
    e.preventDefault();
    openDetail(Number(card.dataset.id));
  }
});
$("#devices").addEventListener("click", (e) => {
  const btn = e.target.closest("[data-add-device]");
  if (btn) openPlantDialog(null, btn.dataset.addDevice);
});

// ------------------------------------------------------------------ detail / charts

function drawChart(canvas, readings, field, range, hours) {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  const ctx = canvas.getContext("2d");
  ctx.scale(dpr, dpr);

  const pts = readings.filter((r) => r[field] !== null && r[field] !== undefined);
  const now = Date.now() / 1000;
  const t0 = now - hours * 3600;

  let lo = range[0];
  let hi = range[1];
  for (const r of pts) {
    lo = Math.min(lo, r[field]);
    hi = Math.max(hi, r[field]);
  }
  const pad = (hi - lo) * 0.08 || 1;
  lo = Math.max(0, lo - pad);
  hi += pad;

  const L = 44, R = 8, T = 8, B = 22;
  const x = (t) => L + ((t - t0) / (now - t0)) * (w - L - R);
  const y = (v) => T + (1 - (v - lo) / (hi - lo)) * (h - T - B);

  ctx.font = "11px system-ui, sans-serif";
  ctx.lineWidth = 1;

  // Optimal band
  ctx.fillStyle = cssVar("--band");
  ctx.fillRect(L, y(range[1]), w - L - R, y(range[0]) - y(range[1]));

  // Y axis labels at band edges and extremes
  ctx.fillStyle = cssVar("--muted");
  ctx.strokeStyle = cssVar("--border");
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  const yTicks = [...new Set([lo, range[0], range[1], hi].map((v) => Math.round(v)))];
  for (const v of yTicks) {
    const yy = y(v);
    if (yy < T - 1 || yy > h - B + 1) continue;
    ctx.fillText(String(v), L - 6, yy);
    ctx.beginPath();
    ctx.moveTo(L, yy + 0.5);
    ctx.lineTo(w - R, yy + 0.5);
    ctx.stroke();
  }

  // X axis labels
  ctx.textBaseline = "top";
  const ticks = 4;
  for (let i = 0; i <= ticks; i++) {
    const t = t0 + ((now - t0) * i) / ticks;
    const d = new Date(t * 1000);
    const label = hours <= 24
      ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
      : d.toLocaleDateString([], { month: "short", day: "numeric" });
    ctx.textAlign = i === 0 ? "left" : i === ticks ? "right" : "center";
    ctx.fillText(label, x(t), h - B + 6);
  }

  if (pts.length === 0) {
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText("no data", (L + w - R) / 2, (T + h - B) / 2);
    return;
  }

  // Data line
  ctx.strokeStyle = cssVar("--line");
  ctx.lineWidth = 1.8;
  ctx.lineJoin = "round";
  ctx.beginPath();
  let prev = null;
  for (const r of pts) {
    const px = x(r.timestamp);
    const py = y(r[field]);
    if (prev === null || r.timestamp - prev > GAP_SECS) ctx.moveTo(px, py);
    else ctx.lineTo(px, py);
    prev = r.timestamp;
  }
  ctx.stroke();

  const last = pts[pts.length - 1];
  ctx.fillStyle = cssVar("--line");
  ctx.beginPath();
  ctx.arc(x(last.timestamp), y(last[field]), 3, 0, Math.PI * 2);
  ctx.fill();
}

function drawCharts() {
  const { plant, readings, hours } = state.detail;
  for (const c of CHARTS) {
    const canvas = $(`#charts canvas[data-field="${c.field}"]`);
    // The checks carry the ranges in effect (normal or dormancy).
    const check = plant.checks.find((k) => k.metric === c.range);
    drawChart(canvas, readings, c.field, [check.min, check.max], hours);
  }
}

async function loadHistory() {
  const { plant, hours } = state.detail;
  const base = `/api/plants/${plant.id}/readings?hours=${hours}`;
  $("#csv-link").href = `${base}&format=csv`;
  $("#json-link").href = base;
  $$(".range-picker button").forEach((b) => b.classList.toggle("active", Number(b.dataset.hours) === hours));

  const charts = $("#charts");
  charts.innerHTML = CHARTS.map((c) => `
    <div class="chart">
      <h4>${c.title} <span class="muted">(${c.unit})</span></h4>
      <canvas data-field="${c.field}"></canvas>
    </div>`).join("");

  state.detail.readings = [];
  try {
    state.detail.readings = await api("GET", base);
  } catch (e) {
    charts.insertAdjacentHTML("afterbegin", `<p class="error">${esc(e.message)}</p>`);
  }
  drawCharts();
}

function openDetail(id) {
  const plant = state.plants.find((p) => p.id === id);
  if (!plant) return;
  state.detail = { plant, hours: state.detail?.hours ?? 24, readings: [] };
  $("#detail-title").textContent = plant.name;
  $("#detail-sub").textContent = [plant.species_info.name, plant.location, plant.species_info.notes]
    .filter(Boolean).join(" · ");
  $("#detail").showModal();
  loadHistory();
}

$$(".range-picker button").forEach((b) => {
  b.addEventListener("click", () => {
    state.detail.hours = Number(b.dataset.hours);
    loadHistory();
  });
});
window.addEventListener("resize", () => {
  if ($("#detail").open && state.detail?.readings) drawCharts();
});

// ------------------------------------------------------------------ plants

async function loadPlants() {
  const plants = await api("GET", "/api/plants");
  state.plants = plants;
  $("#plants-table").innerHTML = plants.length === 0
    ? `<tr><td colspan="7" class="empty">No plants yet. Flash a sensor, then add it here.</td></tr>`
    : plants.map((p) => `
      <tr>
        <td>${esc(p.name)}</td>
        <td>${esc(p.location) || "—"}</td>
        <td>${esc(p.species_info.name)}${p.dormancy_active ? " · dormant" : ""}</td>
        <td><code>${esc(p.device_id)}</code></td>
        <td>${p.soil_dry_raw} / ${p.soil_wet_raw}</td>
        <td>${p.alerts ? "On" : "Off"}</td>
        <td class="actions">
          <button type="button" class="small" data-edit="${p.id}">Edit</button>
          <button type="button" class="small danger" data-delete="${p.id}">Delete</button>
        </td>
      </tr>`).join("");
}

$("#add-plant").addEventListener("click", () => openPlantDialog(null));
$("#plants-table").addEventListener("click", async (e) => {
  const edit = e.target.closest("[data-edit]");
  const del = e.target.closest("[data-delete]");
  if (edit) {
    openPlantDialog(state.plants.find((p) => p.id === Number(edit.dataset.edit)));
  } else if (del) {
    const p = state.plants.find((p) => p.id === Number(del.dataset.delete));
    if (!p || !confirm(`Delete plant “${p.name}”? Its sensor readings are kept.`)) return;
    try {
      await api("DELETE", `/api/plants/${p.id}`);
      toast("Plant deleted");
      loadPlants();
    } catch (err) {
      toast(err.message, true);
    }
  }
});

async function openPlantDialog(plant, presetDevice = "") {
  const form = $("#plant-form");
  form.reset();
  $(".form-error", form).hidden = true;
  $("#calib-hint").textContent = "";
  state.editingPlant = plant;
  $("#plant-dialog-title").textContent = plant ? `Edit ${plant.name}` : "Add plant";

  let species, devices;
  try {
    [species, devices] = await Promise.all([api("GET", "/api/species"), api("GET", "/api/devices")]);
  } catch (e) {
    return toast(e.message, true);
  }
  form.species_id.innerHTML = species.map((s) => `<option value="${s.id}">${esc(s.name)}</option>`).join("");
  state.plantSpecies = species;
  $("#device-list").innerHTML = devices
    .filter((d) => d.plant_id === null || d.plant_id === plant?.id)
    .map((d) => `<option value="${esc(d.device_id)}">`).join("");

  form.name.value = plant?.name ?? "";
  form.location.value = plant?.location ?? "";
  form.device_id.value = plant?.device_id ?? presetDevice;
  if (plant) form.species_id.value = String(plant.species_id);
  form.soil_dry_raw.value = plant?.soil_dry_raw ?? 2800;
  form.soil_wet_raw.value = plant?.soil_wet_raw ?? 1200;
  form.alerts.checked = plant?.alerts ?? true;
  form.dormant.checked = plant?.dormant ?? false;
  updateDormantOption();
  $("#plant-dialog").showModal();
}

// The dormant switch only makes sense for species with dormancy ranges.
function updateDormantOption() {
  const form = $("#plant-form");
  const s = state.plantSpecies.find((s) => s.id === Number(form.species_id.value));
  $("#dormant-option").hidden = !s?.dormancy;
}

$("#plant-form").species_id.addEventListener("change", updateDormantOption);

$("#plant-form").addEventListener("click", async (e) => {
  const btn = e.target.closest("[data-calibrate]");
  if (!btn) return;
  const form = $("#plant-form");
  const id = form.device_id.value.trim();
  if (!id) return toast("Enter the device ID first", true);
  try {
    const devices = await api("GET", "/api/devices");
    const d = devices.find((d) => d.device_id === id);
    if (!d || d.latest.soil_raw == null) return toast(`No soil reading from ${id} yet`, true);
    form[btn.dataset.calibrate].value = d.latest.soil_raw;
    $("#calib-hint").textContent = `Used raw value ${d.latest.soil_raw} from ${ago(d.last_seen)}.`;
  } catch (err) {
    toast(err.message, true);
  }
});

$("#plant-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target;
  const body = {
    name: form.name.value.trim(),
    location: form.location.value.trim(),
    device_id: form.device_id.value.trim(),
    species_id: Number(form.species_id.value),
    soil_dry_raw: Number(form.soil_dry_raw.value),
    soil_wet_raw: Number(form.soil_wet_raw.value),
    alerts: form.alerts.checked,
    dormant: form.dormant.checked,
  };
  const editing = state.editingPlant;
  try {
    if (editing) await api("PUT", `/api/plants/${editing.id}`, body);
    else await api("POST", "/api/plants", body);
    $("#plant-dialog").close();
    toast(editing ? "Plant saved" : "Plant added");
    route();
  } catch (err) {
    $(".form-error", form).textContent = err.message;
    $(".form-error", form).hidden = false;
  }
});

// ------------------------------------------------------------------ species

let speciesList = [];

function rangeText([lo, hi], unit) {
  return `${lo}–${hi} ${unit}`;
}

async function loadSpecies() {
  speciesList = await api("GET", "/api/species");
  $("#species-table").innerHTML = speciesList.map((s) => `
    <tr>
      <td title="${esc(s.notes)}">${esc(s.name)}</td>
      <td>${rangeText(s.temperature, "°C")}</td>
      <td>${rangeText(s.humidity, "%")}</td>
      <td>${rangeText(s.light_lux, "lx")}</td>
      <td>${rangeText(s.soil_moisture, "%")}</td>
      <td>${s.dormancy ? `${rangeText(s.dormancy.temperature, "°C")}, soil ${rangeText(s.dormancy.soil_moisture, "%")}` : "—"}</td>
      <td class="actions">
        <button type="button" class="small" data-edit="${s.id}">Edit</button>
        <button type="button" class="small danger" data-delete="${s.id}">Delete</button>
      </td>
    </tr>`).join("");
}

// Min/max inputs for every range, named `${prefix}${key}_min` / `_max`.
function rangeInputs(prefix) {
  return `<span></span><span class="head">Min</span><span class="head">Max</span>` +
    RANGES.map((r) => `
      <span>${r.label}</span>
      <input type="number" name="${prefix}${r.key}_min" step="${r.step}" required aria-label="${r.label} min">
      <input type="number" name="${prefix}${r.key}_max" step="${r.step}" required aria-label="${r.label} max">`).join("");
}

$("#species-ranges").innerHTML = rangeInputs("");
$("#dormancy-ranges").innerHTML = rangeInputs("dorm_");

// Hidden dormancy inputs are disabled so they skip validation and submission.
function updateDormancyFields() {
  const fields = $("#dormancy-fields");
  fields.hidden = !$("#species-form").has_dormancy.checked;
  fields.disabled = fields.hidden;
}

$("#species-form").has_dormancy.addEventListener("change", updateDormancyFields);

$("#add-species").addEventListener("click", () => openSpeciesDialog(null));
$("#species-table").addEventListener("click", async (e) => {
  const edit = e.target.closest("[data-edit]");
  const del = e.target.closest("[data-delete]");
  if (edit) {
    openSpeciesDialog(speciesList.find((s) => s.id === Number(edit.dataset.edit)));
  } else if (del) {
    const s = speciesList.find((s) => s.id === Number(del.dataset.delete));
    if (!s || !confirm(`Delete species “${s.name}”?`)) return;
    try {
      await api("DELETE", `/api/species/${s.id}`);
      toast("Species deleted");
      loadSpecies();
    } catch (err) {
      toast(err.status === 409 ? "This species is still used by a plant." : err.message, true);
    }
  }
});

function openSpeciesDialog(species) {
  const form = $("#species-form");
  form.reset();
  $(".form-error", form).hidden = true;
  state.editingSpecies = species;
  $("#species-dialog-title").textContent = species ? `Edit ${species.name}` : "Add species";
  form.name.value = species?.name ?? "";
  form.notes.value = species?.notes ?? "";
  for (const r of RANGES) {
    form[`${r.key}_min`].value = species?.[r.key][0] ?? "";
    form[`${r.key}_max`].value = species?.[r.key][1] ?? "";
    form[`dorm_${r.key}_min`].value = species?.dormancy?.[r.key][0] ?? "";
    form[`dorm_${r.key}_max`].value = species?.dormancy?.[r.key][1] ?? "";
  }
  form.has_dormancy.checked = Boolean(species?.dormancy);
  updateDormancyFields();
  $("#species-dialog").showModal();
}

$("#species-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const form = e.target;
  const body = { name: form.name.value.trim(), notes: form.notes.value.trim(), dormancy: null };
  const range = (name) => [Number(form[`${name}_min`].value), Number(form[`${name}_max`].value)];
  if (form.has_dormancy.checked) body.dormancy = {};
  for (const r of RANGES) {
    body[r.key] = range(r.key);
    if (body.dormancy) body.dormancy[r.key] = range(`dorm_${r.key}`);
  }
  const editing = state.editingSpecies;
  try {
    if (editing) await api("PUT", `/api/species/${editing.id}`, body);
    else await api("POST", "/api/species", body);
    $("#species-dialog").close();
    toast(editing ? "Species saved" : "Species added");
    loadSpecies();
  } catch (err) {
    $(".form-error", form).textContent = err.status === 409 ? "A species with this name already exists." : err.message;
    $(".form-error", form).hidden = false;
  }
});

// ------------------------------------------------------------------ settings

async function loadSettings() {
  $$(".origin").forEach((el) => (el.textContent = location.origin));
  const [ntfy, tokens] = await Promise.all([api("GET", "/api/notifications"), api("GET", "/api/tokens")]);

  $("#ntfy-status").innerHTML = ntfy.enabled
    ? `Sending to <code>${esc(ntfy.server)}/${esc(ntfy.topic)}</code>. Subscribe to this topic in the ntfy app.
       Alerts after ${esc(ntfy.alert_after_minutes)} min,
       ${ntfy.remind_after_hours > 0 ? `reminders every ${esc(ntfy.remind_after_hours)} h` : "no reminders"}.`
    : `Notifications are off. Add an <code>[ntfy]</code> section to the server's config.toml to enable them.`;
  $("#ntfy-test").hidden = !ntfy.enabled;

  $("#tokens-table").innerHTML = tokens.length === 0
    ? `<tr><td colspan="4" class="empty">No API tokens.</td></tr>`
    : tokens.map((t) => `
      <tr>
        <td>${esc(t.name)}</td>
        <td>${date(t.created)}</td>
        <td>${t.last_used ? ago(t.last_used) : "never"}</td>
        <td class="actions"><button type="button" class="small danger" data-revoke="${t.id}">Revoke</button></td>
      </tr>`).join("");
}

$("#ntfy-test").addEventListener("click", async () => {
  try {
    await api("POST", "/api/notifications/test");
    toast("Test notification sent");
  } catch (e) {
    toast(e.message, true);
  }
});

$("#token-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  try {
    const r = await api("POST", "/api/tokens", { name: e.target.name.value.trim() });
    e.target.reset();
    $("#new-token-value").textContent = r.token;
    $("#new-token").hidden = false;
    loadSettings();
  } catch (err) {
    toast(err.message, true);
  }
});

$("#tokens-table").addEventListener("click", async (e) => {
  const btn = e.target.closest("[data-revoke]");
  if (!btn || !confirm("Revoke this token? Scripts using it will stop working.")) return;
  try {
    await api("DELETE", `/api/tokens/${btn.dataset.revoke}`);
    toast("Token revoked");
    loadSettings();
  } catch (err) {
    toast(err.message, true);
  }
});

$("#password-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = e.target;
  if (f.new.value !== f.repeat.value) return toast("New passwords don't match", true);
  try {
    await api("POST", "/api/password", { current: f.current.value, new: f.new.value });
    f.reset();
    $("#weak-password").hidden = true;
    toast("Password changed; other sessions logged out");
  } catch (err) {
    toast(err.message, true);
  }
});

// ------------------------------------------------------------------ start

(async () => {
  try {
    const me = await api("GET", "/api/me");
    showApp(me.user, me.weak_password);
  } catch {
    showLogin();
  }
})();
