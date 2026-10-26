// EmpireBT web UI. Talks to the JSON API under /api and follows live
// updates from /api/events. Everything shown is built with DOM methods and
// textContent: nothing from the server is ever parsed as HTML.
//
// Updates carry only the torrents that changed, so the page keeps one row
// element per torrent and touches just the cells whose text changed. That
// keeps thousands of torrents cheap to follow.

const $ = (selector) => document.querySelector(selector);

const ui = {
  torrents: new Map(), // hash → torrent
  order: [], // hashes, in the order they were added
  header: null,
  lists: { categories: [], tags: [], settings: [], saveDirs: [], categoryFolders: {} },
  filter: { kind: "all" },
  search: "",
  sort: { key: "", desc: false },
  selected: null,
  tab: "overview",
  details: null,
  peers: null, // { hash, list, attribution }
  peerSort: { key: "up", desc: true },
  seenNotice: 0,
  events: null,
  rows: new Map(), // hash → row (see makeRow)
  shown: [], // hashes of the rows on screen, in order
  sideShape: "",
  sideCounts: new Map(), // filter key → count element
  networkShape: "",
  detailShape: "", // what the detail panel was last built for
  pending: false,
};

const narrow = window.matchMedia("(max-width: 760px)");

// ----- API --------------------------------------------------------------------

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(path, { method = "GET", body, raw } = {}) {
  const headers = { "X-EmpireBT": "1" };
  let payload;
  if (raw !== undefined) {
    headers["Content-Type"] = "application/x-bittorrent";
    payload = raw;
  } else if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    payload = JSON.stringify(body);
  }
  const response = await fetch(path, {
    method,
    headers,
    body: payload,
    credentials: "same-origin",
    cache: "no-store",
  });
  if (response.status === 401 && path !== "/api/login") {
    showLogin();
    throw new ApiError(401, "Please sign in again.");
  }
  const text = await response.text();
  const data = text ? JSON.parse(text) : null;
  if (!response.ok) {
    throw new ApiError(response.status, (data && data.error) || response.statusText);
  }
  return data;
}

async function act(promise, success) {
  try {
    const result = await promise;
    if (success) toast("success", success);
    return result;
  } catch (error) {
    if (error.status !== 401) toast("error", error.message);
    return undefined;
  }
}

// ----- formatting ----------------------------------------------------------------

function bytes(value) {
  if (value === null || value === undefined) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return index === 0 ? `${value} B` : `${value.toFixed(value < 10 ? 2 : 1)} ${units[index]}`;
}

function rate(value) {
  return value >= 1 ? `${bytes(Math.round(value))}/s` : "";
}

function etaSeconds(torrent) {
  if (!torrent.remaining || torrent.down < 1) return Infinity;
  return torrent.remaining / torrent.down;
}

function eta(remaining, speed) {
  if (!remaining || speed < 1) return "";
  let seconds = Math.round(remaining / speed);
  if (seconds > 99 * 86400) return "∞";
  const days = Math.floor(seconds / 86400);
  seconds %= 86400;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days) return `${days}d ${hours}h`;
  if (hours) return `${hours}h ${minutes}m`;
  if (minutes) return `${minutes}m ${seconds % 60}s`;
  return `${seconds}s`;
}

function percent(progress) {
  return progress === null ? "—" : `${(progress * 100).toFixed(1)}%`;
}

// A country code as a flag: two regional indicator symbols.
function flag(code) {
  if (!code || !/^[A-Z]{2}$/.test(code) || code === "ZZ") return "";
  return String.fromCodePoint(...[...code].map((letter) => 0x1f1e6 + letter.charCodeAt(0) - 65));
}

function element(tag, options = {}, children = []) {
  const node = document.createElement(tag);
  if (options.class) node.className = options.class;
  if (options.text !== undefined) node.textContent = options.text;
  if (options.title) node.title = options.title;
  for (const [name, value] of Object.entries(options.attrs || {})) node.setAttribute(name, value);
  for (const [name, handler] of Object.entries(options.on || {})) node.addEventListener(name, handler);
  for (const child of children) if (child) node.append(child);
  return node;
}

const SVG = "http://www.w3.org/2000/svg";

