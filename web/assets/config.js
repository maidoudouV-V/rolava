import { api } from "./api.js";
import { escapeHtml, lines, refreshIcons, toast } from "./ui.js";

const JSON_NUMBER = /^-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?$/;

export function parseExtraFields(text) {
  if (!text?.trim()) return [];
  const fields = JSON.parse(text);
  if (!fields || Array.isArray(fields) || typeof fields !== "object") throw new Error("请求附加字段必须是 JSON 对象");
  return Object.entries(fields).map(([key, value]) => {
    if (typeof value !== "string" && typeof value !== "number") throw new Error(`附加字段 ${key} 的值只能是字符串或数字`);
    return { key, value: String(value) };
  });
}

export function serializeExtraFields(entries, modelName = "模型") {
  const fields = Object.create(null);
  for (const entry of entries) {
    const key = entry.key.trim();
    if (!key) throw new Error(`${modelName} 的附加字段名不能为空`);
    if (Object.hasOwn(fields, key)) throw new Error(`${modelName} 的附加字段名重复：${key}`);
    const value = entry.value.trim();
    if (JSON_NUMBER.test(value)) {
      const number = Number(value);
      if (!Number.isFinite(number) || (Number.isInteger(number) && !Number.isSafeInteger(number))) throw new Error(`${modelName} 的附加字段 ${key} 数字超出精确范围`);
      fields[key] = number;
    } else {
      fields[key] = entry.value;
    }
  }
  return entries.length ? JSON.stringify(fields) : "";
}

export class ConfigController {
  constructor({ onRestart, onDirty }) {
    this.onRestart = onRestart;
    this.onDirty = onDirty;
    this.data = null;
    this.catalog = [];
    this.visibleCatalog = [];
    this.catalogModelIndex = null;
    this.extraFieldEditIndex = null;
    this.toolIcons = { agent_web_search: "globe-2", memory: "brain" };
    document.querySelectorAll(".config-page input,.config-page select,.config-page textarea").forEach(element => element.addEventListener("input", onDirty));
    document.getElementById("add-provider").addEventListener("click", () => this.addProvider());
    document.getElementById("add-model").addEventListener("click", () => this.addModel());
    document.getElementById("provider-rows").addEventListener("click", event => this.handleProviderAction(event));
    document.getElementById("model-rows").addEventListener("click", event => this.handleModelAction(event));
    document.getElementById("provider-rows").addEventListener("change", event => this.handleRowChange(event, "provider"));
    document.getElementById("model-rows").addEventListener("change", event => this.handleRowChange(event, "model"));
    document.getElementById("provider-rows").addEventListener("input", onDirty);
    document.getElementById("model-rows").addEventListener("input", onDirty);
    document.getElementById("tool-list").addEventListener("input", event => {
      if (event.target.matches("[data-optional-tool]")) onDirty();
    });
    document.getElementById("test-onebot").addEventListener("click", () => this.testOneBot());
    document.getElementById("model-picker-query").addEventListener("input", () => this.renderModelCatalog());
    document.getElementById("model-picker-results").addEventListener("click", event => this.selectCatalogModel(event));
    document.getElementById("close-model-picker").addEventListener("click", () => document.getElementById("model-picker").close());
    const extraDialog = document.getElementById("extra-fields-dialog");
    extraDialog.addEventListener("click", event => this.handleExtraFieldAction(event));
    extraDialog.addEventListener("close", () => { this.extraFieldEditIndex = null; });
  }

  async load() {
    const response = await api.get("/config");
    this.data = response.config;
    this.optionalTools = response.optional_tools;
    this.fill();
  }

