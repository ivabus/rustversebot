"use strict";
const el = id => document.getElementById(id);
const textNode = (tag, value, className) => {
  const node = document.createElement(tag);
  node.textContent = value;
  if (className) node.className = className;
  return node;
};
const date = new Intl.DateTimeFormat("ru-RU", {
  timeZone: "Europe/Moscow", day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit"
});
const time = seconds => date.format(seconds * 1000) + " МСК";
let token = "";
let chats = [];
let activeView = "chats";
let page = 0;
let listRequest = 0;
let detailRequest = 0;
const pendingFeatures = new Set();

function logout() {
  token = "";
  chats = [];
  pendingFeatures.clear();
  page = 0;
  listRequest++;
  detailRequest++;
  for (const id of ["chat-rows", "session-list", "turns", "session-chat"]) el(id).replaceChildren();
  document.querySelectorAll("dialog[open]").forEach(dialog => dialog.close());
  for (const id of ["chats", "sessions", "logout"]) el(id).hidden = true;
  for (const id of ["token", "search", "chat-id"]) el(id).value = "";
  el("notice").textContent = "";
  el("login").hidden = false;
}

async function api(path, options = {}) {
  const key = token;
  let response;
  try {
    response = await fetch("/api/admin/" + path, {
      ...options, cache: "no-store", credentials: "omit", redirect: "error",
      headers: { Authorization: "Bearer " + key, ...options.headers }
    });
  } catch {
    throw new Error("Нет связи с панелью. Повторите попытку.");
  }
  if (token !== key) throw new Error("Сеанс входа завершён.");
  if (response.status === 401) {
    logout();
    throw new Error("Токен не принят. Проверьте его и войдите снова.");
  }
  if (response.status === 503) throw new Error("Задайте BOT_WEB_ADMIN_TOKEN при запуске бота, чтобы открыть панель.");
  if (response.status === 404) throw new Error("Диалог больше недоступен.");
  if (!response.ok) throw new Error("Не удалось выполнить запрос (HTTP " + response.status + ").");
  return response.status === 204 ? null : response.json();
}

function selectView(view) {
  activeView = view;
  document.querySelectorAll("[data-view]").forEach(button => {
    if (button.dataset.view === view) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  });
  if (!el("login").hidden) { el("token").focus(); return; }
  el("chats").hidden = view !== "chats";
  el("sessions").hidden = view !== "sessions";
  el("notice").textContent = "";
  if (view === "sessions") loadSessions();
}

function drawChats() {
  el("chat-count").textContent = chats.length;
  el("prompt-count").textContent = chats.filter(chat => chat.prompt).length;
  el("diff-count").textContent = chats.filter(chat => chat.diff).length;
  const query = el("search").value.trim().toLocaleLowerCase("ru");
  const fragment = document.createDocumentFragment();
  for (const chat of chats.filter(chat => String(chat.chat_id).includes(query) || chat.title?.toLocaleLowerCase("ru").includes(query))) {
    const row = document.createElement("tr");
    const name = document.createElement("td");
    if (chat.title) name.append(textNode("strong", chat.title), textNode("div", chat.chat_id, "meta"));
    else name.textContent = chat.chat_id;
    row.append(name, textNode("td", chat.players));
    for (const feature of ["prompt", "diff"]) {
      const cell = document.createElement("td");
      const toggle = document.createElement("button");
      toggle.className = "switch";
      const key = chat.chat_id + ":" + feature;
      toggle.dataset.key = key;
      toggle.disabled = pendingFeatures.has(key);
      toggle.setAttribute("role", "switch");
      toggle.setAttribute("aria-label", feature + " в чате " + chat.chat_id);
      toggle.setAttribute("aria-checked", String(chat[feature]));
      toggle.addEventListener("click", async () => {
        pendingFeatures.add(key);
        toggle.disabled = true;
        const enabled = !chat[feature];
        try {
          await saveFeature(chat.chat_id, feature, enabled);
          const saved = chats.find(item => item.chat_id === chat.chat_id);
          if (saved) saved[feature] = enabled;
          const visible = document.querySelector('[data-key="' + key + '"]');
          visible?.setAttribute("aria-checked", String(enabled));
          el(feature + "-count").textContent = chats.filter(item => item[feature]).length;
          el("notice").textContent = "Настройки чата " + chat.chat_id + " сохранены.";
        } catch (e) { el("notice").textContent = e.message; }
        finally {
          pendingFeatures.delete(key);
          const visible = document.querySelector('[data-key="' + key + '"]');
          if (visible) visible.disabled = false;
        }
      });
      cell.append(toggle);
      row.append(cell);
    }
    fragment.append(row);
  }
  el("chat-empty").hidden = Boolean(fragment.childElementCount);
  el("chat-empty").textContent = chats.length ? "По вашему запросу чаты не найдены." : "Чатов пока нет. Добавьте чат по его Telegram ID.";
  el("chat-rows").replaceChildren(fragment);
}