// One of the icons defined at the top of index.html.
function icon(name) {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(SVG, "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

const calm = window.matchMedia("(prefers-reduced-motion: reduce)");

// Runs `then` once `node` finishes animating out, or straight away when
// there is no animation to wait for.
function afterAnimation(node, then) {
  let done = false;
  const finish = () => {
    if (done) return;
    done = true;
    then();
  };
  if (calm.matches) return finish();
  node.addEventListener("animationend", finish, { once: true });
  node.addEventListener("transitionend", finish, { once: true });
  setTimeout(finish, 400);
}

// Assigning the same text still costs layout work; skip it.
function setText(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

function setClass(node, name) {
  if (node.className !== name) node.className = name;
}

// ----- signing in ----------------------------------------------------------------

function showLogin() {
  if (ui.events) {
    ui.events.close();
    ui.events = null;
  }
  $("#app").hidden = true;
  $("#login").hidden = false;
  $("#password").focus();
}

async function showApp() {
  $("#login").hidden = true;
  $("#app").hidden = false;
  apply(await api("/api/state"));
  follow();
}

async function login(body) {
  const message = $("#login-error");
  message.hidden = true;
  try {
    await api("/api/login", { method: "POST", body });
    $("#password").value = "";
    await showApp();
  } catch (error) {
    message.textContent = error.message;
    message.hidden = false;
  }
}

$("#login-form").addEventListener("submit", (event) => {
  event.preventDefault();
  login({ password: $("#password").value });
});

$("#logout-button").addEventListener("click", async () => {
  await act(api("/api/logout", { method: "POST" }));
  showLogin();
});

// Live updates; the browser reconnects by itself after errors, and the
// first update on every connection is a full one.
function follow() {
  if (ui.events) ui.events.close();
  ui.events = new EventSource("/api/events");
  ui.events.addEventListener("update", (event) => apply(JSON.parse(event.data)));
  ui.events.addEventListener("error", async () => {
    try {
      await api("/api/session");
    } catch {
      // api() shows the login screen on 401.
    }
  });
}

// Folds an update into what we know, then redraws on the next frame.
function apply(update) {
  if (update.full) {
    ui.torrents.clear();
    ui.order = [];
  }
  for (const hash of update.removed) ui.torrents.delete(hash);
  const changed = new Set();
  for (const torrent of update.torrents) {
    ui.torrents.set(torrent.hash, torrent);
    changed.add(torrent.hash);
  }
  if (update.order) ui.order = update.order;
  ui.header = update.header;
  if (update.lists) ui.lists = update.lists;
  for (const hash of changed) {
    const row = ui.rows.get(hash);
    if (row) fillRow(row, ui.torrents.get(hash));
  }
  if (update.full) {
    for (const [hash, row] of ui.rows) {
      if (!ui.torrents.has(hash)) ui.rows.delete(hash);
      else if (!changed.has(hash)) fillRow(row, ui.torrents.get(hash));
    }
  } else {
    for (const hash of update.removed) ui.rows.delete(hash);
  }
  renderNotices(update.notices);
  schedule();
}

function schedule() {
  if (ui.pending) return;
  ui.pending = true;
  requestAnimationFrame(() => {
    ui.pending = false;
    render();
  });
}

// ----- rendering -------------------------------------------------------------------

const ACTIVE = ["downloading", "metadata", "announcing", "checking", "waiting", "retrying"];

function matches(torrent, filter) {
  switch (filter.kind) {
    case "status":
      return filter.value === "active" ? ACTIVE.includes(torrent.status) : torrent.status === filter.value;
    case "category":
      return torrent.category === filter.value;
    case "uncategorized":
      return !torrent.category;
    case "tag":
      return torrent.tags.includes(filter.value);
    case "untagged":
      return torrent.tags.length === 0;
    default:
      return true;
  }
}

function render() {
  if (!ui.header) return;
  if (ui.selected && !ui.torrents.has(ui.selected)) {
    ui.selected = null;
    ui.details = null;
    ui.peers = null;
    closeDetail();
  }
  renderHeader(ui.header);
  renderSide();
  renderRows();
  renderActions();
  renderDetail(false);
}

function renderHeader(header) {
  const limit = (value) => (value ? ` / ${rate(value)}` : "");
  setText($("#rate-down"), `${rate(header.totals.down) || "0 B/s"}${limit(header.limits.down)}`);
  setText($("#rate-up"), `${rate(header.totals.up) || "0 B/s"}${limit(header.limits.up)}`);
  const network = header.network;
  const parts = [];
  if (network.interface && !network.interfaceUp) {
    parts.push(["bad", `${network.interface} is down · not connecting`]);
  } else {
    const seeding = network.seeding;
    const where = `${network.interface ? network.interface : ""}:${network.port}`;
    if (seeding.state === "listening") {
      parts.push(["good", `● ${seeding.torrents ? `seeding ${seeding.torrents}` : "listening"} on ${where}`]);
    } else if (seeding.state === "failed") {
      parts.push(["bad", `✕ ${seeding.error}`]);
    } else {
      parts.push(["", `◌ ${seeding.state}`]);
    }
  }
  if (network.peers) parts.push(["", `${network.peers} peers`]);
  if (network.dht.state === "running") {
    const ipv6 = network.dht.ipv6Nodes ? ` (${network.dht.ipv6Nodes} IPv6)` : "";
    parts.push(["", `DHT ${network.dht.nodes} nodes${ipv6}`]);
  } else if (network.dht.state === "failed") {
    parts.push(["bad", "DHT off"]);
  }
  if (network.utp) parts.push(["", "uTP"]);
  if (network.lsd) parts.push(["", "LSD"]);
  const shape = JSON.stringify(parts);
  if (shape !== ui.networkShape) {
    ui.networkShape = shape;
    $("#network").replaceChildren(...parts.map(([kind, text]) => element("span", { class: kind, text })));
  }
}

function filterKey(filter) {
  return filter.value === undefined ? filter.kind : `${filter.kind}:${filter.value}`;
}

const STATUS_FILTERS = [
  ["All", { kind: "all" }],
  ["Active", { kind: "status", value: "active" }],
  ["Seeding", { kind: "status", value: "seeding" }],
  ["Paused", { kind: "status", value: "paused" }],
  ["Errors", { kind: "status", value: "error" }],
];

// Every filter's count in one pass over the torrents.
function countFilters() {
  const counts = new Map();
  const bump = (key) => counts.set(key, (counts.get(key) || 0) + 1);
  for (const torrent of ui.torrents.values()) {
    bump("all");
    if (ACTIVE.includes(torrent.status)) bump("status:active");
    bump(`status:${torrent.status}`);
    bump(torrent.category ? `category:${torrent.category}` : "uncategorized");
    if (torrent.tags.length === 0) bump("untagged");
    for (const tag of torrent.tags) bump(`tag:${tag}`);
  }
  return counts;
}

function renderSide() {
  const { categories, tags } = ui.lists;
  // A filter on something removed falls back to everything.
  if ((ui.filter.kind === "category" && !categories.includes(ui.filter.value))
      || (ui.filter.kind === "tag" && !tags.includes(ui.filter.value))) {
    ui.filter = { kind: "all" };
  }
  const shape = JSON.stringify([categories, tags, ui.lists.categoryFolders, filterKey(ui.filter)]);
  if (shape !== ui.sideShape) {
    ui.sideShape = shape;
    buildSide(categories, tags);
  }
  const counts = countFilters();
  for (const [key, node] of ui.sideCounts) setText(node, String(counts.get(key) || 0));
}

function buildSide(categories, tags) {
  ui.sideCounts.clear();
  const item = (label, filter, extras = [], title = undefined) => {
    const count = element("span", { class: "count" });
    ui.sideCounts.set(filterKey(filter), count);
    const active = filterKey(filter) === filterKey(ui.filter);
    return element("li", { class: active ? "active" : "" }, [
      element("button", {
        class: active ? "filter active" : "filter",
        attrs: { "aria-pressed": String(active) },
        on: { click: () => setFilter(filter) },
        title,
      }, [element("span", { class: "label", text: label }), count]),
      ...extras,
    ]);
  };
  const folders = ui.lists.categoryFolders || {};
  const editor = (name) => element("button", {
    class: "edit",
    text: "✎",
    title: `Folder of ${name}`,
    attrs: { "aria-label": `Choose the folder of ${name}` },
    on: { click: () => categoryFolderDialog(name) },
  });
  const remover = (kind, name) => element("button", {
    class: "remove",
    text: "×",
    title: `Remove ${kind} ${name}`,
    attrs: { "aria-label": `Remove ${kind} ${name}` },
    on: {
      click: () => confirmDialog(`Remove the ${kind} “${name}”?`, "Torrents keep everything else.", "Remove", () =>
        act(api(`/api/${kind === "category" ? "categories" : "tags"}/remove`, { method: "POST", body: { name } }))),
    },
  });
  const adder = (kind) => element("button", {
    class: "add",
    text: "+",
    title: `New ${kind}`,
    attrs: { "aria-label": `New ${kind}` },
    on: {
      click: () => kind === "category"
        ? newCategoryDialog()
        : nameDialog("New tag", "", (name) => act(api("/api/tags", { method: "POST", body: { name } }))),
    },
  });
  const heading = (text, kind) => element("h2", {}, [element("span", { text }), adder(kind)]);
  $("#side").replaceChildren(
    element("ul", {}, STATUS_FILTERS.map(([label, filter]) => item(label, filter))),
    heading("Categories", "category"),
    element("ul", { class: "group" }, [
      ...categories.map((name) => item(name, { kind: "category", value: name },
        [editor(name), remover("category", name)],
        folders[name] ? `Saves in ${folders[name]}` : undefined)),
      item("No category", { kind: "uncategorized" }),
    ]),
    heading("Tags", "tag"),
    element("ul", { class: "group" }, [
      ...tags.map((name) => item(`#${name}`, { kind: "tag", value: name }, [remover("tag", name)])),
      item("No tags", { kind: "untagged" }),
    ]),
  );
}

function setFilter(filter) {
  ui.filter = filter;
  schedule();
}

// One row per torrent, made once and then updated in place.
function makeRow(hash) {
  const cell = (name) => element("div", { class: `cell ${name}`, attrs: { role: "cell" } });
  const title = element("span", { class: "title" });
  const labels = element("span", { class: "labels" });
  const name = cell("name");
  name.append(title, labels);
  const fill = element("div", { class: "fill" });
  const percentText = element("span", { class: "percent" });
  const progress = cell("progress");
  progress.append(element("div", { class: "bar" }, [fill]), percentText);
  const node = element("div", {
    class: "row",
    attrs: { role: "row", tabindex: "0", "data-hash": hash },
  }, [name, cell("num size"), progress, cell("status"), cell("num down"), cell("num up"), cell("num peers"), cell("num eta")]);
  const [, size, , statusCell, down, up, peers, etaCell] = node.children;
  const status = element("span", { class: "pill" });
  statusCell.append(status);
  return { node, title, labels, fill, percentText, size, status, down, up, peers, eta: etaCell, labelShape: "" };
}

function fillRow(row, torrent) {
  setText(row.title, torrent.name);
  row.title.title = torrent.error || torrent.name;
  const labelShape = JSON.stringify([torrent.category, torrent.tags]);
  if (labelShape !== row.labelShape) {
    row.labelShape = labelShape;
    const chips = [];
    if (torrent.category) chips.push(element("span", { class: "chip category", text: torrent.category }));
    for (const tag of torrent.tags) chips.push(element("span", { class: "chip tag", text: `#${tag}` }));
    row.labels.replaceChildren(...chips);
  }
  setText(row.size, bytes(torrent.size));
  const width = torrent.progress === null ? "0%" : `${(torrent.progress * 100).toFixed(1)}%`;
  if (row.fill.style.width !== width) row.fill.style.width = width;
  setClass(row.fill, `fill ${torrent.status}`);
  setText(row.percentText, percent(torrent.progress));
  setText(row.status, torrent.statusLabel);
  setClass(row.status, `pill ${torrent.status}`);
  row.status.title = torrent.error || "";
  setText(row.down, rate(torrent.down));
  setText(row.up, rate(torrent.up));
  setText(row.peers, torrent.peers === null ? "" : String(torrent.peers));
  setText(row.eta, eta(torrent.remaining, torrent.down));
}

function rowFor(hash) {
  let row = ui.rows.get(hash);
  if (!row) {
    row = makeRow(hash);
    fillRow(row, ui.torrents.get(hash));
    ui.rows.set(hash, row);
  }
  return row;
}

const SORT_KEYS = {
  name: (torrent) => torrent.name.toLowerCase(),
  size: (torrent) => torrent.size ?? -1,
  progress: (torrent) => torrent.progress ?? -1,
  status: (torrent) => torrent.statusLabel,
  down: (torrent) => torrent.down,
  up: (torrent) => torrent.up,
  peers: (torrent) => torrent.peers ?? -1,
  eta: etaSeconds,
};

function visibleHashes() {
  const search = ui.search;
  let hashes = ui.order.filter((hash) => {
    const torrent = ui.torrents.get(hash);
    return torrent
      && matches(torrent, ui.filter)
      && (!search || torrent.name.toLowerCase().includes(search));
  });
  const key = SORT_KEYS[ui.sort.key];
  if (key) {
    // Ties keep the order torrents were added in.
    const position = new Map(hashes.map((hash, index) => [hash, index]));
    const values = new Map(hashes.map((hash) => [hash, key(ui.torrents.get(hash))]));
    const direction = ui.sort.desc ? -1 : 1;
    hashes = hashes.sort((a, b) => {
      const x = values.get(a);
      const y = values.get(b);
      if (x < y) return -direction;
      if (x > y) return direction;
      return position.get(a) - position.get(b);
    });
  }
  return hashes;
}

function renderRows() {
  const hashes = visibleHashes();
  const same = hashes.length === ui.shown.length && hashes.every((hash, index) => hash === ui.shown[index]);
  if (!same) {
    ui.shown = hashes;
    $("#rows").replaceChildren(...hashes.map((hash) => rowFor(hash).node));
  }
  for (const [hash, row] of ui.rows) row.node.classList.toggle("selected", hash === ui.selected);
  const empty = $("#empty");
  if (ui.torrents.size === 0) {
    setText($("#empty-text"), "No torrents yet. Add one with the + button, or drop a .torrent file here.");
    empty.hidden = false;
  } else if (hashes.length === 0) {
    setText($("#empty-text"), "Nothing matches this filter.");
    empty.hidden = false;
  } else {
    empty.hidden = true;
  }
  const noun = ui.torrents.size === 1 ? "torrent" : "torrents";
  setText($("#shown"), hashes.length === ui.torrents.size ? `${hashes.length} ${noun}` : `${hashes.length} of ${ui.torrents.size}`);
  for (const button of document.querySelectorAll(".rows-head button")) {
    const active = button.dataset.sort === ui.sort.key;
    button.classList.toggle("sorted", active);
    button.classList.toggle("desc", active && ui.sort.desc);
  }
}

$("#rows").addEventListener("click", (event) => {
  const row = event.target.closest(".row");
  if (row) select(row.dataset.hash);
});

$("#rows").addEventListener("keydown", (event) => {
  const row = event.target.closest(".row");
  if (!row) return;
  if (event.key === "Enter") {
    select(row.dataset.hash);
  } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const next = event.key === "ArrowDown" ? row.nextElementSibling : row.previousElementSibling;
    if (next) {
      next.focus();
      select(next.dataset.hash, { open: false });
    }
  }
});

// Header clicks cycle through ascending, descending, and the added order.
for (const button of document.querySelectorAll(".rows-head button")) {
  button.addEventListener("click", () => {
    const key = button.dataset.sort;
    if (ui.sort.key !== key) ui.sort = { key, desc: ["size", "progress", "down", "up", "peers"].includes(key) };
    else if (ui.sort.desc === ["size", "progress", "down", "up", "peers"].includes(key)) ui.sort = { key, desc: !ui.sort.desc };
    else ui.sort = { key: "", desc: false };
    $("#sort").value = ui.sort.key;
    schedule();
  });
}

$("#sort").addEventListener("change", (event) => {
  const key = event.target.value;
  ui.sort = { key, desc: ["size", "progress", "down", "up", "peers"].includes(key) };
  schedule();
});

function selectedTorrent() {
  return ui.selected ? ui.torrents.get(ui.selected) : undefined;
}

function renderActions() {
  const torrent = selectedTorrent();
  for (const button of document.querySelectorAll(".selection-actions button")) {
    button.disabled = !torrent;
  }
  setText($("#detail-title"), torrent ? torrent.name : "");
}

async function select(hash, { open = true } = {}) {
  if (ui.selected !== hash) {
    ui.selected = hash;
    ui.details = null;
    ui.peers = null;
    ui.detailShape = "";
  }
  if (open) openDetail();
  render();
  await Promise.all([loadDetails(), ui.tab === "peers" ? loadPeers() : null]);
}

// On a phone the details slide in over the list; Back, the back gesture,
// or swiping the sheet to the right closes them.
function openDetail() {
  if (!narrow.matches || $("#app").classList.contains("detail-open")) return;
  $("#app").classList.add("detail-open");
  $("#detail-body").scrollTop = 0;
  history.pushState({ detail: true }, "");
}

function closeDetail() {
  if (!$("#app").classList.contains("detail-open")) return;
  $("#app").classList.remove("detail-open");
  if (history.state && history.state.detail) history.back();
}

window.addEventListener("popstate", () => $("#app").classList.remove("detail-open"));
$("#detail-back").addEventListener("click", closeDetail);

// Follows a finger dragging `node` along one axis; past `distance`, or
// flicked, `dismiss` runs, otherwise it springs back.
function draggable(node, { axis, start = () => true, dismiss }) {
  let origin = null;
  let moved = 0;
  let dragging = false;
  node.addEventListener("touchstart", (event) => {
    if (event.touches.length !== 1 || !start(event)) return;
    const touch = event.touches[0];
    origin = { x: touch.clientX, y: touch.clientY, at: performance.now() };
    moved = 0;
    dragging = false;
  }, { passive: true });
  node.addEventListener("touchmove", (event) => {
    if (!origin) return;
    const touch = event.touches[0];
    const dx = touch.clientX - origin.x;
    const dy = touch.clientY - origin.y;
    const along = axis === "x" ? dx : dy;
    const across = axis === "x" ? dy : dx;
    if (!dragging) {
      if (Math.abs(across) > 10 && Math.abs(across) > Math.abs(along)) {
        origin = null; // a scroll the other way
        return;
      }
      if (along < 10) return;
      dragging = true;
      node.classList.add("dragging");
    }
    moved = Math.max(0, along);
    node.style.transform = axis === "x" ? `translateX(${moved}px)` : `translateY(${moved}px)`;
  }, { passive: true });
  const end = () => {
    if (!origin) return;
    const speed = moved / Math.max(1, performance.now() - origin.at);
    origin = null;
    if (!dragging) return;
    node.classList.remove("dragging");
    node.style.transform = "";
    const size = axis === "x" ? node.offsetWidth : node.offsetHeight;
    if (moved > size * 0.3 || speed > 0.6) dismiss();
  };
  node.addEventListener("touchend", end);
  node.addEventListener("touchcancel", end);
}

draggable($("#detail"), {
  axis: "x",
  start: () => narrow.matches && $("#app").classList.contains("detail-open"),
  dismiss: closeDetail,
});
narrow.addEventListener("change", () => {
  if (!narrow.matches) closeDetail();
});

async function loadDetails() {
  if (!ui.selected) return;
  const hash = ui.selected;
  try {
    const details = await api(`/api/torrents/${hash}`);
    if (ui.selected === hash) {
      ui.details = details;
      renderDetail(true);
    }
  } catch {
    // Removed meanwhile, or signed out.
  }
}

async function loadPeers() {
  if (!ui.selected) return;
  const hash = ui.selected;
  try {
    const result = await api(`/api/torrents/${hash}/peers`);
    if (ui.selected === hash) {
      ui.peers = { hash, list: result.peers, attribution: result.attribution };
      if (ui.tab === "peers") renderDetail(true);
    }
  } catch {
    // Removed meanwhile, or signed out.
  }
}

const visible = () => !$("#app").hidden && document.visibilityState === "visible";
// Details change slowly; peers quickly. Only fetched while shown.
setInterval(() => { if (visible() && ui.tab !== "peers") loadDetails(); }, 5000);
setInterval(() => { if (visible() && ui.tab === "peers") loadPeers(); }, 2000);

for (const tab of document.querySelectorAll(".tabs button")) {
  tab.addEventListener("click", () => {
    ui.tab = tab.dataset.tab;
    for (const other of document.querySelectorAll(".tabs button")) other.classList.toggle("active", other === tab);
    renderDetail(true);
    const body = $("#detail-body");
    body.classList.remove("fade");
    void body.offsetWidth; // restart the animation
    body.classList.add("fade");
    if (ui.tab === "peers") loadPeers();
  });
}

function field(label, value) {
  return element("div", { class: "field" }, [element("span", { class: "label", text: label }), element("span", { class: "value", text: value })]);
}

// The overview follows every update and is patched in place, so text in
// it stays selectable; the other tabs redraw when their data arrives.
function renderDetail(force) {
  const body = $("#detail-body");
  const torrent = selectedTorrent();
  if (!torrent) {
    if (ui.detailShape !== "none") {
      ui.detailShape = "none";
      body.replaceChildren(element("p", { class: "muted", text: "Select a torrent to see its details." }));
    }
    return;
  }
  if (ui.tab === "overview") {
    renderOverview(body, torrent);
  } else if (force || ui.detailShape !== `${ui.tab}:${torrent.hash}`) {
    ui.detailShape = `${ui.tab}:${torrent.hash}`;
    if (ui.tab === "files") body.replaceChildren(...filesView());
    else if (ui.tab === "peers") body.replaceChildren(...peersView());
    else body.replaceChildren(...trackersView());
  }
}

function renderOverview(body, torrent) {
  const details = ui.details;
  const swarm = torrent.seeders !== null || torrent.leechers !== null
    ? ` · swarm ${torrent.seeders ?? "?"} seeders, ${torrent.leechers ?? "?"} leechers` : "";
  const pieces = details && details.pieces
    ? `${details.verifiedPieces ?? "?"} / ${details.pieces} pieces` : "";
  const values = [
    [
      ["Status", torrent.statusLabel + (torrent.error ? ` — ${torrent.error}` : "")],
      ["Progress", torrent.progress === null ? "unknown until checked" : `${percent(torrent.progress)}${pieces ? ` · ${pieces}` : ""}`],
      ["Size", torrent.size === null ? "unknown until the metadata arrives" : bytes(torrent.size)],
      ["Speed", `↓ ${rate(torrent.down) || "0 B/s"}   ↑ ${rate(torrent.up) || "0 B/s"}`],
      ["Peers", `${torrent.peers ?? 0} connected${swarm}`],
      ["ETA", eta(torrent.remaining, torrent.down) || "—"],
    ],
    [
      ["Info hash", torrent.hash],
      ["Format", details && details.format ? `${details.format}${details.private ? " · private" : ""}` : torrent.magnet ? "magnet link" : "—"],
      ["Saved in", torrent.savePath || "—"],
      ["Category", torrent.category || "none"],
      ["Tags", torrent.tags.length ? torrent.tags.join(", ") : "none"],
    ],
  ];
  const shape = `overview:${torrent.hash}`;
  if (ui.detailShape !== shape) {
    ui.detailShape = shape;
    body.replaceChildren(element("div", { class: "overview" }, values.map((column) =>
      element("div", { class: "column" }, column.map(([label, value]) => field(label, value))))));
    return;
  }
  const nodes = body.querySelectorAll(".field .value");
  values.flat().forEach(([, value], index) => { if (nodes[index]) setText(nodes[index], value); });
}

// The files as folders holding files, each with its total size.
function fileTree(files) {
  const root = { folders: new Map(), files: [], size: 0 };
  for (const file of files) {
    const parts = file.path.split("/");
    const name = parts.pop();
    let folder = root;
    root.size += file.size;
    let path = "";
    for (const part of parts) {
      path = path ? `${path}/${part}` : part;
      if (!folder.folders.has(part)) folder.folders.set(part, { name: part, path, folders: new Map(), files: [], size: 0 });
      folder = folder.folders.get(part);
      folder.size += file.size;
    }
    folder.files.push({ ...file, name });
  }
  return root;
}

const byName = (a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });

function renameButton(label, onClick) {
  return element("button", { class: "rename", title: label, attrs: { type: "button", "aria-label": label }, on: { click: onClick } }, [icon("rename")]);
}

function filesView() {
  const details = ui.details;
  if (!details) return [element("p", { class: "muted", text: "Loading…" })];
  const torrent = details.torrent;
  const renamedShown = torrent.name !== details.originalName;
  const nameRow = element("div", { class: "name-row" }, [
    element("div", { class: "name-text" }, [
      element("span", { class: "label", text: "Name" }),
      element("span", { class: "value", text: torrent.name }),
      renamedShown ? element("span", { class: "faint", text: `Its own name: ${details.originalName}` }) : null,
    ]),
    renameButton("Rename the torrent", () => renameTorrentDialog(torrent, details.originalName)),
  ]);
  if (torrent.magnet) return [nameRow, element("p", { class: "muted", text: "The file list arrives with the metadata." })];
  const rows = [];
  const walk = (folder, depth) => {
    for (const child of [...folder.folders.values()].sort(byName)) {
      rows.push(element("tr", { class: "folder" }, [
        element("td", { class: "path", attrs: { style: `--depth: ${depth}` } }, [icon("category"), element("span", { text: child.name })]),
        element("td", { class: "num", text: bytes(child.size) }),
        element("td", { class: "act" }, [renameButton(`Rename the folder ${child.name}`, () => renamePathDialog(torrent, child.path, true))]),
      ]));
      walk(child, depth + 1);
    }
    for (const file of folder.files.sort(byName)) {
      rows.push(element("tr", {}, [
        element("td", { class: "path", attrs: { style: `--depth: ${depth}` }, title: file.original ? `Renamed from ${file.original}` : "" }, [
          element("span", { text: file.name }),
          file.original ? element("span", { class: "renamed", text: "renamed" }) : null,
        ]),
        element("td", { class: "num", text: bytes(file.size) }),
        element("td", { class: "act" }, [renameButton(`Rename ${file.name}`, () => renamePathDialog(torrent, file.path, false))]),
      ]));
    }
  };
  walk(fileTree(details.files), 0);
  return [nameRow, element("table", { class: "files" }, [element("tbody", {}, rows)])];
}

function renameTorrentDialog(torrent, originalName) {
  const input = element("input", { attrs: { type: "text", value: torrent.name, placeholder: originalName, maxlength: "255", spellcheck: "false" } });
  const form = openDialog("Rename torrent", [
    input,
    element("p", { class: "muted", text: `Only the name shown changes; its files keep theirs. Leave it empty to show its own name, ${originalName}.` }),
  ], [cancelButton(), element("button", { class: "primary", text: "Rename", attrs: { type: "submit" } })]);
  form.addEventListener("submit", submitter(form, async () => {
    await api(`/api/torrents/${torrent.hash}/name`, { method: "PUT", body: { name: input.value.trim() || null } });
    loadDetails();
  }));
}

// Renames a file or folder where it is; a `/` in the new name moves it
// into a folder, made if needed.
function renamePathDialog(torrent, path, folder) {
  const parts = path.split("/");
  const name = parts.pop();
  const parent = parts.join("/");
  const input = element("input", { attrs: { type: "text", value: name, required: "", spellcheck: "false", autocapitalize: "off", autocorrect: "off" } });
  const form = openDialog(folder ? "Rename folder" : "Rename file", [
    input,
    element("p", { class: "muted", text: `${folder ? "Everything in it moves along. " : ""}The ${folder ? "folder" : "file"} is renamed on disk${parent ? ` inside ${parent}` : ""}; it keeps seeding. Use / to move it into a folder.` }),
  ], [cancelButton(), element("button", { class: "primary", text: "Rename", attrs: { type: "submit" } })]);
  // Select the name without its extension, as file managers do.
  if (!narrow.matches) {
    const dot = folder ? -1 : name.lastIndexOf(".");
    input.setSelectionRange(0, dot > 0 ? dot : name.length);
  }
  form.addEventListener("submit", submitter(form, async () => {
    const chosen = input.value.trim().replace(/^\/+|\/+$/g, "");
    if (!chosen) throw new ApiError(400, "Type a name.");
    if (chosen === name) return;
    const to = parent ? `${parent}/${chosen}` : chosen;
    await api(`/api/torrents/${torrent.hash}/rename`, { method: "POST", body: { from: path, to } });
    loadDetails();
  }));
}

function trackersView() {
  const details = ui.details;
  const torrent = selectedTorrent();
  const trackers = details ? details.trackers : [];
  // Asks trackers and the DHT for peers now rather than when next due.
  const reannounce = torrent && element("button", {
    class: "reannounce",
    attrs: { type: "button" },
    on: {
      click: (event) => busy(event.currentTarget,
        act(api(`/api/torrents/${torrent.hash}/reannounce`, { method: "POST" }))),
    },
  }, [icon("recheck"), element("span", { text: "Reannounce now" })]);
  const lines = trackers.length
    ? trackers.map((url) => element("li", { text: url }))
    : [element("li", { class: "muted", text: "No trackers: peers are found through the DHT and peer exchange." })];
  const announce = details && details.announce;
  const summary = announce && announce.trackersTotal
    ? element("p", { text: `Seeding announced to ${announce.trackersOk}/${announce.trackersTotal} trackers${announce.lastError ? ` · ${announce.lastError}` : ""}` })
    : null;
  return [reannounce, summary, element("ul", { class: "trackers" }, lines)].filter(Boolean);
}


// How the peer list can be ordered: label, value, and whether largest
// first is the natural direction.
const PEER_SORTS = {
  up: ["Upload speed", (peer) => peer.up, true],
  down: ["Download speed", (peer) => peer.down, true],
  uploaded: ["Uploaded", (peer) => peer.uploaded, true],
  downloaded: ["Downloaded", (peer) => peer.downloaded, true],
  progress: ["Progress", (peer) => peer.progress ?? -1, true],
  client: ["Client", (peer) => (peer.client || "").toLowerCase(), false],
  country: ["Country", (peer) => peer.country || "~", false],
  address: ["Address", (peer) => peer.address, false],
};

function setPeerSort(key) {
  const natural = PEER_SORTS[key][2];
  ui.peerSort = ui.peerSort.key === key
    ? { key, desc: !ui.peerSort.desc }
    : { key, desc: natural };
  renderDetail(true);
}

function sortedPeers(list) {
  const [, value] = PEER_SORTS[ui.peerSort.key];
  const direction = ui.peerSort.desc ? -1 : 1;
  // Ties: who gets most from us, then who gives most.
  const tie = (a, b) => b.up - a.up || b.down - a.down || b.uploaded - a.uploaded;
  return [...list].sort((a, b) => {
    const x = value(a);
    const y = value(b);
    if (x < y) return -direction;
    if (x > y) return direction;
    return tie(a, b);
  });
}

function peersView() {
  const peers = ui.peers;
  if (!peers || peers.hash !== ui.selected) return [element("p", { class: "muted", text: "Loading…" })];
  if (peers.list.length === 0) return [element("p", { class: "muted", text: "No peers connected." })];
  const list = sortedPeers(peers.list);
  const sending = list.filter((peer) => peer.up >= 1).length;
  const receiving = list.filter((peer) => peer.down >= 1).length;
  // On a phone the headers are hidden; this menu sorts there.
  const menu = element("select", {
    attrs: { "aria-label": "Sort peers by" },
    on: { change: (event) => setPeerSort(event.target.value) },
  }, Object.entries(PEER_SORTS).map(([key, [label]]) =>
    element("option", { text: label, attrs: { value: key } })));
  menu.value = ui.peerSort.key;
  const summary = element("div", { class: "peers-bar" }, [
    element("span", { class: "muted", text:
      `${list.length} peer${list.length === 1 ? "" : "s"} · sending to ${sending} · getting from ${receiving}` }),
    element("label", { class: "sort" }, [icon("sort"), menu]),
  ]);
  const rows = list.map((peer) => {
    const country = peer.country && peer.country !== "ZZ" ? peer.country : "";
    return element("tr", { class: peer.up >= 1 ? "sending" : "" }, [
      element("td", { class: "country", title: country }, [
        element("span", { class: "flag", text: flag(country), attrs: { "aria-hidden": "true" } }),
        element("span", { class: "code", text: country }),
      ]),
      element("td", { class: "address", text: peer.address }),
      element("td", { class: "client", text: peer.client || "—" }),
      // qBittorrent's flags, such as "D U I E P"; the tooltip says what each means.
      element("td", { class: "flags muted", text: peer.flags, title: peer.flagsDescription }),
      element("td", { class: "num", text: peer.progress === null ? "" : `${(peer.progress * 100).toFixed(0)}%` }),
      element("td", { class: "num down", text: rate(peer.down) }),
      element("td", { class: "num up", text: rate(peer.up) }),
      element("td", { class: "num total got", text: peer.downloaded ? bytes(peer.downloaded) : "", title: "Downloaded from it" }),
      element("td", { class: "num total sent", text: peer.uploaded ? bytes(peer.uploaded) : "", title: "Uploaded to it" }),
    ]);
  });
  const columns = [
    ["", "country"], ["Address", "address"], ["Client", "client"], ["Flags", null],
    ["Done", "progress"], ["Down", "down"], ["Up", "up"], ["Downloaded", "downloaded"], ["Uploaded", "uploaded"],
  ];
  const head = element("thead", {}, [element("tr", {}, columns.map(([text, key], index) => {
    const numeric = index >= 4;
    if (!key) return element("th", { text });
    const active = ui.peerSort.key === key;
    return element("th", {
      class: numeric ? "num" : "",
      attrs: { "aria-sort": active ? (ui.peerSort.desc ? "descending" : "ascending") : "none" },
    }, [element("button", {
      class: `peer-sort${active ? " sorted" : ""}${active && ui.peerSort.desc ? " desc" : ""}`,
      text: text || "Country",
      on: { click: () => setPeerSort(key) },
    })]);
  }))]);
  const credit = peers.list.some((peer) => peer.country)
    ? element("p", { class: "credit muted", text: peers.attribution })
    : null;
  return [summary, element("table", { class: "peers" }, [head, element("tbody", {}, rows)]), credit].filter(Boolean);
}

function renderNotices(notices) {
  for (const notice of notices) {
    // Only new ones, and not ones from before this page loaded.
    if (notice.id > ui.seenNotice) {
      if (ui.seenNotice !== 0 || notice.ageMs < 10000) toast(notice.level, notice.text);
      ui.seenNotice = notice.id;
    }
  }
}

function toast(level, text) {
  const leave = () => {
    if (item.classList.contains("leaving")) return;
    item.classList.add("leaving");
    afterAnimation(item, () => item.remove());
  };
  const item = element("div", { class: `toast ${level}`, text, on: { click: leave } });
  $("#toasts").append(item);
  setTimeout(leave, level === "error" ? 9000 : 5000);
}

// Shows a button working until `promise` settles, so a tap visibly lands.
async function busy(button, promise) {
  button.classList.add("busy");
  button.disabled = true;
  try {
    return await promise;
  } finally {
    button.classList.remove("busy");
    button.disabled = !selectedTorrent();
  }
}

// ----- actions -------------------------------------------------------------------------

$("#search").addEventListener("input", (event) => {
  ui.search = event.target.value.trim().toLowerCase();
  schedule();
});

for (const button of document.querySelectorAll(".selection-actions button")) {
  button.addEventListener("click", () => {
    const torrent = selectedTorrent();
    if (!torrent) return;
    const action = button.dataset.action;
    if (["start", "pause", "recheck"].includes(action)) {
      busy(button, act(api(`/api/torrents/${torrent.hash}/${action}`, { method: "POST" })));
    } else if (action === "remove") {
      removeDialog(torrent);
    } else if (action === "category") {
      categoryDialog(torrent);
    } else if (action === "tags") {
      tagsDialog(torrent);
    }
  });
}

$("#add-button").addEventListener("click", addDialog);
$("#settings-button").addEventListener("click", settingsDialog);

// Dropping a .torrent file anywhere adds it.
document.addEventListener("dragover", (event) => {
  if (!$("#app").hidden) event.preventDefault();
});
document.addEventListener("drop", (event) => {
  if ($("#app").hidden) return;
  event.preventDefault();
  for (const file of event.dataTransfer.files) upload(file);
});

async function upload(file) {
  const raw = await file.arrayBuffer();
  await act(api("/api/torrents/upload", { method: "POST", raw }));
}

// ----- dialogs ----------------------------------------------------------------------------

function openDialog(title, content, buttons) {
  const dialog = $("#dialog");
  dialog.classList.remove("closing");
  const form = element("form", { attrs: { method: "dialog" } }, [
    element("div", { class: "grabber", attrs: { "aria-hidden": "true" } }),
    element("h2", { text: title }),
    ...content,
    element("p", { class: "dialog-error error", attrs: { hidden: "" } }),
    element("div", { class: "buttons" }, buttons),
  ]);
  dialog.replaceChildren(form);
  dialog.showModal();
  // Focusing opens the keyboard on phones, hiding half the dialog.
  const first = form.querySelector("input, textarea, select");
  if (first && !narrow.matches) first.focus();
  return form;
}

function closeDialog() {
  const dialog = $("#dialog");
  if (!dialog.open || dialog.classList.contains("closing")) return;
  dialog.classList.add("closing");
  afterAnimation(dialog, () => {
    dialog.classList.remove("closing");
    dialog.style.transform = "";
    dialog.close();
  });
}

// Escape and taps outside close dialogs the same animated way; on a phone
// they can also be dragged down by the top.
$("#dialog").addEventListener("cancel", (event) => {
  event.preventDefault();
  closeDialog();
});
$("#dialog").addEventListener("click", (event) => {
  if (event.target === event.currentTarget) closeDialog();
});
draggable($("#dialog"), {
  axis: "y",
  start: (event) => narrow.matches && Boolean(event.target.closest(".grabber, h2")),
  dismiss: closeDialog,
});

function dialogError(form, message) {
  const node = form.querySelector(".dialog-error");
  node.textContent = message;
  node.hidden = false;
}

function cancelButton() {
  return element("button", { text: "Cancel", attrs: { type: "button" }, on: { click: closeDialog } });
}

// Runs `submit`, closing the dialog on success and showing failures in it.
function submitter(form, submit) {
  return async (event) => {
    event.preventDefault();
    try {
      await submit();
      closeDialog();
    } catch (error) {
      if (error.status !== 401) dialogError(form, error.message);
    }
  };
}

function confirmDialog(title, text, label, confirm) {
  const yes = element("button", { class: "danger", text: label, attrs: { type: "submit" } });
  const form = openDialog(title, [element("p", { text })], [cancelButton(), yes]);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    closeDialog();
    confirm();
  });
}