  fill() {
    const { app, server } = this.data;
    document.getElementById("server-host").value = server.server_host;
    document.getElementById("server-port").value = server.server_port;
    document.getElementById("server-token").placeholder = server.server_token_configured ? "已配置，留空保持不变" : "未配置";
    document.getElementById("onebot-api").value = server.onebot_api;
    document.getElementById("onebot-token").placeholder = server.onebot_token_configured ? "已配置，留空保持不变" : "未配置";
    document.getElementById("group-max-history").value = app.group_max_history_messages;
    document.getElementById("direct-max-history").value = app.direct_max_history_messages;
    document.getElementById("history-summary-enabled").checked = app.history_summary_enabled;
    document.getElementById("history-summary-days").value = app.history_summary_days;
    document.getElementById("startup-history-fetch-count").value = app.startup_history_fetch_count;
    document.getElementById("image-window").value = app.vision_image_message_window;
    document.getElementById("retry-count").value = app.ai_request_retry_count;
    document.getElementById("request-timeout").value = app.ai_request_timeout_seconds;
    document.getElementById("reply-delay").value = app.reply_delay_random_max_secs;
    document.getElementById("direct-whitelist").value = app.direct_whitelist.join("\n");
    document.getElementById("group-whitelist").value = app.group_whitelist.join("\n");
    document.getElementById("command-whitelist").value = app.command_whitelist.join("\n");
    this.extraFieldDrafts = this.data.models.map(model => parseExtraFields(model.extra_body));
    this.renderProviders();
    this.renderModels();
    this.renderAssignments();
    const enabledTools = new Set(app.enabled_actions);
    document.getElementById("tool-list").innerHTML = this.optionalTools.map(tool => `<div class="optional-tool-row">
      <div class="optional-tool-copy"><span class="optional-tool-icon"><i data-lucide="${this.toolIcons[tool.name] || "wrench"}"></i></span><div><strong>${escapeHtml(tool.display_name)}</strong><p>${escapeHtml(tool.description)}</p></div></div>
      <label class="switch" aria-label="启用${escapeHtml(tool.display_name)}"><input type="checkbox" data-optional-tool="${escapeHtml(tool.name)}" ${enabledTools.has(tool.name) ? "checked" : ""}><span></span></label>
    </div>`).join("") || '<div class="empty">暂无可选模块</div>';
    refreshIcons();
  }

  renderProviders() {
    document.getElementById("provider-rows").innerHTML = this.data.providers.map((provider, index) => `<tr data-index="${index}">
      <td><input data-field="name" value="${escapeHtml(provider.name)}"></td>
      <td><select data-field="type">${["openai_compatible","openai_responses","openrouter","gemini"].map(type => `<option value="${type}" ${type === provider.type ? "selected" : ""}>${type === "gemini" ? "Gemini" : type}</option>`).join("")}</select></td>
      <td><input data-field="base_url" value="${escapeHtml(provider.base_url)}"></td>
      <td><input data-field="key" type="password" value="${escapeHtml(provider.key || "")}" placeholder="${provider.key_configured ? "已配置" : "未配置"}"></td>
      <td class="row-actions"><button data-remove-provider="${index}" title="删除"><i data-lucide="trash-2"></i></button></td>
    </tr>`).join("");
  }

  renderModels() {
    const providers = this.data.providers.map(provider => provider.name);
    document.getElementById("model-rows").innerHTML = this.data.models.map((model, index) => {
      const providerType = this.data.providers.find(provider => provider.name === model.provider)?.type;
      return `<tr data-index="${index}">
      <td><input data-field="name" value="${escapeHtml(model.name)}"></td>
      <td><div class="model-id-control"><input data-field="model" value="${escapeHtml(model.model)}" placeholder="选择或输入模型 ID"><button data-pick-model="${index}" title="从 Provider 获取模型"><i data-lucide="chevrons-up-down"></i></button></div></td>
      <td><select data-field="provider">${providers.map(name => `<option ${name === model.provider ? "selected" : ""}>${escapeHtml(name)}</option>`).join("")}</select></td>
      <td><input data-field="max_tokens" type="number" min="1" value="${model.max_tokens ?? ""}" placeholder="默认"></td>
      <td><select data-field="reasoning_effort">${this.reasoningOptions(providerType, model.reasoning_effort)}</select></td>
      <td><select data-field="vision"><option value="disable" ${model.vision === "disable" ? "selected" : ""}>禁用</option><option value="enable" ${model.vision === "enable" ? "selected" : ""}>启用</option></select></td>
      <td><button type="button" class="button small" data-edit-extra="${index}">设置${this.extraFieldDrafts[index].length ? `（${this.extraFieldDrafts[index].length}）` : ""}</button></td>
      <td class="row-actions"><button data-test-model="${index}" title="测试"><i data-lucide="flask-conical"></i></button><button data-remove-model="${index}" title="删除"><i data-lucide="trash-2"></i></button></td>
    </tr>`;
    }).join("");
    refreshIcons();
  }

  reasoningOptions(providerType, selected) {
    const automatic = providerType === "gemini" ? "自动（Gemini 默认）" : providerType === "openrouter" ? "自动（路由默认）" : "自动（模型默认）";
    const levels = ["none", "minimal", "low", "medium", "high", "xhigh"];
    const options = [["auto", automatic], ...levels.map(level => [level, level])];
    if (!options.some(([value]) => value === selected)) selected = "auto";
    return options.map(([value, label]) => `<option value="${value}" ${value === (selected || "auto") ? "selected" : ""}>${label}</option>`).join("");
  }

