const TOKEN_KEY = "rolava.admin.token";
const TOKEN_EXPIRES_AT_KEY = "rolava.admin.token.expiresAt";
const TOKEN_TTL_MS = 7 * 24 * 60 * 60 * 1000;

class AdminApi {
  get token() {
    const token = localStorage.getItem(TOKEN_KEY) || "";
    const expiresAt = Number(localStorage.getItem(TOKEN_EXPIRES_AT_KEY));
    if (!token || !Number.isFinite(expiresAt) || Date.now() >= expiresAt) {
      this.token = "";
      return "";
    }
    return token;
  }

  set token(value) {
    if (value) {
      localStorage.setItem(TOKEN_KEY, value);
      localStorage.setItem(TOKEN_EXPIRES_AT_KEY, String(Date.now() + TOKEN_TTL_MS));
      return;
    }
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(TOKEN_EXPIRES_AT_KEY);
  }

  async request(path, options = {}) {
    const headers = new Headers(options.headers || {});
    headers.set("Authorization", `Bearer ${this.token}`);
    if (options.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    const response = await fetch(`/api/admin${path}`, { ...options, headers });
    const body = await response.json().catch(() => ({}));
    if (response.status === 401) {
      this.token = "";
      window.dispatchEvent(new Event("admin-auth-expired"));
    }
    if (!response.ok) throw new Error(body.error || `请求失败 (${response.status})`);
    return body;
  }

  get(path) { return this.request(path); }
  post(path, body = {}) { return this.request(path, { method: "POST", body: JSON.stringify(body) }); }
  put(path, body) { return this.request(path, { method: "PUT", body: JSON.stringify(body) }); }
  delete(path) { return this.request(path, { method: "DELETE" }); }
}

export const api = new AdminApi();