// Removing, optionally with the files, as qBittorrent asks.
function removeDialog(torrent) {
  const deleteFiles = element("input", { class: "switch", attrs: { type: "checkbox" } });
  const where = torrent.savePath ? ` in ${torrent.savePath}` : "";
  const about = element("p", { class: "muted" });
  const yes = element("button", { class: "danger", attrs: { type: "submit" } });
  const update = () => {
    about.textContent = deleteFiles.checked
      ? `Its files${where} are deleted for good. Other files there, and files another torrent uses, stay.`
      : `Its downloaded files stay on disk${where}.`;
    yes.textContent = deleteFiles.checked ? "Remove and delete files" : "Remove";
  };
  deleteFiles.addEventListener("change", update);
  update();
  const form = openDialog(`Remove “${torrent.name}”?`, [
    element("label", { class: "switch-row" }, [element("span", { text: "Also delete the downloaded files" }), deleteFiles]),
    about,
  ], [cancelButton(), yes]);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    closeDialog();
    const query = deleteFiles.checked ? "?deleteFiles=true" : "";
    act(api(`/api/torrents/${torrent.hash}${query}`, { method: "DELETE" }));
  });
}

function nameDialog(title, value, save) {
  const input = element("input", { attrs: { type: "text", maxlength: "64", value, required: "" } });
  const form = openDialog(title, [input], [cancelButton(), element("button", { class: "primary", text: "Save", attrs: { type: "submit" } })]);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    closeDialog();
    save(input.value.trim());
  });
}

