import { api } from "./api.js";
import { escapeHtml, toast } from "./ui.js";

const numberFormat = new Intl.NumberFormat("zh-CN");
const currencyFormat = new Intl.NumberFormat("en-US", {
  style: "currency", currency: "USD", minimumFractionDigits: 6, maximumFractionDigits: 6,
});
const costText = value => value == null ? "未知"
  : value > 0 && value < 0.000001 ? "< $0.000001" : currencyFormat.format(value);
const timeFormat = new Intl.DateTimeFormat("zh-CN", {
  year: "numeric", month: "2-digit", day: "2-digit",
  hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false,
});
const tokenText = value => value == null ? "—" : numberFormat.format(value);
const rateText = value => value == null ? "—"
  : value > 0 && value < 0.0001 ? "<0.01%" : `${(value * 100).toFixed(2)}%`;
const purposeNames = {
  chat: "主模型", filter: "前置过滤", vision: "图片理解", web_search: "联网搜索",
  history_summary: "历史摘要", memory_review: "记忆整理", model_test: "模型测试",
};
const responseKindNames = { text: "文本", tool: "工具", text_tool: "文本 / 工具", empty: "无正文或工具" };
const providerNames = { openrouter: "OpenRouter", gemini: "Gemini", openai_compatible: "OpenAI Compatible" };
const outcomeNames = { success: "成功", failed: "失败", interrupted: "中断" };