  renderAssignments() {
    const options = this.data.models.map(model => `<option value="${escapeHtml(model.name)}">${escapeHtml(model.name)}</option>`).join("");
    for (const [id, key, optional] of [["chat-model","chat_model_name"],["filter-model","filter_model_name",true],["web-model","web_search_model_name",true],["visual-model","visual_model_name",true]]) {
      const element = document.getElementById(id);
      element.innerHTML = `${optional ? '<option value="">关闭</option>' : ""}${options}`;
      element.value = this.data.app[key];
    }
  }

  syncRows() {
    document.querySelectorAll("#provider-rows tr").forEach((row, index) => {
      for (const input of row.querySelectorAll("[data-field]")) {
        this.data.providers[index][input.dataset.field] = input.value;
      }
    });
    document.querySelectorAll("#model-rows tr").forEach((row, index) => {
      for (const input of row.querySelectorAll("[data-field]")) {
        this.data.models[index][input.dataset.field] = input.dataset.field === "max_tokens" ? (input.value.trim() ? Number(input.value) : null) : input.value;
      }
    });
  }

  addProvider() {
    this.syncRows();
    this.data.providers.push({ name: `Provider${this.data.providers.length + 1}`, original_name: null, type: "openai_compatible", base_url: "https://", key_configured: false, key: null });
    this.renderProviders(); this.renderModels(); this.onDirty();
  }

  addModel() {
    this.syncRows();
    this.data.models.push({ name: `model-${this.data.models.length + 1}`, provider: this.data.providers[0]?.name || "", model: "", max_tokens: null, reasoning_effort: "auto", vision: "disable", extra_body: "" });
    this.extraFieldDrafts.push([]);
    this.renderModels(); this.renderAssignments(); this.onDirty();
  }

  handleProviderAction(event) {
    const button = event.target.closest("[data-remove-provider]"); if (!button) return;
    this.syncRows(); this.data.providers.splice(Number(button.dataset.removeProvider), 1); this.renderProviders(); this.renderModels(); this.onDirty();
  }

  async handleModelAction(event) {
    const editExtra = event.target.closest("[data-edit-extra]");
    if (editExtra) { this.openExtraFieldDialog(Number(editExtra.dataset.editExtra)); return; }
    const remove = event.target.closest("[data-remove-model]");
    if (remove) { this.syncRows(); const index = Number(remove.dataset.removeModel); this.data.models.splice(index, 1); this.extraFieldDrafts.splice(index, 1); this.renderModels(); this.renderAssignments(); this.onDirty(); return; }
    const test = event.target.closest("[data-test-model]");
    if (test) {
      this.syncRows();
      const index = Number(test.dataset.testModel);
      const model = this.data.models[index];
      const provider = this.data.providers.find(item => item.name === model.provider);
      if (!provider) return toast("模型引用的 Provider 不存在", true);
      try { model.extra_body = serializeExtraFields(this.extraFieldDrafts[index], model.name); toast(`正在测试 ${model.name}`); await api.post("/test/model", { provider, model }); toast(`${model.name} 连接正常`); }
      catch (error) { toast(error.message, true); }
    }
    const picker = event.target.closest("[data-pick-model]");
    if (picker) await this.openModelPicker(Number(picker.dataset.pickModel));
  }

  openExtraFieldDialog(index) {
    this.syncRows();
    this.extraFieldEditIndex = index;
    document.getElementById("extra-fields-model-name").textContent = this.data.models[index].name;
    document.getElementById("extra-fields-error").hidden = true;
    this.renderExtraFieldDialog(this.extraFieldDrafts[index]);
    document.getElementById("extra-fields-dialog").showModal();
  }

  readExtraFieldDialog() {
    return [...document.querySelectorAll("#extra-field-rows [data-extra-entry]")].map(entry => ({
      key: entry.querySelector("[data-extra-key]").value,
      value: entry.querySelector("[data-extra-value]").value,
    }));
  }

  renderExtraFieldDialog(entries) {
    document.getElementById("extra-field-rows").innerHTML = entries.map((entry, index) => `<div class="extra-field-row" data-extra-entry>
      <input data-extra-key aria-label="字段名" value="${escapeHtml(entry.key)}" placeholder="字段名">
      <input data-extra-value aria-label="值" value="${escapeHtml(entry.value)}" placeholder="值">
      <button type="button" data-remove-extra="${index}" title="删除字段"><i data-lucide="x"></i></button>
    </div>`).join("");
    refreshIcons();
  }

