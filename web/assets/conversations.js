import { api } from "./api.js";
import { escapeHtml, formatTime, initial, openDialog, refreshIcons, toast } from "./ui.js";

export class ConversationsController {
  constructor() {
    this.kind = "all";
    this.cursor = null;
    this.items = [];
    this.selectedId = null;
    this.pollingMessages = false;
    document.getElementById("conversation-filters").addEventListener("click", event => {
      const button = event.target.closest("[data-kind]");
      if (!button || button.classList.contains("active")) return;
      document.querySelectorAll("#conversation-filters button").forEach(item => item.classList.toggle("active", item === button));
      this.kind = button.dataset.kind; this.load(true);
    });
    document.getElementById("conversation-list").addEventListener("click", event => {
      const row = event.target.closest("[data-conversation-id]");
      if (row) this.select(Number(row.dataset.conversationId));
    });
    document.getElementById("load-conversations").addEventListener("click", () => this.load(false));
    this.messagePollTimer = window.setInterval(() => this.pollMessages(), 1500);
  }

  async load(reset = true) {
    if (reset) { this.cursor = null; this.items = []; }
    const query = new URLSearchParams({ kind: this.kind, limit: "30" });
    if (this.cursor) query.set("cursor", this.cursor);
    const response = await api.get(`/conversations?${query}`);
    this.items.push(...response.items);
    this.cursor = response.next_cursor;
    for (const [kind, count] of Object.entries(response.counts)) {
      const element = document.querySelector(`#conversation-filters [data-kind="${kind}"] span`);
      if (element) element.textContent = count;
    }
    this.renderList();
    document.getElementById("load-conversations").hidden = !this.cursor;
  }

  renderList() {
    const container = document.getElementById("conversation-list");
    if (!this.items.length) { container.innerHTML = '<div class="empty">当前没有会话</div>'; return; }
    container.innerHTML = this.items.map(item => `<button class="conversation-row ${item.id === this.selectedId ? "active" : ""}" data-conversation-id="${item.id}">
      <span class="avatar ${item.kind === "direct" ? "direct" : ""}">${escapeHtml(initial(item.title))}</span>
      <span><strong>${escapeHtml(item.title)}</strong><small>${escapeHtml(item.latest_sender_name ? `${item.latest_sender_name}：${item.latest_content || ""}` : item.latest_content || "暂无消息")}</small></span>
      <span>${item.latest_message_at ? `<time>${formatTime(item.latest_message_at)}</time>` : ""}${item.unread_count ? `<b class="unread">${item.unread_count > 99 ? "99+" : item.unread_count}</b>` : ""}</span>
    </button>`).join("");
  }

  async select(id) {
    this.selectedId = id; this.renderList();
    const detail = document.getElementById("conversation-detail");
    detail.innerHTML = '<div class="empty detail-empty"><span class="spinner"></span>正在读取会话</div>';
    try {
      const [conversation, messages, users, characters, tasks] = await Promise.all([
        api.get(`/conversations/${id}`),
        api.get(`/conversations/${id}/messages?limit=50`),
        api.get(`/conversations/${id}/user-memories`).catch(error => ({ users: [], error: error.message })),
        api.get(`/conversations/${id}/character-memories`),
        api.get(`/conversations/${id}/scheduled-tasks`),
      ]);
      if (this.selectedId !== id) return;
      this.detailData = { conversation, messages, users, characters, tasks };
      this.renderDetail();
    } catch (error) { detail.innerHTML = `<div class="empty detail-empty"><strong>会话读取失败</strong><span>${escapeHtml(error.message)}</span></div>`; }
  }