function dateInput(timestamp) {
  const date = new Date(timestamp * 1000);
  const pad = value => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export class StatisticsController {
  constructor() {
    this.root = document.getElementById("page-statistics");
    this.start = document.getElementById("statistics-start");
    this.end = document.getElementById("statistics-end");
    this.purpose = document.getElementById("statistics-purpose");
    this.rows = document.getElementById("statistics-rows");
    this.hours = 24;
    this.range = null;
    this.cursors = [null];
    this.page = 0;
    this.loading = false;
    this.nextCursor = null;
    this.root.querySelector("form").addEventListener("submit", event => {
      event.preventDefault();
      this.applyRange();
    });
    this.purpose.addEventListener("change", () => this.applyRange());
    this.root.querySelectorAll("[data-statistics-hours]").forEach(button => {
      button.addEventListener("click", () => {
        this.hours = Number(button.dataset.statisticsHours);
        this.setRelativeRange();
        this.applyRange();
      });
    });
    for (const input of [this.start, this.end]) {
      input.addEventListener("input", () => { this.hours = null; this.updatePresets(); });
    }
    document.getElementById("statistics-refresh").addEventListener("click", () => this.open());
    document.getElementById("statistics-previous").addEventListener("click", () => this.changePage(this.page - 1));
    document.getElementById("statistics-next").addEventListener("click", () => {
      if (this.nextCursor) {
        this.cursors[this.page + 1] = this.nextCursor;
        this.changePage(this.page + 1);
      }
    });
    this.setRelativeRange();
  }

  setRelativeRange() {
    if (this.hours == null) return;
    const end = Math.floor(Date.now() / 1000) + 1;
    this.start.value = dateInput(end - this.hours * 3600);
    this.end.value = dateInput(end);
    this.updatePresets();
  }

  updatePresets() {
    this.root.querySelectorAll("[data-statistics-hours]").forEach(button => {
      const active = Number(button.dataset.statisticsHours) === this.hours;
      button.classList.toggle("active", active);
      button.setAttribute("aria-pressed", String(active));
    });
  }

  async open() {
    if (this.loading) return;
    this.setRelativeRange();
    await this.applyRange();
  }

  async applyRange() {
    if (this.loading) return;
    const start = Math.floor(new Date(this.start.value).getTime() / 1000);
    const end = Math.floor(new Date(this.end.value).getTime() / 1000);
    if (!Number.isFinite(start) || !Number.isFinite(end) || start < 0 || end <= start) {
      toast("请选择有效的开始与结束时间，开始时间必须早于结束时间", true);
      return;
    }
    this.range = { start, end, purpose: this.purpose.value };
    this.cursors = [null];
    this.page = 0;
    this.nextCursor = null;
    for (const id of ["requests", "input", "output", "total", "cache-rate", "cost"]) {
      document.getElementById(`statistics-${id}`).textContent = "—";
    }
    document.getElementById("statistics-request-detail").textContent = "正在汇总";
    document.getElementById("statistics-cost-detail").textContent = "正在汇总";
    document.getElementById("statistics-coverage").textContent = "正在读取用量数据";
    this.rows.innerHTML = '<tr><td colspan="12" class="empty">正在加载请求记录</td></tr>';
    await this.load(true);
  }

  async changePage(page) {
    if (this.loading || page < 0 || page >= this.cursors.length) return;
    const previous = this.page;
    this.page = page;
    if (!await this.load(false)) this.page = previous;
    this.updatePagination();
  }

  async load(includeSummary) {
    this.loading = true;
    this.root.setAttribute("aria-busy", "true");
    this.root.querySelectorAll("button, input, select").forEach(element => { element.disabled = true; });
    const query = new URLSearchParams({
      start: String(this.range.start), end: String(this.range.end),
      purpose: this.range.purpose,
      limit: "50", summary: String(includeSummary),
    });
    if (this.cursors[this.page]) query.set("cursor", this.cursors[this.page]);
    try {
      const data = await api.get(`/request-statistics?${query}`);
      this.nextCursor = data.next_cursor;
      this.renderRows(data.items);
      if (data.summary) this.renderSummary(data.summary);
      document.getElementById("statistics-range-label").textContent =
        `${data.purpose === "all" ? "全部用途" : purposeNames[data.purpose] || "未标注"} · ${timeFormat.format(new Date(data.start * 1000))} 至 ${timeFormat.format(new Date(data.end * 1000))}（不含结束时间，浏览器本地时间）`;
      return true;
    } catch (error) {
      toast(error.message, true);
      if (includeSummary) {
        this.rows.innerHTML = '<tr><td colspan="12" class="empty">请求记录加载失败，请重试</td></tr>';
        document.getElementById("statistics-request-detail").textContent = "加载失败";
        document.getElementById("statistics-coverage").textContent = "统计数据加载失败";
        document.getElementById("statistics-cost-detail").textContent = "加载失败";
      }
      return false;
    } finally {
      this.loading = false;
      this.root.setAttribute("aria-busy", "false");
      this.root.querySelectorAll("button, input, select").forEach(element => { element.disabled = false; });
      this.updatePagination();
    }
  }

  renderSummary(summary) {
    document.getElementById("statistics-requests").textContent = tokenText(summary.requests);
    document.getElementById("statistics-input").textContent = tokenText(summary.prompt_tokens);
    document.getElementById("statistics-output").textContent = tokenText(summary.completion_tokens);
    document.getElementById("statistics-total").textContent = tokenText(summary.total_tokens);
    document.getElementById("statistics-cache-rate").textContent = rateText(summary.cache_hit_rate);
    document.getElementById("statistics-cost").textContent = costText(summary.estimated_cost_usd);
    document.getElementById("statistics-cost-detail").textContent =
      `已估算 ${tokenText(summary.cost_known_requests)} / ${tokenText(summary.requests)} 条，未知不计入`;
    document.getElementById("statistics-request-detail").textContent =
      `成功 ${tokenText(summary.successful)} · 失败 ${tokenText(summary.failed)} · 中断 ${tokenText(summary.interrupted)}`;
    document.getElementById("statistics-coverage").textContent =
      `${tokenText(summary.usage_reported)} / ${tokenText(summary.requests)} 条请求返回了用量数据。缓存 0.00% 表示未命中，— 表示无数据或输入为 0。输出含思考用量；总 Token 使用供应商返回值，缓存已包含在输入中。`;
  }

  renderRows(items) {
    this.rows.innerHTML = items.length ? items.map(item => {
      const rate = item.cached_tokens != null && item.prompt_tokens > 0
        ? item.cached_tokens / item.prompt_tokens : null;
      const cacheText = item.cached_tokens == null ? "未返回"
        : item.prompt_tokens == null ? "输入未返回"
        : item.prompt_tokens === 0 ? "不适用" : rateText(rate);
      const elapsed = item.elapsed_ms < 1000 ? `${item.elapsed_ms} ms` : `${(item.elapsed_ms / 1000).toFixed(2)} s`;
      const statusClass = Object.hasOwn(outcomeNames, item.outcome) ? item.outcome : "interrupted";
      const status = outcomeNames[item.outcome] || "未知";
      return `<tr>
        <td class="statistics-time">${escapeHtml(timeFormat.format(new Date(item.started_at * 1000)))}</td>
        <td><strong>${escapeHtml(item.provider_name)}</strong><small>${escapeHtml(providerNames[item.provider_type] || item.provider_type)}</small></td>
        <td class="statistics-model"><strong title="${escapeHtml(item.model_name)}">${escapeHtml(item.model_name || item.model)}</strong><small title="${escapeHtml(item.model)}">${escapeHtml(item.model)}</small></td>
        <td>${escapeHtml(purposeNames[item.purpose] || "未标注")}</td>
        <td>${escapeHtml(responseKindNames[item.response_kind] || "未取得响应")}</td>
        <td><span class="statistics-outcome ${statusClass}">${status}</span>${item.http_status && item.http_status >= 400 ? `<small>HTTP ${item.http_status}</small>` : ""}</td>
        <td class="statistics-number">${tokenText(item.prompt_tokens)}</td>
        <td class="statistics-number">${tokenText(item.completion_tokens)}</td>
        <td class="statistics-number">${tokenText(item.total_tokens)}</td>
        <td class="statistics-number">${escapeHtml(cacheText)}</td>
        <td class="statistics-number">${escapeHtml(costText(item.estimated_cost_usd))}</td>
        <td class="statistics-number">${elapsed}</td>
      </tr>`;
    }).join("") : '<tr><td colspan="12" class="empty">这个时间范围内没有请求记录</td></tr>';
  }

  updatePagination() {
    document.getElementById("statistics-previous").disabled = this.loading || this.page === 0;
    document.getElementById("statistics-next").disabled = this.loading || !this.nextCursor;
    document.getElementById("statistics-page-label").textContent = `第 ${this.page + 1} 页 · 每页 50 条`;
  }
}
