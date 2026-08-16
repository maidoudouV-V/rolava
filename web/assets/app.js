import { api } from "./api.js";
import { ConfigController } from "./config.js";
import { ConversationsController } from "./conversations.js";
import { LogsController } from "./logs.js";
import { PromptController } from "./prompts.js";
import { escapeHtml, formatTime, initial, refreshIcons, toast } from "./ui.js";

const pageNames = {
  overview: "概览", conversations: "会话管理", logs: "运行日志", connection: "连接配置",
  models: "模型服务", behavior: "消息行为", prompts: "角色与提示词", access: "权限与工具",
};
const configPages = new Set(["connection", "models", "behavior", "access"]);
let initialized = false;
let dirty = false;
let startedAt = null;

const companionMessages = [
  "正在关注每一段对话",
  "正安静查看网友分享",
  "正在认真准备回复",
  "正在关注平凡的日常",
  "准备编写新的故事",
  "正在浏览猫猫meme",
  "正在网上冲浪🏄‍♀️",
  "正在围观群友聊天",
  "正在悄悄记下重点",
  "正在等待新的话题",
  "正在整理聊天思路",
  "正在研究网友发言",
  "正在观察互联网热点",
  "正在翻看新鲜消息",
  "正在努力跟上话题",
  "正在潜水🫧",
  "正在思考晚饭吃什么",
  "正在补充赛博能量⚡",
];

const conversations = new ConversationsController();
new LogsController();
const config = new ConfigController({ onRestart: restart, onDirty: markDirty });
const prompts = new PromptController({ onRestart: restart });

document.getElementById("login-form").addEventListener("submit", async event => {
  event.preventDefault();
  const error = document.getElementById("login-error");
  error.hidden = true;
  api.token = document.getElementById("login-token").value.trim();
  try { await api.post("/auth/verify"); await enterApp(); }
  catch (requestError) { api.token = ""; error.textContent = requestError.message; error.hidden = false; }
});

document.getElementById("logout").addEventListener("click", () => {
  api.token = ""; location.reload();
});
document.getElementById("save-config-only").addEventListener("click", () => saveConfig(false));
document.getElementById("save-config").addEventListener("click", () => saveConfig(true));
document.getElementById("mobile-menu").addEventListener("click", () => document.querySelector(".sidebar").classList.toggle("open"));
document.querySelector(".nav").addEventListener("click", event => {
  const button = event.target.closest("[data-page]"); if (button) showPage(button.dataset.page);
});
document.body.addEventListener("click", event => {
  const button = event.target.closest("[data-open-page]"); if (button) showPage(button.dataset.openPage);
  const recent = event.target.closest("[data-open-conversation]");
  if (recent) { showPage("conversations"); conversations.select(Number(recent.dataset.openConversation)); }
});
window.addEventListener("admin-auth-expired", () => {
  document.getElementById("app-shell").hidden = true;
  document.getElementById("login-screen").hidden = false;
  document.getElementById("login-error").textContent = "登录已失效，请重新输入 Token";
  document.getElementById("login-error").hidden = false;
});

async function enterApp() {
  document.getElementById("login-screen").hidden = true;
  document.getElementById("app-shell").hidden = false;
  refreshIcons();
  if (initialized) return;
  initialized = true;
  const results = await Promise.allSettled([loadStatus(), loadRecent(), conversations.load(), config.load(), prompts.load()]);
  for (const result of results) if (result.status === "rejected") toast(result.reason.message, true);
  refreshIcons();
}

function showPage(page) {
  document.querySelectorAll(".page").forEach(element => element.classList.toggle("active", element.id === `page-${page}`));
  document.querySelectorAll(".nav-button").forEach(button => button.classList.toggle("active", button.dataset.page === page));
  document.getElementById("page-name").textContent = pageNames[page] || "Rolava";
  document.getElementById("save-config-only").hidden = !configPages.has(page);
  document.getElementById("save-config").hidden = !configPages.has(page);
  document.querySelector(".sidebar").classList.remove("open");
  window.scrollTo({ top: 0, behavior: "smooth" });
}

function markDirty() {
  dirty = true;
  setSaveState("有未保存的修改", "dirty");
  document.getElementById("save-config-only").disabled = false;
  document.getElementById("save-config").disabled = false;
}

