const TOKEN_KEY = "rolava.admin.token";

class AdminApi {
  get token() { return sessionStorage.getItem(TOKEN_KEY) || ""; }
  set token(value) { value ? sessionStorage.setItem(TOKEN_KEY, value) : sessionStorage.removeItem(TOKEN_KEY); }

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
