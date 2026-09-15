import { api } from "./api.js";
import { escapeHtml, refreshIcons } from "./ui.js";

const LEVEL_WEIGHT = { TRACE: 0, DEBUG: 1, INFO: 2, WARN: 3, ERROR: 4 };
const MAX_CLIENT_ENTRIES = 1000;
const MAX_PREVIEW_CHARS = 500;
const MAX_PREVIEW_LINES = 8;

export class LogsController {
  constructor() {
    this.entries = [];
    this.expandedIds = new Set();
    this.afterId = null;
    this.instanceId = null;
    this.polling = false;
    this.paused = false;
    this.minimumLevel = "INFO";
    document.getElementById("log-stream").addEventListener("click", event => {
      const button = event.target.closest("button[data-log-id]");
      if (!button) return;
      const id = Number(button.dataset.logId);
      if (this.expandedIds.has(id)) this.expandedIds.delete(id);
      else this.expandedIds.add(id);
      this.render();
    });

    document.getElementById("log-level-filter").addEventListener("change", event => {
      this.minimumLevel = event.target.value;
      this.render();
    });
    document.getElementById("toggle-logs").addEventListener("click", () => this.togglePause());
    document.getElementById("clear-logs").addEventListener("click", () => {
      this.entries = [];
      this.render();
    });
    window.setInterval(() => this.poll(), 1200);
  }

  async poll() {
    if (!api.token || document.hidden || this.paused || this.polling || !document.getElementById("page-logs").classList.contains("active")) return;
    this.polling = true;
    try {
      const query = new URLSearchParams({ limit: "200" });
      if (this.afterId != null) query.set("after_id", String(this.afterId));
      let response = await api.get(`/logs?${query}`);
      if ((this.instanceId != null && response.instance_id !== this.instanceId)
        || (this.afterId != null && response.latest_id < this.afterId)) {
        this.entries = [];
        this.afterId = null;
        this.instanceId = null;
        this.render();
        response = await api.get("/logs?limit=200");
      }
      this.instanceId = response.instance_id;
      if (response.items.length) {
        this.entries.push(...response.items);
        if (this.entries.length > MAX_CLIENT_ENTRIES) this.entries.splice(0, this.entries.length - MAX_CLIENT_ENTRIES);
        this.afterId = response.items[response.items.length - 1].id;
        this.render();
      } else if (this.afterId == null) {
        this.afterId = response.latest_id;
        this.render();
      }
      this.setStatus(response.truncated ? "部分旧日志已被覆盖" : "实时更新", response.truncated ? "warning" : "live");
    } catch (_) {
      this.setStatus("连接中断，等待恢复", "warning");
    } finally {
      this.polling = false;
    }
  }

  togglePause() {
    this.paused = !this.paused;
    const button = document.getElementById("toggle-logs");
    button.innerHTML = this.paused
      ? '<i data-lucide="play"></i>继续'
      : '<i data-lucide="pause"></i>暂停';
    this.setStatus(this.paused ? "已暂停" : "实时更新", this.paused ? "paused" : "live");
    refreshIcons();
    if (!this.paused) this.poll();
  }

  setStatus(text, state) {
    const status = document.getElementById("log-status");
    status.textContent = text;
    status.className = `log-status ${state}`;
  }

  render() {
    const retainedIds = new Set(this.entries.map(entry => entry.id));
    for (const id of this.expandedIds) {
      if (!retainedIds.has(id)) this.expandedIds.delete(id);
    }
    const stream = document.getElementById("log-stream");
    const shouldFollow = stream.scrollHeight - stream.scrollTop - stream.clientHeight < 80;
    const minimumWeight = LEVEL_WEIGHT[this.minimumLevel];
    const visible = this.entries.filter(entry => (LEVEL_WEIGHT[entry.level] ?? 0) >= minimumWeight);
    stream.innerHTML = visible.length
      ? visible.map(entry => this.entryHtml(entry)).join("")
      : '<div class="empty log-empty">当前级别暂无日志</div>';
    if (shouldFollow) stream.scrollTop = stream.scrollHeight;
  }

  entryHtml(entry) {
    const target = String(entry.target || "").replace(/^rolava(?:::)?/, "") || "rolava";
    const message = String(entry.message || "事件");
    const fields = String(entry.fields || "");
    const text = fields ? `${message}\n${fields}` : message;
    const preview = text.slice(0, MAX_PREVIEW_CHARS).split("\n").slice(0, MAX_PREVIEW_LINES).join("\n");
    const collapsible = preview.length < text.length;
    const expanded = this.expandedIds.has(entry.id);
    const content = collapsible && !expanded
      ? `<pre>${escapeHtml(preview)}…</pre>`
      : `<span>${escapeHtml(message)}</span>${fields ? `<pre>${escapeHtml(fields)}</pre>` : ""}`;
    return `<article class="log-entry level-${entry.level.toLowerCase()}">
      <time>${formatLogTime(entry.timestamp)}</time>
      <strong>${escapeHtml(entry.level)}</strong>
      <code title="${escapeHtml(entry.target)}">${escapeHtml(target)}</code>
      <div>${content}${collapsible ? `<button type="button" class="log-expand" data-log-id="${entry.id}" aria-expanded="${expanded}">${expanded ? "收起" : "展开"}</button>` : ""}</div>
    </article>`;
  }
}

function formatLogTime(timestamp) {
  return new Intl.DateTimeFormat("zh-CN", {
    hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false,
  }).format(new Date(timestamp * 1000));
}