  renderDetail() {
    const { conversation } = this.detailData;
    const title = conversation.title || conversation.source_id;
    document.getElementById("conversation-detail").innerHTML = `
      <header class="detail-header"><div class="detail-identity"><span class="avatar ${conversation.kind === "direct" ? "direct" : ""}">${escapeHtml(initial(title))}</span><div><h2>${escapeHtml(title)}</h2><p>${conversation.kind === "group" ? `群聊${conversation.member_count != null ? ` · ${conversation.member_count} 位成员` : ""}` : "私聊"} · QQ ${escapeHtml(conversation.source_id)}</p></div></div></header>
      <nav class="detail-tabs"><button class="active" data-tab="messages">聊天记录</button><button data-tab="users">用户记忆</button><button data-tab="characters">角色记忆</button><button data-tab="tasks">定时任务</button></nav>
      <div class="detail-body">
        <section class="tab-panel active" data-panel="messages">${this.messagesHtml()}</section>
        <section class="tab-panel" data-panel="users">${this.userMemoriesHtml()}</section>
        <section class="tab-panel" data-panel="characters">${this.characterMemoriesHtml()}</section>
        <section class="tab-panel" data-panel="tasks">${this.tasksHtml()}</section>
      </div>`;
    this.bindDetailEvents();
    refreshIcons();
    requestAnimationFrame(() => {
      const body = document.querySelector("#conversation-detail .detail-body");
      if (body) body.scrollTop = body.scrollHeight;
    });
  }

  messagesHtml() {
    const items = this.detailData.messages.items;
    if (!items.length) return '<div class="empty">暂无聊天记录</div>';
    return `<div class="chat-list">${items.map(message => this.messageHtml(message)).join("")}</div>`;
  }

  messageHtml(message) {
    return `<article class="chat-message ${message.is_bot ? "bot" : ""}" data-message-id="${message.id}"><span class="avatar">${escapeHtml(initial(message.sender_name))}</span><div class="chat-copy"><div class="chat-author">${escapeHtml(message.sender_name)} · ${formatTime(message.timestamp, true)}</div><div class="chat-bubble">${escapeHtml(message.content || "[非文本消息]")}</div></div></article>`;
  }

  async pollMessages() {
    const id = this.selectedId;
    const messages = this.detailData?.messages?.items;
    const messagesTabActive = document.querySelector('#conversation-detail [data-tab="messages"]')?.classList.contains("active");
    if (!api.token || document.hidden || !id || !messages || this.pollingMessages || !document.getElementById("page-conversations").classList.contains("active") || !messagesTabActive) return;

    this.pollingMessages = true;
    try {
      const lastId = messages.length ? messages[messages.length - 1].id : 0;
      const response = await api.get(`/conversations/${id}/messages?after_id=${lastId}&limit=100`);
      if (this.selectedId !== id || !response.items.length) return;

      const panel = document.querySelector('#conversation-detail [data-panel="messages"]');
      const body = document.querySelector("#conversation-detail .detail-body");
      const shouldFollow = body && body.scrollHeight - body.scrollTop - body.clientHeight < 80;
      this.detailData.messages.items.push(...response.items);

      let list = panel?.querySelector(".chat-list");
      if (!list && panel) {
        panel.innerHTML = '<div class="chat-list"></div>';
        list = panel.firstElementChild;
      }
      if (list) list.insertAdjacentHTML("beforeend", response.items.map(message => this.messageHtml(message)).join(""));
      if (shouldFollow && body) body.scrollTop = body.scrollHeight;
      this.updateSelectedConversation(response.items[response.items.length - 1]);
    } catch (_) {
      // 短暂网络错误交给下一轮轮询恢复，鉴权失效仍由统一 API 客户端处理。
    } finally {
      this.pollingMessages = false;
    }
  }

  updateSelectedConversation(message) {
    const item = this.items.find(conversation => conversation.id === this.selectedId);
    if (!item) return;
    item.latest_sender_name = message.sender_name;
    item.latest_content = message.content;
    item.latest_message_at = message.timestamp;
    this.items = [item, ...this.items.filter(conversation => conversation !== item)];
    this.renderList();
  }

