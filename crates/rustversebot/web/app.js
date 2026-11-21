"use strict";
const el = id => document.getElementById(id);
const chat = el("chat");
const kind = el("type");
const query = new URLSearchParams(location.search);
const validUid = value => /^[0-9]{6,12}$/.test(value || "");
const validKind = value => ["deadly_assault", "shiyu_defense"].includes(value);
const scores = new Intl.NumberFormat("ru");
let historyRequest = 0;
let boardRequest = 0;

async function get(path) {
  const result = await fetch(path);
  if (!result.ok) throw new Error("HTTP " + result.status);
  return result.json();
}

function updateUrl() {
  const params = new URLSearchParams({kind: kind.value});
  if (chat.value) params.set("chat", chat.value);
  if (validUid(el("uid").value)) params.set("uid", el("uid").value);
  history.replaceState(null, "", location.pathname + "?" + params);
}

async function loadBoard() {
  const request = ++boardRequest;
  const rows = el("rows");
  rows.replaceChildren();
  el("summary").textContent = "Загрузка рейтинга…";
  try {
    const data = await get("/api/chats/" + chat.value + "/leaderboard/" + kind.value);
    if (request !== boardRequest) return;
    for (const [index, player] of data.entries()) {
      const row = rows.insertRow();
      row.insertCell().textContent = index + 1;
      const button = document.createElement("button");
      button.className = "player";
      button.textContent = player.nickname || "Без имени";
      button.onclick = () => loadHistory(player.uid);
      row.insertCell().append(button);
      row.insertCell().textContent = player.uid;
      const score = row.insertCell();
      score.className = "number";
      score.textContent = scores.format(player.total_score);
    }
    el("summary").textContent = data.length ? "Игроков в рейтинге: " + data.length : "Для этого режима пока нет результатов.";
  } catch { if (request === boardRequest) el("summary").textContent = "Не удалось загрузить рейтинг."; }
}

function message(value) {
  const p = document.createElement("p");
  p.className = "empty";
  p.textContent = value;
  el("chart").replaceChildren(p);
}

function draw(data) {
  if (!data.length) { message("История пока пуста."); return; }
  const values = data.map(row => row.total_score);
  const min = Math.min(...values), max = Math.max(...values);
  const points = values.map((score, i) => [
    65 + i * 810 / Math.max(1, values.length - 1),
    max === min ? 105 : 185 - 155 * (score - min) / (max - min)
  ]);
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 900 215");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "График результатов игрока");
  const append = (tag, attrs) => {
    const node = document.createElementNS(svg.namespaceURI, tag);
    for (const [key,value] of Object.entries(attrs)) node.setAttribute(key, value);
    svg.append(node);
    return node;
  };
  for (const [value, y] of max === min ? [[max,105]] : [[max,30],[min,185]]) {
    append("line", {class:"chart-grid", x1:65, x2:875, y1:y, y2:y});
    append("text", {class:"chart-label", x:0, y:y+4}).textContent = scores.format(value);
  }
  append("polyline", {class:"chart-line", points:points.map(point => point.join(",")).join(" ")});
  points.forEach(([x,y], i) => {
    const dot = append("circle", {class:"chart-dot", cx:x, cy:y, r:4});
    const title = document.createElementNS(svg.namespaceURI, "title");
    title.textContent = scores.format(values[i]) + " · " + (data[i].season_start_msk || "—");
    dot.append(title);
  });
  const legend = document.createElement("div");
  legend.className = "legend";
  for (const label of ["Снимков: " + data.length, (data[0].season_start_msk || "—") + " — " + (data.at(-1).season_end_msk || "—")]) {
    const span = document.createElement("span");
    span.textContent = label;
    legend.append(span);
  }
  el("chart").replaceChildren(svg, legend);
}

async function loadHistory(uid) {
  if (!validUid(uid)) return;
  const request = ++historyRequest;
  el("uid").value = uid;
  updateUrl();
  message("Загрузка истории…");
  try {
    const data = await get("/api/users/" + uid + "/history");
    if (request === historyRequest) draw(data.filter(entry => entry.endgame_type === kind.value));
  } catch { if (request === historyRequest) message("Не удалось загрузить историю."); }
}

async function init() {
  if (validKind(query.get("kind"))) kind.value = query.get("kind");
  try {
    const chats = await get("/api/chats");
    for (const item of chats) chat.add(new Option("Чат " + item.chat_id + " · " + item.user_count + " игр.", item.chat_id));
    if (chats.some(item => String(item.chat_id) === query.get("chat"))) chat.value = query.get("chat");
    if (chats.length) loadBoard();
    else el("summary").textContent = "Нет зарегистрированных игроков.";
  } catch { el("summary").textContent = "Не удалось загрузить список чатов."; }
  if (validUid(query.get("uid"))) loadHistory(query.get("uid"));
}

chat.onchange = () => {
  historyRequest++;
  el("uid").value = "";
  message("Выберите игрока в таблице или введите UID.");
  updateUrl();
  loadBoard();
};
kind.onchange = () => {
  updateUrl();
  if (chat.value) loadBoard();
  if (validUid(el("uid").value)) loadHistory(el("uid").value);
};
el("history-form").onsubmit = event => { event.preventDefault(); loadHistory(el("uid").value); };
init();