async function loadChats() {
  chats = await api("chats");
  drawChats();
  const filter = el("session-chat");
  const previous = filter.value;
  filter.replaceChildren(new Option("Все чаты", ""));
  for (const chat of chats) filter.add(new Option((chat.title || "Чат") + " · " + chat.chat_id, chat.chat_id));
  filter.value = previous;
}

function saveFeature(chat, feature, enabled) {
  return api("chats/" + chat + "/features/" + feature, {
    method: "PUT", headers: {"Content-Type": "application/json"}, body: JSON.stringify({enabled})
  });
}

function sessionMeta(session) {
  return ["Чат " + session.chat_id, session.topic && "Тема " + session.topic,
    "Реплик: " + session.turns, time(session.updated_at)].filter(Boolean).join(" · ");
}

async function loadSessions() {
  const request = ++listRequest;
  const query = new URLSearchParams({page});
  if (el("session-chat").value) query.set("chat", el("session-chat").value);
  const list = el("session-list");
  list.replaceChildren(textNode("p", "Загрузка диалогов…", "empty"));
  el("prev").disabled = true;
  el("next").disabled = true;
  el("page").textContent = "Страница " + (page + 1);
  try {
    const result = await api("sessions?" + query);
    if (request !== listRequest) return;
    list.replaceChildren();
    for (const session of result.items) {
      const item = document.createElement("button");
      item.className = "session-item";
      item.append(textNode("strong", session.title), textNode("span", sessionMeta(session), "meta"));
      item.addEventListener("click", () => openSession(session));
      list.append(item);
    }
    if (!result.items.length) list.append(textNode("p", "За последние 24 часа диалогов не найдено.", "empty"));
    el("next").disabled = !result.more;
  } catch (e) {
    if (request === listRequest) list.replaceChildren(textNode("p", e.message, "empty"));
    if (!token) el("notice").textContent = e.message;
  } finally { if (request === listRequest) el("prev").disabled = page === 0; }
}

async function openSession(session) {
  const request = ++detailRequest;
  el("transcript-meta").textContent = sessionMeta(session);
  el("turns").replaceChildren(textNode("p", "Загрузка реплик…", "empty"));
  el("transcript").showModal();
  try {
    const turns = await api("sessions/" + session.chat_id + "/" + session.root);
    if (request !== detailRequest) return;
    const fragment = document.createDocumentFragment();
    const numbers = new Map(turns.map((turn, index) => [turn.message, index + 1]));
    for (const turn of turns) {
      const item = document.createElement("article");
      item.className = "turn";
      const parent = numbers.get(turn.parent);
      item.append(textNode("div", "Реплика " + numbers.get(turn.message) +
        (parent ? " · в ответ на " + parent : "") + " · " + time(turn.created_at), "meta"),
        textNode("h3", "Вопрос"), textNode("p", turn.question),
        textNode("h3", "Ассистент"), textNode("p", turn.answer));
      fragment.append(item);
    }
    el("turns").replaceChildren(fragment);
  } catch (e) {
    if (request === detailRequest) el("turns").replaceChildren(textNode("p", e.message, "empty"));
    if (!token) el("notice").textContent = e.message;
  }
}

el("login-form").addEventListener("submit", async event => {
  event.preventDefault();
  const button = event.currentTarget.querySelector("button");
  button.disabled = true;
  token = el("token").value;
  try {
    await loadChats();
    el("token").value = "";
    el("login").hidden = true;
    el("logout").hidden = false;
    selectView(activeView);
  } catch (e) {
    logout();
    el("notice").textContent = e.message;
  } finally { button.disabled = false; }
});
el("add-form").addEventListener("submit", async event => {
  event.preventDefault();
  const id = el("chat-id").value.trim();
  if (!Number.isSafeInteger(Number(id)) || Math.abs(Number(id)) >= 2 ** 52 || Number(id) === 0) {
    el("add-status").textContent = "Введите корректный числовой Telegram ID.";
    return;
  }
  const button = event.currentTarget.querySelector(".primary");
  button.disabled = true;
  try {
    await saveFeature(id, el("first-feature").value, true);
    el("search").value = "";
    await loadChats();
    el("add-dialog").close();
    el("notice").textContent = "Настройки чата " + id + " сохранены.";
  } catch (e) { el("add-status").textContent = e.message; if (!token) el("notice").textContent = e.message; }
  finally { button.disabled = false; }
});
el("add").onclick = () => { el("add-form").reset(); el("add-status").textContent = ""; el("add-dialog").showModal(); };
el("transcript").onclose = () => { detailRequest++; el("turns").replaceChildren(); el("transcript-meta").textContent = ""; };
el("logout").onclick = logout;
el("search").oninput = drawChats;
el("prev").onclick = () => { page--; loadSessions(); };
el("next").onclick = () => { page++; loadSessions(); };
el("session-chat").onchange = () => { page = 0; loadSessions(); };
el("refresh").onclick = () => { page = 0; loadSessions(); };
document.querySelectorAll("[data-close]").forEach(button => button.onclick = () => button.closest("dialog").close());
document.querySelectorAll("[data-view]").forEach(button => button.onclick = () => selectView(button.dataset.view));