function addDialog() {
  const source = element("textarea", {
    attrs: { rows: "3", placeholder: "magnet:?xt=… or https://example.org/file.torrent", spellcheck: "false", autocapitalize: "off", autocorrect: "off" },
  });
  const file = element("input", { attrs: { type: "file", accept: ".torrent,application/x-bittorrent", multiple: "" } });
  // Allowed folders are suggested; any folder inside them works too.
  const folders = ui.lists.saveDirs || [];
  const saveTo = element("input", {
    attrs: {
      type: "text",
      list: "save-folders",
      value: folders[0] || "",
      spellcheck: "false",
      autocapitalize: "off",
      autocorrect: "off",
    },
  });
  const suggestions = element("datalist", { attrs: { id: "save-folders" } },
    folders.map((folder) => element("option", { attrs: { value: folder } })));
  const categoryFolders = ui.lists.categoryFolders || {};
  const category = element("input", {
    attrs: { type: "text", list: "add-categories", placeholder: "none", maxlength: "64" },
  });
  const categories = element("datalist", { attrs: { id: "add-categories" } },
    ui.lists.categories.map((name) => element("option", { attrs: { value: name } })));
  const tags = element("input", { attrs: { type: "text", placeholder: "none, or several separated by commas" } });
  const layout = element("select", {}, [
    ["original", "As the torrent has it"],
    ["subfolder", "In a folder of its own"],
    ["nosubfolder", "Without its top folder"],
  ].map(([value, text]) => element("option", { text, attrs: { value } })));
  // Picking a category moves "Save in" to its folder, unless typed over.
  let typedFolder = false;
  saveTo.addEventListener("input", () => { typedFolder = true; });
  category.addEventListener("input", () => {
    if (!typedFolder) saveTo.value = categoryFolders[category.value.trim()] || folders[0] || "";
  });
  const form = openDialog("Add torrents", [
    element("label", { text: "Magnet links or .torrent URLs, one per line" }),
    source,
    element("label", { text: "Or upload .torrent files (or drop them on the page)" }),
    file,
    element("label", { text: "Category" }),
    category,
    categories,
    element("label", { text: "Tags" }),
    tags,
    element("label", { text: "Save in" }),
    saveTo,
    suggestions,
    element("p", { class: "muted", text: "The download folder, a folder inside it, a category's folder, or one set in storage.allowed_dirs." }),
    element("label", { text: "Content layout" }),
    layout,
    element("p", { class: "muted", text: "A single file can go in a folder named after it; a torrent's files can go straight in the save folder." }),
  ], [cancelButton(), element("button", { class: "primary", text: "Add", attrs: { type: "submit" } })]);
  form.addEventListener("submit", submitter(form, async () => {
    const lines = source.value.split("\n").map((line) => line.trim()).filter(Boolean);
    if (!lines.length && !file.files.length) throw new ApiError(400, "Paste a link or choose a file.");
    const savePath = saveTo.value.trim() || undefined;
    const chosenCategory = category.value.trim() || undefined;
    const chosenTags = tags.value.split(",").map((tag) => tag.trim()).filter(Boolean);
    const chosenLayout = layout.value === "original" ? undefined : layout.value;
    for (const line of lines) {
      await api("/api/torrents", { method: "POST", body: { source: line, savePath, category: chosenCategory, tags: chosenTags, layout: chosenLayout } });
    }
    const query = new URLSearchParams();
    if (chosenLayout) query.set("layout", chosenLayout);
    if (savePath) query.set("savePath", savePath);
    if (chosenCategory) query.set("category", chosenCategory);
    if (chosenTags.length) query.set("tags", chosenTags.join(","));
    const suffix = query.toString() ? `?${query}` : "";
    for (const chosen of file.files) await api(`/api/torrents/upload${suffix}`, { method: "POST", raw: await chosen.arrayBuffer() });
  }));
}

