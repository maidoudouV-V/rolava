import fs from 'node:fs/promises';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

export const API = Object.freeze({
  QR_GENERATE: 'https://passport.bilibili.com/x/passport-login/web/qrcode/generate',
  QR_POLL: 'https://passport.bilibili.com/x/passport-login/web/qrcode/poll',
  COOKIE_INFO: 'https://passport.bilibili.com/x/passport-login/web/cookie/info',
  COOKIE_REFRESH: 'https://passport.bilibili.com/x/passport-login/web/cookie/refresh',
  COOKIE_CONFIRM: 'https://passport.bilibili.com/x/passport-login/web/confirm/refresh',
  COOKIE_CORRESPOND: 'https://www.bilibili.com/correspond/1',
  VIDEO_VIEW: 'https://api.bilibili.com/x/web-interface/view',
  SUBTITLE_WEB_VIEW: 'https://api.bilibili.com/x/v2/subtitle/web/view',
});

export const DEFAULT_HEADERS = Object.freeze({
  Accept: '*/*',
  'User-Agent':
    'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/140 Safari/537.36',
  Referer: 'https://www.bilibili.com/',
});

const here = path.dirname(fileURLToPath(import.meta.url));
const skillRoot = path.resolve(here, '..');
const projectRoot = path.resolve(skillRoot, '..', '..');
const appDataRoot = path.join(projectRoot, 'data');

export const DATA_DIR = path.join(appDataRoot, 'bilibili-subtitle');
export const AUTH_FILE = path.join(DATA_DIR, 'auth.json');
export const LOGIN_STATE_FILE = path.join(DATA_DIR, 'login-state.json');
export const REFRESH_LOCK_FILE = path.join(DATA_DIR, 'refresh.lock');

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.name = 'HttpError';
    this.status = status;
  }
}

export function jsonOut(payload) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
}

export function fail(error) {
  const message = compactMessage(error?.message ?? error);
  jsonOut({ error: message || '未知错误' });
}

export function compactMessage(value, maxChars = 1000) {
  const text = String(value ?? '').replace(/\s+/g, ' ').trim();
  return text.length <= maxChars ? text : `${text.slice(0, maxChars - 1)}…`;
}

export function baseHeaders(extra = {}) {
  return { ...DEFAULT_HEADERS, ...extra };
}

export async function request(url, options = {}) {
  const {
    responseType = 'json',
    includeResponse = false,
    timeoutMs = 15_000,
    maxBytes = 2 * 1024 * 1024,
    headers,
    ...fetchOptions
  } = options;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  let response;
  let bytes;
  try {
    response = await fetch(url, {
      ...fetchOptions,
      headers: baseHeaders(headers),
      signal: controller.signal,
    });

    const contentLength = Number(response.headers.get('content-length'));
    if (Number.isFinite(contentLength) && contentLength > maxBytes) {
      await response.body?.cancel();
      throw new Error(`B站接口响应超过 ${maxBytes} 字节限制`);
    }
    bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.byteLength > maxBytes) {
      throw new Error(`B站接口响应超过 ${maxBytes} 字节限制`);
    }
  } catch (error) {
    if (error?.name === 'AbortError') {
      throw new Error('请求B站接口超时');
    }
    throw error;
  } finally {
    clearTimeout(timer);
  }

  let data;
  if (responseType === 'buffer') {
    data = bytes;
  } else {
    const text = new TextDecoder().decode(bytes);
    if (responseType === 'text') {
      data = text;
    } else {
      try {
        data = text ? JSON.parse(text) : null;
      } catch {
        throw new Error(`B站接口返回了无法解析的内容（HTTP ${response.status}）`);
      }
    }
  }

  if (!response.ok) {
    const upstream = responseType === 'json'
      ? compactMessage(data?.message ?? data?.msg ?? '')
      : compactMessage(new TextDecoder().decode(bytes), 200);
    throw new HttpError(
      response.status,
      `B站接口请求失败（HTTP ${response.status}${upstream ? `：${upstream}` : ''}）`,
    );
  }
  return includeResponse ? { data, response } : data;
}

export function assertBiliSuccess(payload, context = 'B站接口') {
  const code = Number(payload?.code);
  if (code !== 0) {
    const message = compactMessage(payload?.message ?? payload?.msg ?? '请求失败');
    throw new Error(`${context}返回 ${code}：${message}`);
  }
  return payload?.data;
}