  handleExtraFieldAction(event) {
    const dialog = document.getElementById("extra-fields-dialog");
    if (event.target.closest("[data-cancel-extra]")) { dialog.close(); return; }
    const add = event.target.closest("[data-add-extra]");
    if (add) {
      const entries = this.readExtraFieldDialog();
      entries.push({ key: "", value: "" });
      this.renderExtraFieldDialog(entries);
      document.querySelector("#extra-field-rows [data-extra-entry]:last-child [data-extra-key]").focus();
      return;
    }
    const remove = event.target.closest("[data-remove-extra]");
    if (remove) {
      const entries = this.readExtraFieldDialog();
      entries.splice(Number(remove.dataset.removeExtra), 1);
      this.renderExtraFieldDialog(entries);
      return;
    }
    if (event.target.closest("[data-confirm-extra]")) {
      const index = this.extraFieldEditIndex;
      if (index === null) return;
      const entries = this.readExtraFieldDialog();
      try {
        const serialized = serializeExtraFields(entries, this.data.models[index].name);
        this.extraFieldDrafts[index] = entries;
        this.data.models[index].extra_body = serialized;
        this.renderModels();
        this.onDirty();
        dialog.close();
      } catch (error) {
        const message = document.getElementById("extra-fields-error");
        message.textContent = error.message;
        message.hidden = false;
      }
    }
  }

  async openModelPicker(index) {
    this.syncRows();
    const model = this.data.models[index];
    const provider = this.data.providers.find(item => item.name === model.provider);
    if (!provider) return toast("请先为模型选择 Provider", true);
    this.catalogModelIndex = index;
    this.catalog = [];
    document.getElementById("model-picker-query").value = "";
    document.getElementById("model-picker-status").textContent = `正在从 ${provider.name} 获取模型列表`;
    document.getElementById("model-picker-results").innerHTML = '<div class="empty"><span class="spinner small-spinner"></span><span>正在读取模型</span></div>';
    const dialog = document.getElementById("model-picker");
    dialog.showModal();
    refreshIcons();
    try {
      const response = await api.post("/providers/models", { provider });
      this.catalog = response.items || [];
      this.renderModelCatalog();
      document.getElementById("model-picker-query").focus();
    } catch (error) {
      document.getElementById("model-picker-status").textContent = "模型目录读取失败";
      document.getElementById("model-picker-results").innerHTML = `<div class="empty"><i data-lucide="circle-alert"></i><strong>无法获取模型列表</strong><span>${escapeHtml(error.message)}</span></div>`;
      refreshIcons();
    }
  }

  renderModelCatalog() {
    const query = document.getElementById("model-picker-query").value.trim().toLocaleLowerCase();
    const filtered = query ? this.catalog.filter(item => `${item.name}\n${item.id}`.toLocaleLowerCase().includes(query)) : this.catalog;
    this.visibleCatalog = filtered.slice(0, 100);
    const status = document.getElementById("model-picker-status");
    status.textContent = filtered.length > 100 ? `找到 ${filtered.length} 个模型，仅渲染前 100 个，请继续输入筛选` : `找到 ${filtered.length} 个模型`;
    document.getElementById("model-picker-results").innerHTML = this.visibleCatalog.length ? this.visibleCatalog.map((item, index) => `<button class="model-catalog-item" data-catalog-index="${index}">
      <span><strong>${escapeHtml(item.name)}</strong><code>${escapeHtml(item.id)}</code></span>
      <small class="${item.vision === true ? "vision" : item.vision === false ? "text-only" : "unknown"}">${item.vision === true ? "支持图像" : item.vision === false ? "仅文本" : "能力未知"}</small>
    </button>`).join("") : '<div class="empty"><i data-lucide="search-x"></i><strong>没有匹配模型</strong><span>可以缩短关键词后重试</span></div>';
    refreshIcons();
  }