// A folder field suggesting the folders the web UI may save in.
function folderInput(value) {
  const input = element("input", {
    attrs: { type: "text", list: "folder-choices", value: value || "", placeholder: "the download folder", spellcheck: "false", autocapitalize: "off", autocorrect: "off" },
  });
  const choices = element("datalist", { attrs: { id: "folder-choices" } },
    (ui.lists.saveDirs || []).map((folder) => element("option", { attrs: { value: folder } })));
  return [input, choices];
}

function newCategoryDialog() {
  const name = element("input", { attrs: { type: "text", maxlength: "64", required: "" } });
  const [folder, choices] = folderInput("");
  const form = openDialog("New category", [
    element("label", { text: "Name" }),
    name,
    element("label", { text: "Save new torrents in (optional)" }),
    folder,
    choices,
  ], [cancelButton(), element("button", { class: "primary", text: "Create", attrs: { type: "submit" } })]);
  form.addEventListener("submit", submitter(form, async () => {
    const chosen = name.value.trim();
    if (folder.value.trim()) {
      await api("/api/categories/folder", { method: "PUT", body: { name: chosen, savePath: folder.value.trim() } });
    } else {
      await api("/api/categories", { method: "POST", body: { name: chosen } });
    }
  }));
}

function categoryFolderDialog(category) {
  const [folder, choices] = folderInput((ui.lists.categoryFolders || {})[category]);
  const form = openDialog(`Folder of ${category}`, [
    element("p", { class: "muted", text: "New torrents in this category are saved here. Torrents already in it stay where they are. Leave empty for the download folder." }),
    folder,
    choices,
  ], [cancelButton(), element("button", { class: "primary", text: "Save", attrs: { type: "submit" } })]);
  form.addEventListener("submit", submitter(form, () =>
    api("/api/categories/folder", { method: "PUT", body: { name: category, savePath: folder.value.trim() || null } })));
}