  userMemoriesHtml() {
    const data = this.detailData.users;
    if (data.error) return `<div class="panel-toolbar"><h3>已保存的用户记忆</h3></div><div class="empty"><strong>群成员读取失败</strong><span>${escapeHtml(data.error)}</span></div>`;
    const withMemories = data.users.filter(user => user.memories.length);
    return `<div class="panel-toolbar"><h3>已保存的用户记忆${data.stale ? "（使用缓存成员）" : ""}</h3><button class="button small" data-add-user-memory><i data-lucide="plus"></i>添加</button></div>
      <div class="memory-stack">${withMemories.length ? withMemories.map(user => `<div><div class="member-heading"><span class="avatar">${escapeHtml(initial(user.card || user.nickname))}</span><div><strong>${escapeHtml(user.card || user.nickname || user.user_id)}</strong><small>QQ ${escapeHtml(user.user_id)}</small></div></div>${user.memories.map(memory => this.entityHtml(memory.content, memory.id, "user", { userId: user.user_id })).join("")}</div>`).join("") : '<div class="empty">当前群成员没有已保存的用户记忆</div>'}</div>`;
  }

  characterMemoriesHtml() {
    const items = this.detailData.characters.items;
    return `<div class="panel-toolbar"><h3>当前会话角色记忆</h3><button class="button small" data-add-character><i data-lucide="plus"></i>添加</button></div><div class="memory-stack">${items.length ? items.map(memory => this.entityHtml(memory.content, memory.title, "character", { id: memory.id, retention: memory.remaining_days || 1, meta: memory.expiring ? "即将遗忘" : `剩余 ${memory.remaining_days} 天` })).join("") : '<div class="empty">当前没有角色记忆</div>'}</div>`;
  }

  tasksHtml() {
    const items = this.detailData.tasks.items;
    return `<div class="panel-toolbar"><h3>运行中的定时任务</h3><button class="button small" data-add-task><i data-lucide="plus"></i>添加</button></div><div class="task-stack">${items.length ? items.map(task => this.entityHtml(task.instruction, task.title, "task", { id: task.id, schedule: task.schedule, meta: `${task.schedule} · 下次 ${formatTime(task.next_run_at, true)}` })).join("") : '<div class="empty">当前没有定时任务</div>'}</div>`;
  }

  entityHtml(content, title, type, data = {}) {
    return `<article class="entity-item"><div><h4>${escapeHtml(title)}</h4>${data.meta ? `<small>${escapeHtml(data.meta)}</small>` : ""}<p>${escapeHtml(content)}</p></div><div class="entity-actions"><button data-edit="${type}" data-id="${escapeHtml(data.id ?? title)}" data-user-id="${escapeHtml(data.userId || "")}" title="编辑"><i data-lucide="pencil"></i></button><button data-delete="${type}" data-id="${escapeHtml(data.id ?? title)}" data-user-id="${escapeHtml(data.userId || "")}" title="删除"><i data-lucide="trash-2"></i></button></div></article>`;
  }

  bindDetailEvents() {
    const root = document.getElementById("conversation-detail");
    root.querySelector(".detail-tabs").addEventListener("click", event => {
      const button = event.target.closest("[data-tab]"); if (!button) return;
      root.querySelectorAll("[data-tab]").forEach(item => item.classList.toggle("active", item === button));
      root.querySelectorAll("[data-panel]").forEach(panel => panel.classList.toggle("active", panel.dataset.panel === button.dataset.tab));
    });
    root.addEventListener("click", event => this.handleDetailAction(event));
  }

  async handleDetailAction(event) {
    const addUser = event.target.closest("[data-add-user-memory]"); if (addUser) return this.editUserMemory();
    const addCharacter = event.target.closest("[data-add-character]"); if (addCharacter) return this.editCharacterMemory();
    const addTask = event.target.closest("[data-add-task]"); if (addTask) return this.editTask();
    const edit = event.target.closest("[data-edit]");
    if (edit) return this.editEntity(edit.dataset.edit, edit.dataset.id, edit.dataset.userId);
    const remove = event.target.closest("[data-delete]");
    if (remove) return this.deleteEntity(remove.dataset.delete, remove.dataset.id, remove.dataset.userId);
  }