export async function ensureDataDir() {
  await fs.mkdir(DATA_DIR, { recursive: true, mode: 0o700 });
  const [rootStat, dataStat] = await Promise.all([
    fs.lstat(appDataRoot),
    fs.lstat(DATA_DIR),
  ]);
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()
    || dataStat.isSymbolicLink() || !dataStat.isDirectory()) {
    throw new Error('B站凭据目录必须是普通目录');
  }
  const [resolvedRoot, resolvedDataRoot, resolvedData] = await Promise.all([
    fs.realpath(projectRoot),
    fs.realpath(appDataRoot),
    fs.realpath(DATA_DIR),
  ]);
  const rootRelative = path.relative(resolvedRoot, resolvedDataRoot);
  const dataRelative = path.relative(resolvedDataRoot, resolvedData);
  if (rootRelative !== 'data'
    || !dataRelative
    || dataRelative.startsWith('..')
    || path.isAbsolute(dataRelative)) {
    throw new Error('B站凭据目录超出应用 data 目录');
  }
  await fs.chmod(DATA_DIR, 0o700).catch(() => {});
}

async function loadJson(file) {
  try {
    const stat = await fs.lstat(file);
    if (stat.isSymbolicLink() || !stat.isFile()) {
      throw new Error('B站凭据文件必须是普通文件');
    }
    return JSON.parse(await fs.readFile(file, 'utf8'));
  } catch (error) {
    if (error?.code === 'ENOENT') return null;
    throw error;
  }
}

async function savePrivateJson(file, value) {
  await ensureDataDir();
  const existing = await fs.lstat(file).catch(error => {
    if (error?.code === 'ENOENT') return null;
    throw error;
  });
  if (existing && (existing.isSymbolicLink() || !existing.isFile())) {
    throw new Error('B站凭据文件必须是普通文件');
  }
  const temporary = `${file}.${process.pid}.${Date.now()}.tmp`;
  try {
    await fs.writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, {
      encoding: 'utf8',
      mode: 0o600,
      flag: 'wx',
    });
    await fs.chmod(temporary, 0o600).catch(() => {});
    await fs.rename(temporary, file);
    await fs.chmod(file, 0o600).catch(() => {});
  } finally {
    await fs.rm(temporary, { force: true }).catch(() => {});
  }
}

export async function loadAuth() {
  const auth = await loadJson(AUTH_FILE);
  if (!auth) return null;
  if (!auth.cookies || typeof auth.cookies.SESSDATA !== 'string') {
    throw new Error('保存的B站登录凭据格式无效，请重新扫码获取');
  }
  if (auth.loginType !== 'web') {
    throw new Error('保存的不是WEB端登录凭据，请在管理页重新扫码');
  }
  if (typeof auth.refreshToken !== 'string' || !auth.refreshToken) {
    throw new Error('保存的B站WEB刷新凭据无效，请重新扫码获取');
  }
  return auth;
}

export async function saveAuth(auth) {
  await savePrivateJson(AUTH_FILE, auth);
}

export function authCookieHeader(auth) {
  return Object.entries(auth?.cookies ?? {})
    .filter(([name, value]) => name && typeof value === 'string' && value)
    .map(([name, value]) => `${name}=${value}`)
    .join('; ');
}

export async function loadLoginState() {
  return loadJson(LOGIN_STATE_FILE);
}

export async function saveLoginState(state) {
  await savePrivateJson(LOGIN_STATE_FILE, state);
}

export async function clearLoginState() {
  await fs.rm(LOGIN_STATE_FILE, { force: true });
}

export async function withAuthLock(task, timeoutMs = 8_000) {
  await ensureDataDir();
  const deadline = Date.now() + timeoutMs;
  let handle;
  while (!handle) {
    try {
      handle = await fs.open(REFRESH_LOCK_FILE, 'wx', 0o600);
    } catch (error) {
      if (error?.code !== 'EEXIST') throw error;
      const stat = await fs.stat(REFRESH_LOCK_FILE).catch(() => null);
      if (stat && Date.now() - stat.mtimeMs > 2 * 60_000) {
        await fs.rm(REFRESH_LOCK_FILE, { force: true });
        continue;
      }
      if (Date.now() >= deadline) throw new Error('B站登录凭据正在更新，请稍后重试');
      await delay(150);
    }
  }
  try {
    return await task();
  } finally {
    await handle.close().catch(() => {});
    await fs.rm(REFRESH_LOCK_FILE, { force: true }).catch(() => {});
  }
}