function categoryDialog(torrent) {
  const list = element("datalist", { attrs: { id: "known-categories" } },
    ui.lists.categories.map((name) => element("option", { attrs: { value: name } })));
  const input = element("input", {
    attrs: { type: "text", maxlength: "64", list: "known-categories", value: torrent.category || "", placeholder: "none" },
  });
  const known = element("div", { class: "chips" }, ui.lists.categories.map((name) => element("button", {
    class: "chip category",
    text: name,
    attrs: { type: "button" },
    on: { click: () => { input.value = name; } },
  })));
  const form = openDialog(`Category of ${torrent.name}`, [input, list, element("p", { class: "muted", text: "Leave empty for none. A new name creates the category." }), known],
    [cancelButton(), element("button", { class: "primary", text: "Save", attrs: { type: "submit" } })]);
  form.addEventListener("submit", submitter(form, () =>
    api(`/api/torrents/${torrent.hash}/category`, { method: "PUT", body: { category: input.value.trim() || null } })));
}

function tagsDialog(torrent) {
  const input = element("input", { attrs: { type: "text", value: torrent.tags.join(", "), placeholder: "none" } });
  const known = element("div", { class: "chips" }, ui.lists.tags.map((tag) => element("button", {
    class: "chip tag",
    text: `#${tag}`,
    attrs: { type: "button" },
    on: {
      click: () => {
        const tags = input.value.split(",").map((value) => value.trim()).filter(Boolean);
        if (!tags.includes(tag)) tags.push(tag);
        input.value = tags.join(", ");
      },
    },
  })));
  const form = openDialog(`Tags of ${torrent.name}`, [input, element("p", { class: "muted", text: "Separate tags with commas. Tap a tag to add it." }), known],
    [cancelButton(), element("button", { class: "primary", text: "Save", attrs: { type: "submit" } })]);
  form.addEventListener("submit", submitter(form, () => {
    const tags = input.value.split(",").map((value) => value.trim()).filter(Boolean);
    return api(`/api/torrents/${torrent.hash}/tags`, { method: "PUT", body: { tags } });
  }));
}

