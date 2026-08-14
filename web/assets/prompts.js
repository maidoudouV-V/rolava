import { api } from "./api.js";
import { escapeHtml, refreshIcons, toast } from "./ui.js";

export class PromptController {
  constructor({ onRestart }) {
    this.onRestart = onRestart;
    this.current = null;
    document.getElementById("prompt-list").addEventListener("click", event => {
      const button = event.target.closest("[data-prompt-id]");
      if (button) this.select(button.dataset.promptId);
    });
    document.getElementById("save-prompt").addEventListener("click", () => this.save());
  }

  async load() {
    const response = await api.get("/prompts");
    const items = response.items.filter(item => item.category === "core");
    document.getElementById("prompt-list").innerHTML = `
      <p class="prompt-category">核心提示词</p>
      ${items.map(item => `<button class="prompt-button" data-prompt-id="${escapeHtml(item.id)}">${escapeHtml(item.name)}</button>`).join("")}
    `;
    const first = items[0];
    if (first) await this.select(first.id);
  }

  async select(promptId) {
    try {
      const response = await api.get(`/prompts/${encodeURIComponent(promptId)}`);
      this.current = promptId;
      document.querySelectorAll(".prompt-button").forEach(button => button.classList.toggle("active", button.dataset.promptId === promptId));
      document.getElementById("prompt-title").textContent = document.querySelector(`[data-prompt-id="${CSS.escape(promptId)}"]`)?.textContent || promptId;
      document.getElementById("prompt-meta").textContent = "核心提示词";
      const editor = document.getElementById("prompt-content");
      editor.value = response.content;
      editor.disabled = false;
      document.getElementById("save-prompt").disabled = false;
    } catch (error) { toast(error.message, true); }
  }

  async save() {
    if (!this.current) return;
    const button = document.getElementById("save-prompt");
    button.disabled = true;
    try {
      await api.put(`/prompts/${encodeURIComponent(this.current)}`, { content: document.getElementById("prompt-content").value });
      this.onRestart();
    } catch (error) {
      button.disabled = false;
      toast(error.message, true);
    }
    refreshIcons();
  }
}