  selectCatalogModel(event) {
    const button = event.target.closest("[data-catalog-index]");
    if (!button || this.catalogModelIndex === null) return;
    const item = this.visibleCatalog[Number(button.dataset.catalogIndex)];
    const model = this.data.models[this.catalogModelIndex];
    if (!item || !model) return;
    const oldName = model.name;
    model.model = item.id;
    model.name = this.uniqueModelName(item.name || item.id, this.catalogModelIndex);
    if (item.vision !== null && item.vision !== undefined) model.vision = item.vision ? "enable" : "disable";
    for (const key of ["chat_model_name", "filter_model_name", "web_search_model_name", "visual_model_name"]) {
      if (this.data.app[key] === oldName) this.data.app[key] = model.name;
    }
    document.getElementById("model-picker").close();
    this.renderModels();
    this.renderAssignments();
    this.onDirty();
  }

  uniqueModelName(candidate, currentIndex) {
    const base = candidate.trim() || `model-${currentIndex + 1}`;
    const names = new Set(this.data.models.filter((_, index) => index !== currentIndex).map(model => model.name));
    if (!names.has(base)) return base;
    for (let suffix = 2; ; suffix += 1) if (!names.has(`${base}-${suffix}`)) return `${base}-${suffix}`;
  }

  handleRowChange(event, type) {
    const input = event.target.closest("[data-field]"); if (!input) return;
    const index = Number(input.closest("tr").dataset.index);
    if (type === "provider" && input.dataset.field === "name") {
      const oldName = this.data.providers[index].name; const newName = input.value;
      this.syncRows();
      this.data.models.forEach(model => { if (model.provider === oldName) model.provider = newName; });
      this.renderModels();
    } else if (type === "provider" && input.dataset.field === "type") {
      this.syncRows();
      this.renderModels();
    } else if (type === "model" && input.dataset.field === "name") {
      const oldName = this.data.models[index].name; const newName = input.value;
      this.syncRows();
      for (const key of ["chat_model_name", "filter_model_name", "web_search_model_name", "visual_model_name"]) {
        if (this.data.app[key] === oldName) this.data.app[key] = newName;
      }
      this.renderAssignments();
    } else if (type === "model" && input.dataset.field === "provider") {
      this.syncRows();
      this.renderModels();
    }
    this.onDirty();
  }

  buildUpdate() {
    this.syncRows();
    this.data.models.forEach((model, index) => { model.extra_body = serializeExtraFields(this.extraFieldDrafts[index], model.name); });
    const app = this.data.app;
    Object.assign(app, {
      chat_model_name: document.getElementById("chat-model").value,
      filter_model_name: document.getElementById("filter-model").value,
      web_search_model_name: document.getElementById("web-model").value,
      visual_model_name: document.getElementById("visual-model").value,
      group_max_history_messages: Number(document.getElementById("group-max-history").value),
      direct_max_history_messages: Number(document.getElementById("direct-max-history").value),
      history_summary_enabled: document.getElementById("history-summary-enabled").checked,
      history_summary_days: Number(document.getElementById("history-summary-days").value),
      startup_history_fetch_count: Number(document.getElementById("startup-history-fetch-count").value),
      vision_image_message_window: Number(document.getElementById("image-window").value),
      ai_request_retry_count: Number(document.getElementById("retry-count").value),
      ai_request_timeout_seconds: Number(document.getElementById("request-timeout").value),
      reply_delay_random_max_secs: Number(document.getElementById("reply-delay").value),
      direct_whitelist: lines(document.getElementById("direct-whitelist").value),
      group_whitelist: lines(document.getElementById("group-whitelist").value),
      command_whitelist: lines(document.getElementById("command-whitelist").value),
      enabled_actions: [...document.querySelectorAll("[data-optional-tool]:checked")].map(input => input.dataset.optionalTool),
    });
    const server = { ...this.data.server,
      server_host: document.getElementById("server-host").value.trim(),
      server_port: Number(document.getElementById("server-port").value),
      server_token: document.getElementById("server-token").value || null,
      onebot_api: document.getElementById("onebot-api").value.trim(),
      onebot_token: document.getElementById("onebot-token").value || null,
    };
    return { app, server, providers: this.data.providers.map(provider => ({ ...provider, key: provider.key || null })), models: this.data.models };
  }

  async save(restartAfterSave) {
    const update = this.buildUpdate();
    await api.put(`/config?restart=${restartAfterSave}`, update);
    if (restartAfterSave) this.onRestart();
    else await this.load();
  }

  async testOneBot() {
    try {
      const result = await api.post("/test/onebot", {
        onebot_api: document.getElementById("onebot-api").value.trim(),
        onebot_token: document.getElementById("onebot-token").value || null,
      });
      toast(`OneBot 连接正常，Bot QQ ${result.bot_id}`);
    } catch (error) { toast(error.message, true); }
  }
}