function settingsDialog() {
  const rows = ui.lists.settings.map((setting) => {
    let input;
    if (setting.kind === "bool") {
      input = element("input", { attrs: { type: "checkbox" } });
      input.checked = setting.value === "true";
    } else if (setting.kind === "choice") {
      input = element("select", {}, setting.choices.map((choice) => element("option", { text: choice, attrs: { value: choice } })));
      input.value = setting.value;
    } else {
      input = element("input", { attrs: { type: "text", value: setting.value, autocapitalize: "off", autocorrect: "off" } });
    }
    input.disabled = !setting.editable;
    input.dataset.key = setting.key;
    input.dataset.original = setting.value;
    input.id = `setting-${setting.key}`;
    const notes = [];
    if (!setting.editable) notes.push("change it in the settings file or with `empirebt config set`");
    if (setting.overridden) notes.push("a command-line flag or environment variable overrides it this run");
    if (setting.needsRestart) notes.push("applies after restart");
    return element("div", { class: `setting kind-${setting.kind}` }, [
      element("label", { class: "key", text: setting.key, attrs: { for: input.id } }),
      input,
      element("p", { class: "muted", text: setting.description + (notes.length ? ` (${notes.join("; ")})` : "") }),
    ]);
  });
  const form = openDialog("Settings", [element("div", { class: "settings" }, [
    appearanceSection(),
    element("h3", { class: "settings-heading", text: "EmpireBT" }),
    ...rows,
  ])],
    [cancelButton(), element("button", { class: "primary", text: "Save changes", attrs: { type: "submit" } })]);
  form.classList.add("wide");
  form.addEventListener("submit", submitter(form, async () => {
    for (const input of form.querySelectorAll("[data-key]")) {
      if (input.disabled) continue;
      const value = input.type === "checkbox" ? String(input.checked) : input.value.trim();
      if (value !== input.dataset.original) {
        await api("/api/settings", { method: "PUT", body: { key: input.dataset.key, value } });
      }
    }
  }));
}

