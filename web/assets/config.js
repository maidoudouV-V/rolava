import { api } from "./api.js";
import { escapeHtml, lines, refreshIcons, toast } from "./ui.js";

export class ConfigController {
  constructor({ onRestart, onDirty }) {
    this.onRestart = onRestart;
    this.onDirty = onDirty;
    this.data = null;
    this.catalog = [];
    this.visibleCatalog = [];
    this.catalogModelIndex = null;
    this.toolIcons = { agent_web_search: "globe-2", memory: "brain", scheduled_tasks: "calendar-clock" };
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
    document.getElementById("max-history").value = app.max_history_messages;
    document.getElementById("image-window").value = app.vision_image_message_window;
    document.getElementById("retry-count").value = app.ai_request_retry_count;
    document.getElementById("request-timeout").value = app.ai_request_timeout_seconds;
    document.getElementById("reply-delay").value = app.reply_delay_random_max_secs;
    document.getElementById("direct-whitelist").value = app.direct_whitelist.join("\n");
    document.getElementById("group-whitelist").value = app.group_whitelist.join("\n");
    document.getElementById("command-whitelist").value = app.command_whitelist.join("\n");
    this.renderProviders();
    this.renderModels();
    this.renderAssignments();
    const enabledTools = new Set(app.enabled_actions);
    document.getElementById("tool-list").innerHTML = this.optionalTools.map(tool => `<div class="optional-tool-row">
      <div class="optional-tool-copy"><span class="optional-tool-icon"><i data-lucide="${this.toolIcons[tool.name] || "wrench"}"></i></span><div><strong>${escapeHtml(tool.display_name)}</strong><p>${escapeHtml(tool.description)}</p></div></div>
      <label class="switch" aria-label="启用${escapeHtml(tool.display_name)}"><input type="checkbox" data-optional-tool="${escapeHtml(tool.name)}" ${enabledTools.has(tool.name) ? "checked" : ""}><span></span></label>
    </div>`).join("") || '<div class="empty">暂无可选工具</div>';
    refreshIcons();
  }

  renderProviders() {
    document.getElementById("provider-rows").innerHTML = this.data.providers.map((provider, index) => `<tr data-index="${index}">
      <td><input data-field="name" value="${escapeHtml(provider.name)}"></td>
      <td><select data-field="type">${["openai_compatible","openrouter","google_aistudio"].map(type => `<option ${type === provider.type ? "selected" : ""}>${type}</option>`).join("")}</select></td>
      <td><input data-field="base_url" value="${escapeHtml(provider.base_url)}"></td>
      <td><input data-field="key" type="password" placeholder="${provider.key_configured ? "已配置" : "未配置"}"></td>
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
      <td class="row-actions"><button data-test-model="${index}" title="测试"><i data-lucide="flask-conical"></i></button><button data-remove-model="${index}" title="删除"><i data-lucide="trash-2"></i></button></td>
    </tr>`;
    }).join("");
    refreshIcons();
  }

  reasoningOptions(providerType, selected) {
    const automatic = providerType === "google_aistudio" ? "自动（Google 默认）" : providerType === "openrouter" ? "自动（路由默认）" : "自动（模型默认）";
    const options = [["auto", automatic], ["none", "none"], ["minimal", "minimal"], ["low", "low"], ["medium", "medium"], ["high", "high"], ["xhigh", "xhigh"]];
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
    this.data.models.push({ name: `model-${this.data.models.length + 1}`, provider: this.data.providers[0]?.name || "", model: "", max_tokens: null, reasoning_effort: "auto", vision: "disable" });
    this.renderModels(); this.renderAssignments(); this.onDirty();
  }

  handleProviderAction(event) {
    const button = event.target.closest("[data-remove-provider]"); if (!button) return;
    this.syncRows(); this.data.providers.splice(Number(button.dataset.removeProvider), 1); this.renderProviders(); this.renderModels(); this.onDirty();
  }

  async handleModelAction(event) {
    const remove = event.target.closest("[data-remove-model]");
    if (remove) { this.syncRows(); this.data.models.splice(Number(remove.dataset.removeModel), 1); this.renderModels(); this.renderAssignments(); this.onDirty(); return; }
    const test = event.target.closest("[data-test-model]");
    if (test) {
      this.syncRows();
      const model = this.data.models[Number(test.dataset.testModel)];
      const provider = this.data.providers.find(item => item.name === model.provider);
      if (!provider) return toast("模型引用的 Provider 不存在", true);
      try { toast(`正在测试 ${model.name}`); await api.post("/test/model", { provider, model }); toast(`${model.name} 连接正常`); }
      catch (error) { toast(error.message, true); }
    }
    const picker = event.target.closest("[data-pick-model]");
    if (picker) await this.openModelPicker(Number(picker.dataset.pickModel));
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
    const app = this.data.app;
    Object.assign(app, {
      chat_model_name: document.getElementById("chat-model").value,
      filter_model_name: document.getElementById("filter-model").value,
      web_search_model_name: document.getElementById("web-model").value,
      visual_model_name: document.getElementById("visual-model").value,
      max_history_messages: Number(document.getElementById("max-history").value),
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