  async editEntity(type, id, userId) {
    if (type === "user") {
      const user = this.detailData.users.users.find(item => item.user_id === userId); const memory = user.memories.find(item => item.id === id);
      return this.editUserMemory(user, memory);
    }
    if (type === "character") return this.editCharacterMemory(this.detailData.characters.items.find(item => String(item.id) === id));
    if (type === "task") return this.editTask(this.detailData.tasks.items.find(item => item.id === id));
  }

  async editUserMemory(user = null, memory = null) {
    const users = this.detailData.users.users;
    if (!users.length) return toast("当前没有可选择的会话成员", true);
    const values = await openDialog({ title: memory ? "修改用户记忆" : "添加用户记忆", eyebrow: "USER MEMORY", fields: [
      { name: "user_id", label: "用户", type: "select", value: user?.user_id || users[0].user_id, options: users.map(item => ({ value: item.user_id, label: `${item.card || item.nickname || item.user_id} (${item.user_id})` })) },
      { name: "content", label: "记忆内容", type: "textarea", value: memory?.content || "", rows: 5 },
    ]});
    if (!values) return;
    try {
      if (memory) await api.put(`/conversations/${this.selectedId}/users/${encodeURIComponent(values.user_id)}/memories/${encodeURIComponent(memory.id)}`, { content: values.content });
      else await api.post(`/conversations/${this.selectedId}/user-memories`, values);
      toast("用户记忆已保存"); await this.select(this.selectedId);
    } catch (error) { toast(error.message, true); }
  }

  async editCharacterMemory(memory = null) {
    const values = await openDialog({ title: memory ? "修改角色记忆" : "添加角色记忆", eyebrow: "CHARACTER MEMORY", fields: [
      { name: "title", label: "标题", value: memory?.title || "" },
      { name: "content", label: "内容", type: "textarea", value: memory?.content || "", rows: 6 },
      { name: "retention_days", label: "从现在起保留天数", type: "number", value: memory?.remaining_days || 30, min: 1, max: 365 },
    ]});
    if (!values) return;
    values.retention_days = Number(values.retention_days);
    try {
      if (memory) await api.put(`/conversations/${this.selectedId}/character-memories/${memory.id}`, values);
      else await api.post(`/conversations/${this.selectedId}/character-memories`, values);
      toast("角色记忆已保存"); await this.select(this.selectedId);
    } catch (error) { toast(error.message, true); }
  }

  async editTask(task = null) {
    const values = await openDialog({ title: task ? "修改定时任务" : "添加定时任务", eyebrow: "SCHEDULED TASK", fields: [
      { name: "title", label: "标题", value: task?.title || "" },
      { name: "schedule", label: "时间表达式", value: task?.schedule || "at:" },
      { name: "instruction", label: "任务说明", type: "textarea", value: task?.instruction || "", rows: 6 },
    ]});
    if (!values) return;
    try {
      if (task) await api.put(`/conversations/${this.selectedId}/scheduled-tasks/${encodeURIComponent(task.id)}`, values);
      else await api.post(`/conversations/${this.selectedId}/scheduled-tasks`, values);
      toast("定时任务已保存"); await this.select(this.selectedId);
    } catch (error) { toast(error.message, true); }
  }

  async deleteEntity(type, id, userId) {
    if (!window.confirm("确认删除这条内容？此操作无法撤销。")) return;
    try {
      if (type === "user") await api.delete(`/conversations/${this.selectedId}/users/${encodeURIComponent(userId)}/memories/${encodeURIComponent(id)}`);
      if (type === "character") await api.delete(`/conversations/${this.selectedId}/character-memories/${id}`);
      if (type === "task") await api.delete(`/conversations/${this.selectedId}/scheduled-tasks/${encodeURIComponent(id)}`);
      toast("已删除"); await this.select(this.selectedId);
    } catch (error) { toast(error.message, true); }
  }
}