// ----- appearance ----------------------------------------------------------------------
//
// Theme and accent color are per browser: saved here, never on the server.

const ACCENTS = [
  ["mono", "Plain", null],
  ["blue", "Blue", "#4c8dff"],
  ["teal", "Teal", "#2cc5db"],
  ["green", "Green", "#3ddc84"],
  ["yellow", "Yellow", "#facc15"],
  ["orange", "Orange", "#fb923c"],
  ["red", "Red", "#f87171"],
  ["pink", "Pink", "#f472b6"],
  ["purple", "Purple", "#a78bfa"],
];

const appearance = { theme: "system", accent: "mono" };
const APPEARANCE_KEY = "empirebt.appearance";

function loadAppearance() {
  try {
    const saved = JSON.parse(localStorage.getItem(APPEARANCE_KEY) || "{}");
    if (["system", "dark", "light"].includes(saved.theme)) appearance.theme = saved.theme;
    if (accentColor(saved.accent) !== undefined) appearance.accent = saved.accent;
  } catch {
    // Storage blocked or damaged: keep the defaults.
  }
  applyAppearance();
}

function saveAppearance() {
  try {
    localStorage.setItem(APPEARANCE_KEY, JSON.stringify(appearance));
  } catch {
    // Private browsing may refuse; it still applies to this page.
  }
}

// A preset's color, a custom "#rrggbb", null for plain, or undefined when
// `accent` is neither.
function accentColor(accent) {
  const preset = ACCENTS.find(([key]) => key === accent);
  if (preset) return preset[2];
  return typeof accent === "string" && /^#[0-9a-f]{6}$/i.test(accent) ? accent.toLowerCase() : undefined;
}

function applyAppearance() {
  const root = document.documentElement;
  if (appearance.theme === "system") delete root.dataset.theme;
  else root.dataset.theme = appearance.theme;
  const color = accentColor(appearance.accent);
  if (color) {
    const [r, g, b] = [1, 3, 5].map((index) => parseInt(color.slice(index, index + 2), 16));
    // Dark text on light colors, white on dark ones.
    const light = (0.299 * r + 0.587 * g + 0.114 * b) / 255 > 0.6;
    root.style.setProperty("--accent", color);
    root.style.setProperty("--accent-text", light ? "#0b0b0c" : "#ffffff");
    root.style.setProperty("--accent-soft", `rgb(${r} ${g} ${b} / 0.15)`);
  } else {
    for (const name of ["--accent", "--accent-text", "--accent-soft"]) root.style.removeProperty(name);
  }
  // The browser's own bar follows the page.
  const background = getComputedStyle(root).getPropertyValue("--bg").trim();
  for (const meta of document.querySelectorAll('meta[name="theme-color"]')) {
    if (appearance.theme === "system") meta.content = meta.media.includes("light") ? "#f4f4f5" : "#0b0b0c";
    else meta.content = background;
  }
}

function appearanceSection() {
  const segment = (value, label) => element("button", {
    class: appearance.theme === value ? "active" : "",
    text: label,
    attrs: { type: "button", "aria-pressed": String(appearance.theme === value) },
    on: {
      click: (event) => {
        appearance.theme = value;
        for (const other of event.currentTarget.parentNode.children) {
          other.classList.toggle("active", other === event.currentTarget);
          other.setAttribute("aria-pressed", String(other === event.currentTarget));
        }
        applyAppearance();
        saveAppearance();
      },
    },
  });
  const swatches = element("div", { class: "swatches" });
  const pick = (accent) => {
    appearance.accent = accent;
    const custom = !ACCENTS.some(([key]) => key === accent);
    for (const node of swatches.children) {
      const chosen = node.classList.contains("custom-color") ? custom : node.dataset.accent === accent;
      node.classList.toggle("active", chosen);
    }
    applyAppearance();
    saveAppearance();
  };
  for (const [key, label, color] of ACCENTS) {
    const swatch = element("button", {
      class: `swatch${key === "mono" ? " mono" : ""}${appearance.accent === key ? " active" : ""}`,
      title: label,
      attrs: { type: "button", "aria-label": `${label} accent`, "data-accent": key },
      on: { click: () => pick(key) },
    });
    if (color) swatch.style.setProperty("--swatch", color);
    swatches.append(swatch);
  }
  const custom = !ACCENTS.some(([key]) => key === appearance.accent);
  const input = element("input", {
    attrs: { type: "color", "aria-label": "Custom accent color", value: custom ? appearance.accent : "#4c8dff" },
    on: { input: (event) => pick(event.target.value) },
  });
  swatches.append(element("label", { class: `custom-color${custom ? " active" : ""}`, title: "Any color" }, [input]));
  return element("div", { class: "appearance" }, [
    element("h3", { text: "Appearance" }),
    element("div", { class: "row-line" }, [
      element("span", { text: "Theme" }),
      element("div", { class: "segments" }, [segment("system", "System"), segment("dark", "Dark"), segment("light", "Light")]),
    ]),
    element("div", { class: "row-line" }, [element("span", { text: "Accent" })]),
    swatches,
    element("p", { class: "note", text: "Saved in this browser and applied right away." }),
  ]);
}

loadAppearance();

// ----- start --------------------------------------------------------------------------

async function start() {
  // A one-time login link carries its token in the fragment, which never
  // reaches the server's logs; take it, then drop it from the address bar.
  const token = new URLSearchParams(location.hash.slice(1)).get("login");
  if (token) {
    history.replaceState(null, "", location.pathname);
    await login({ token });
    if (!$("#app").hidden) return;
  }
  try {
    await api("/api/session");
    await showApp();
  } catch {
    showLogin();
  }
}

start();