async function saveConfig(restartAfterSave) {
  const buttons = [document.getElementById("save-config-only"), document.getElementById("save-config")];
  buttons.forEach(button => { button.disabled = true; });
  setSaveState("正在校验配置", "saving");
  try {
    await config.save(restartAfterSave);
    if (!restartAfterSave) {
      dirty = false;
      setSaveState("已保存，重启后生效", "saved");
      toast("配置已保存，重启 Rolava 后生效");
    }
  } catch (error) {
    toast(error.message, true);
    buttons.forEach(button => { button.disabled = false; });
    setSaveState("保存失败", "error");
  }
}

function setSaveState(message, state) {
  const element = document.getElementById("save-state");
  element.textContent = message;
  element.className = `save-state ${state}`;
}

async function loadStatus() {
  const status = await api.get("/status");
  startedAt = status.started_at;
  const online = status.onebot_online !== false && Boolean(status.bot_id || status.last_event_at);
  document.getElementById("sidebar-dot").classList.toggle("online", online);
  const botName = status.bot_name || (status.bot_id ? `QQ ${status.bot_id}` : "Rolava");
  document.getElementById("sidebar-bot").textContent = online ? `${botName} 正在运行` : "OneBot 状态异常";
  document.getElementById("sidebar-bot-id").textContent = status.bot_id ? `QQ ${status.bot_id}` : "等待 OneBot 事件";
  document.getElementById("hero-status").textContent = online
    ? `${botName} ${companionMessages[Math.floor(Math.random() * companionMessages.length)]}`
    : `${botName} 暂时离线，等待重新连接`;
  document.getElementById("hero-detail").textContent = `已运行 ${formatDuration(status.uptime_seconds)}${status.last_event_at ? ` · 最近事件 ${formatTime(status.last_event_at, true)}` : ""}`;
  document.getElementById("metric-conversations").textContent = status.conversations;
  document.getElementById("metric-conversation-kinds").textContent = `${status.group_conversations} 个群聊 · ${status.direct_conversations} 个私聊`;
  document.getElementById("metric-messages").textContent = status.messages_today;
  document.getElementById("metric-memories").textContent = status.user_memories + status.character_memories;
  document.getElementById("metric-tasks").textContent = status.scheduled_tasks;
}

async function loadRecent() {
  const response = await api.get("/conversations?kind=all&limit=4");
  const container = document.getElementById("recent-conversations");
  container.innerHTML = response.items.length ? response.items.map(item => `<button class="recent-item" data-open-conversation="${item.id}"><span class="avatar ${item.kind === "direct" ? "direct" : ""}">${escapeHtml(initial(item.title))}</span><span><strong>${escapeHtml(item.title)}</strong><small>${escapeHtml(item.latest_sender_name ? `${item.latest_sender_name}：${item.latest_content || ""}` : item.latest_content || "暂无消息")}</small></span><time>${formatTime(item.latest_message_at)}</time></button>`).join("") : '<div class="empty">当前没有会话</div>';
}

function formatDuration(seconds) {
  const days = Math.floor(seconds / 86400); const hours = Math.floor(seconds % 86400 / 3600); const minutes = Math.floor(seconds % 3600 / 60);
  if (days) return `${days} 天 ${hours} 小时`;
  if (hours) return `${hours} 小时 ${minutes} 分钟`;
  return `${minutes} 分钟`;
}

async function restart() {
  dirty = false;
  document.getElementById("restart-screen").hidden = false;
  document.getElementById("restart-message").textContent = "配置已保存，正在等待服务重新上线。";
  await new Promise(resolve => setTimeout(resolve, 900));
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const status = await api.get("/status");
      if (status.started_at !== startedAt) { location.reload(); return; }
    } catch (_) { /* worker 正在释放端口 */ }
    await new Promise(resolve => setTimeout(resolve, 800));
  }
  document.getElementById("restart-message").textContent = "服务暂未恢复，请检查终端日志后刷新页面。";
}

refreshIcons();
if (api.token) {
  api.post("/auth/verify").then(enterApp).catch(() => { api.token = ""; });
}
