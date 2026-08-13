export function escapeHtml(value) {
  return String(value ?? "").replace(/[&<>'"]/g, character => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;"
  })[character]);
}

export function refreshIcons() {
  window.lucide?.createIcons({ attrs: { "stroke-width": 1.8 } });
}

export function toast(message, error = false) {
  const element = document.createElement("div");
  element.className = `toast${error ? " error" : ""}`;
  element.textContent = message;
  document.getElementById("toast-region").append(element);
  setTimeout(() => element.remove(), 4200);
}

export function formatTime(timestamp, withDate = false) {
  if (!timestamp) return "-";
  const date = new Date(timestamp * 1000);
  return new Intl.DateTimeFormat("zh-CN", {
    month: withDate ? "2-digit" : undefined,
    day: withDate ? "2-digit" : undefined,
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

export function initial(value) {
  return Array.from(String(value || "?").trim())[0] || "?";
}

export function lines(value) {
  return String(value || "").split(/[\n,]/).map(item => item.trim()).filter(Boolean);
}

export function openDialog({ title, eyebrow = "EDIT", fields, submitText = "保存" }) {
  const dialog = document.getElementById("entity-dialog");
  const form = dialog.querySelector("form");
  const container = document.getElementById("dialog-fields");
  const error = document.getElementById("dialog-error");
  document.getElementById("dialog-title").textContent = title;
  document.getElementById("dialog-eyebrow").textContent = eyebrow;
  document.getElementById("dialog-submit").textContent = submitText;
  error.hidden = true;
  container.innerHTML = fields.map(field => {
    const required = field.required === false ? "" : "required";
    if (field.type === "textarea") {
      return `<label>${escapeHtml(field.label)}<textarea name="${escapeHtml(field.name)}" rows="${field.rows || 5}" ${required}>${escapeHtml(field.value)}</textarea></label>`;
    }
    if (field.type === "select") {
      const options = field.options.map(option => `<option value="${escapeHtml(option.value)}" ${String(option.value) === String(field.value) ? "selected" : ""}>${escapeHtml(option.label)}</option>`).join("");
      return `<label>${escapeHtml(field.label)}<select name="${escapeHtml(field.name)}" ${required}>${options}</select></label>`;
    }
    return `<label>${escapeHtml(field.label)}<input name="${escapeHtml(field.name)}" type="${field.type || "text"}" value="${escapeHtml(field.value)}" ${field.min != null ? `min="${field.min}"` : ""} ${field.max != null ? `max="${field.max}"` : ""} ${required}></label>`;
  }).join("");
  refreshIcons();

  return new Promise(resolve => {
    const close = () => {
      dialog.removeEventListener("close", onClose);
      form.removeEventListener("submit", onSubmit);
    };
    const onClose = () => { close(); resolve(null); };
    const onSubmit = event => {
      event.preventDefault();
      if (event.submitter?.value === "cancel") {
        close();
        dialog.close();
        resolve(null);
        return;
      }
      if (!form.reportValidity()) return;
      const values = Object.fromEntries(new FormData(form));
      close();
      dialog.close();
      resolve(values);
    };
    dialog.addEventListener("close", onClose);
    form.addEventListener("submit", onSubmit);
    dialog.showModal();
  });
}
