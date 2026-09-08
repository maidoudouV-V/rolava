import { api } from "./api.js";
import { escapeHtml, refreshIcons, toast } from "./ui.js";

export class SkillsController {
  constructor({ onRestart, onDirty }) {
    this.onRestart = onRestart;
    this.onDirty = onDirty;
    this.items = [];
    this.currentSkill = null;
    this.currentFile = null;
    this.originalContent = "";
    this.environmentItems = [];
    this.enabledDirty = false;
    this.environmentDirty = false;
    this.fileDirty = false;

    document.getElementById("refresh-skills").addEventListener("click", () => this.refresh(true));
    document.getElementById("save-enabled-skills").addEventListener("click", () => this.saveEnabled());
    document.getElementById("add-skill-environment").addEventListener("click", () => this.addEnvironment());
    document.getElementById("skill-list").addEventListener("click", event => {
      const button = event.target.closest("[data-skill-name]");
      if (button) this.selectSkill(button.dataset.skillName);
    });
    document.getElementById("skill-list").addEventListener("change", event => {
      const toggle = event.target.closest("[data-skill-toggle]");
      if (toggle) this.toggleSkill(toggle.dataset.skillToggle, toggle.checked);
    });
  }

  async load() {
    await this.refresh(false);
  }

  async refresh(confirmDiscard) {
    if (confirmDiscard && (this.enabledDirty || this.environmentDirty || this.fileDirty)
      && !window.confirm("刷新会放弃当前未保存的修改，确定继续吗？")) return;
    const button = document.getElementById("refresh-skills");
    button.disabled = true;
    try {
      const response = await api.get("/skills");
      this.items = response.items || [];
      this.environmentItems = (response.environment || []).map(item => ({ name: item.name, value: "", existing: true }));
      this.currentSkill = null;
      this.currentFile = null;
      this.originalContent = "";
      this.enabledDirty = false;
      this.environmentDirty = false;
      this.fileDirty = false;
      document.getElementById("save-enabled-skills").disabled = true;
      document.getElementById("skill-count").textContent = `${this.items.length} 个已发现`;
      this.renderList();
      this.renderEnvironment();
      document.getElementById("skill-detail").innerHTML = '<div class="empty detail-empty"><i data-lucide="folder-open"></i><strong>选择一个 Skill</strong><span>查看并编辑该目录中的文件</span></div>';
      refreshIcons();
    } catch (error) {
      toast(error.message, true);
    } finally {
      button.disabled = false;
    }
  }

  renderList() {
    const container = document.getElementById("skill-list");
    if (!this.items.length) {
      container.innerHTML = '<div class="empty"><i data-lucide="folder-search"></i><strong>没有发现 Skill</strong><span>在 skills 目录中加入包含 SKILL.md 的文件夹</span></div>';
      refreshIcons();
      return;
    }
    container.innerHTML = this.items.map(item => `
      <div class="skill-row${item.name === this.currentSkill ? " active" : ""}">
        <button class="skill-select" data-skill-name="${escapeHtml(item.name)}">
          <strong>${escapeHtml(item.name)}</strong>
          <small>${escapeHtml(item.description)}</small>
          <code>${escapeHtml(item.entry_path)}</code>
        </button>
        <label class="switch" aria-label="${item.enabled ? "关闭" : "启用"} ${escapeHtml(item.name)}">
          <input type="checkbox" data-skill-toggle="${escapeHtml(item.name)}" ${item.enabled ? "checked" : ""}><span></span>
        </label>
      </div>`).join("");
  }

  toggleSkill(name, enabled) {
    const item = this.items.find(skill => skill.name === name);
    if (!item || item.enabled === enabled) return;
    item.enabled = enabled;
    this.enabledDirty = true;
    this.updateSettingsButton();
    this.onDirty("Skill 启用状态尚未应用");
    this.renderList();
    const state = document.querySelector("#skill-detail .skill-state");
    if (name === this.currentSkill && state) {
      state.textContent = enabled ? "已启用" : "未启用";
      state.classList.toggle("enabled", enabled);
    }
    refreshIcons();
  }

  async saveEnabled() {
    const environment = this.environmentItems.map(item => ({
      name: item.name.trim(),
      value: item.value || null,
    }));
    if (environment.some(item => !item.name)) {
      toast("环境变量名称不能为空", true);
      return;
    }
    if (new Set(environment.map(item => item.name)).size !== environment.length) {
      toast("环境变量名称不能重复", true);
      return;
    }
    const button = document.getElementById("save-enabled-skills");
    button.disabled = true;
    try {
      await api.put("/skills/settings", {
        enabled: this.items.filter(item => item.enabled).map(item => item.name),
        environment,
      });
      this.enabledDirty = false;
      this.environmentDirty = false;
      this.onRestart();
    } catch (error) {
      button.disabled = false;
      toast(error.message, true);
    }
  }

  addEnvironment() {
    this.environmentItems.push({ name: "", value: "", existing: false });
    this.environmentDirty = true;
    this.onDirty("Skill 设置尚未应用");
    this.updateSettingsButton();
    this.renderEnvironment();
    document.querySelector("#skill-environment-list .skill-environment-row:last-child input")?.focus();
  }

  renderEnvironment() {
    const container = document.getElementById("skill-environment-list");
    if (!this.environmentItems.length) {
      container.innerHTML = '<div class="empty"><i data-lucide="key-round"></i><strong>没有环境变量</strong></div>';
      refreshIcons();
      return;
    }
    container.innerHTML = this.environmentItems.map((item, index) => `
      <div class="skill-environment-row">
        <label><span>变量名</span><input type="text" data-env-name="${index}" value="${escapeHtml(item.name)}" ${item.existing ? "readonly" : ""} autocomplete="off" placeholder="API_KEY"></label>
        <label><span>变量值</span><input type="password" data-env-value="${index}" value="${escapeHtml(item.value)}" autocomplete="new-password" placeholder="${item.existing ? "已配置，留空保持不变" : "输入变量值"}"></label>
        <button class="icon-button danger" data-env-delete="${index}" title="删除变量" aria-label="删除变量"><i data-lucide="trash-2"></i></button>
      </div>`).join("");
    container.querySelectorAll("[data-env-name]").forEach(input => input.addEventListener("input", () => {
      this.environmentItems[Number(input.dataset.envName)].name = input.value;
      this.markEnvironmentDirty();
    }));
    container.querySelectorAll("[data-env-value]").forEach(input => input.addEventListener("input", () => {
      this.environmentItems[Number(input.dataset.envValue)].value = input.value;
      this.markEnvironmentDirty();
    }));
    container.querySelectorAll("[data-env-delete]").forEach(button => button.addEventListener("click", () => {
      this.environmentItems.splice(Number(button.dataset.envDelete), 1);
      this.markEnvironmentDirty();
      this.renderEnvironment();
    }));
    refreshIcons();
  }

  markEnvironmentDirty() {
    this.environmentDirty = true;
    this.onDirty("Skill 环境变量尚未应用");
    this.updateSettingsButton();
  }

  updateSettingsButton() {
    document.getElementById("save-enabled-skills").disabled = !(this.enabledDirty || this.environmentDirty);
  }

  async selectSkill(name) {
    if (name === this.currentSkill) return;
    if (this.fileDirty && !window.confirm("当前文件尚未保存，确定切换 Skill 吗？")) return;
    const item = this.items.find(skill => skill.name === name);
    if (!item) return;
    this.currentSkill = name;
    this.currentFile = null;
    this.fileDirty = false;
    this.renderList();
    const detail = document.getElementById("skill-detail");
    detail.innerHTML = `
      <header><div><h2>${escapeHtml(item.name)}</h2><p>${escapeHtml(item.description)}</p></div><span class="skill-state${item.enabled ? " enabled" : ""}">${item.enabled ? "已启用" : "未启用"}</span></header>
      <div class="skill-editor-layout">
        <aside class="skill-files"><strong>目录文件</strong><div id="skill-file-list"><div class="empty">正在读取</div></div></aside>
        <section class="skill-file-editor">
          <div class="skill-file-toolbar"><code id="skill-file-name">选择文件</code><button class="button primary small" id="save-skill-file" disabled><i data-lucide="save"></i>保存文件</button></div>
          <label class="sr-only" for="skill-file-content">Skill 文件内容</label><textarea class="skill-file-content" id="skill-file-content" spellcheck="false" disabled></textarea>
        </section>
      </div>`;
    refreshIcons();
    document.getElementById("save-skill-file").addEventListener("click", () => this.saveFile());
    document.getElementById("skill-file-content").addEventListener("input", event => {
      this.fileDirty = event.target.value !== this.originalContent;
      document.getElementById("save-skill-file").disabled = !this.fileDirty;
    });
    try {
      const response = await api.get(`/skills/${encodeURIComponent(name)}/files`);
      this.renderFiles(response.items || []);
      const entry = (response.items || []).find(file => file.path === "SKILL.md");
      if (entry) await this.selectFile(entry.path);
    } catch (error) {
      document.getElementById("skill-file-list").innerHTML = `<div class="empty">${escapeHtml(error.message)}</div>`;
    }
  }

  renderFiles(files) {
    const container = document.getElementById("skill-file-list");
    if (!files.length) {
      container.innerHTML = '<div class="empty">该 Skill 没有可编辑文件</div>';
      return;
    }
    container.innerHTML = files.map(file => `<button class="skill-file-button${file.path === this.currentFile ? " active" : ""}" data-skill-file="${escapeHtml(file.path)}">${escapeHtml(file.path)}<small>${formatBytes(file.size)}</small></button>`).join("");
    container.querySelectorAll("[data-skill-file]").forEach(button => {
      button.addEventListener("click", () => this.selectFile(button.dataset.skillFile));
    });
  }

  async selectFile(path) {
    if (path === this.currentFile) return;
    if (this.fileDirty && !window.confirm("当前文件尚未保存，确定切换文件吗？")) return;
    try {
      const response = await api.get(`/skills/${encodeURIComponent(this.currentSkill)}/file?path=${encodeURIComponent(path)}`);
      this.currentFile = response.path;
      this.originalContent = response.content;
      this.fileDirty = false;
      const editor = document.getElementById("skill-file-content");
      editor.value = response.content;
      editor.disabled = false;
      document.getElementById("skill-file-name").textContent = response.path;
      document.getElementById("save-skill-file").disabled = true;
      document.querySelectorAll("[data-skill-file]").forEach(button => button.classList.toggle("active", button.dataset.skillFile === response.path));
    } catch (error) {
      toast(error.message, true);
    }
  }

  async saveFile() {
    if (!this.currentSkill || !this.currentFile) return;
    const button = document.getElementById("save-skill-file");
    const editor = document.getElementById("skill-file-content");
    button.disabled = true;
    try {
      await api.put(`/skills/${encodeURIComponent(this.currentSkill)}/file?path=${encodeURIComponent(this.currentFile)}`, { content: editor.value });
      this.originalContent = editor.value;
      this.fileDirty = false;
      toast(this.currentFile === "SKILL.md" ? "入口文件已保存，刷新列表可更新元数据" : "Skill 文件已保存");
    } catch (error) {
      button.disabled = false;
      toast(error.message, true);
    }
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}
