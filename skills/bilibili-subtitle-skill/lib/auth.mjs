import crypto from 'node:crypto';
import {
  API,
  DEFAULT_HEADERS,
  request,
  assertBiliSuccess,
  authCookieHeader,
  loadAuth,
  saveAuth,
  saveLoginState,
  loadLoginState,
  clearLoginState,
  withAuthLock,
} from './core.mjs';

const WEB_REFRESH_PUBLIC_KEY = `-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDLgd2OAkcGVtoE3ThUREbio0Eg
Uc/prcajMKXvkCKFCWhJYJcLkcM2DKKcSeFpD/j6Boy538YXnR6VhcuUJOhH2x71
nzPjfdTcqMz7djHum0qSZA0AyCBDABUqCrfNgCiJ00Ra7GmRj+YCK1NJEuewlb40
JNrRuoEUXpabUzGB8QIDAQAB
-----END PUBLIC KEY-----`;
const QR_LIFETIME_MS = 180_000;
const CHECK_INTERVAL_MS = 12 * 60 * 60 * 1000;
const LOGIN_COOKIE_NAMES = new Set([
  'DedeUserID',
  'DedeUserID__ckMd5',
  'SESSDATA',
  'bili_jct',
  'sid',
]);

const QR_STATUS = Object.freeze({
  SUCCESS: 0,
  WAITING: 86101,
  SCANNED: 86090,
  EXPIRED: 86038,
});

export async function startQrLogin() {
  const url = new URL(API.QR_GENERATE);
  url.searchParams.set('source', 'main-fe-header');
  const payload = await request(url);
  const data = assertBiliSuccess(payload, 'B站WEB二维码接口');
  if (!data?.url || !data?.qrcode_key) {
    throw new Error('B站WEB二维码接口没有返回登录地址或二维码密钥');
  }
  const createdAt = Date.now();
  const expiresAt = createdAt + QR_LIFETIME_MS;
  await saveLoginState({ qrcodeKey: data.qrcode_key, createdAt, expiresAt });
  const { default: QRCode } = await import('qrcode');
  const qrImage = await QRCode.toDataURL(data.url, {
    errorCorrectionLevel: 'M',
    margin: 2,
    width: 260,
    color: { dark: '#111111ff', light: '#ffffffff' },
  });
  return { status: 'waiting', qr_image: qrImage, expires_at: expiresAt };
}

export async function pollQrLogin() {
  const state = await loadLoginState();
  if (!state?.qrcodeKey || Date.now() >= Number(state.expiresAt)) {
    await clearLoginState();
    return { status: 'expired', message: '二维码已失效，请重新获取' };
  }

  const url = new URL(API.QR_POLL);
  url.searchParams.set('qrcode_key', state.qrcodeKey);
  url.searchParams.set('source', 'main-fe-header');
  const { data: payload, response } = await request(url, { includeResponse: true });
  const data = assertBiliSuccess(payload, 'B站WEB扫码登录接口');
  const code = Number(data?.code);
  if (code === QR_STATUS.WAITING) {
    return { status: 'waiting', message: data?.message || '等待扫码' };
  }
  if (code === QR_STATUS.SCANNED) {
    return { status: 'scanned', message: data?.message || '已扫码，等待确认' };
  }
  if (code === QR_STATUS.EXPIRED) {
    await clearLoginState();
    return { status: 'expired', message: data?.message || '二维码已失效' };
  }
  if (code !== QR_STATUS.SUCCESS) {
    throw new Error(`B站WEB扫码登录接口返回 ${code}：${data?.message || '请求失败'}`);
  }

  let cookies = parseLoginUrlCookies(data?.url);
  cookies = parseSetCookies(response.headers, cookies);
  if (!hasRequiredLoginCookies(cookies) && data?.url) {
    cookies = await collectCrossDomainCookies(data.url, cookies);
  }
  const auth = webAuthFromResponse(data, response.headers, undefined, undefined, cookies);
  await withAuthLock(() => saveAuth(auth));
  await clearLoginState();
  return { status: 'success', message: 'WEB端登录凭据已保存' };
}

function webAuthFromResponse(
  data,
  headers,
  previous = undefined,
  pendingConfirm = undefined,
  loginCookies = undefined,
) {
  if (typeof data?.refresh_token !== 'string' || !data.refresh_token) {
    throw new Error('B站WEB登录接口未返回 refresh_token');
  }
  const cookies = parseSetCookies(headers, { ...previous?.cookies, ...loginCookies });
  for (const required of ['SESSDATA', 'bili_jct']) {
    if (!cookies[required]) throw new Error(`B站WEB登录接口未返回 ${required}`);
  }
  const now = Date.now();
  return {
    loginType: 'web',
    refreshToken: data.refresh_token,
    cookies,
    updatedAt: now,
    lastCheckedAt: now,
    ...(pendingConfirm ? { pendingConfirmRefreshToken: pendingConfirm } : {}),
  };
}

function parseSetCookies(headers, previousCookies = undefined) {
  const lines = typeof headers?.getSetCookie === 'function' ? headers.getSetCookie() : [];
  const cookies = { ...(previousCookies ?? {}) };

  for (const line of lines) {
    const segments = String(line).split(';').map(segment => segment.trim());
    const separator = segments[0].indexOf('=');
    if (separator <= 0) continue;
    const name = segments[0].slice(0, separator);
    const value = segments[0].slice(separator + 1);
    let deleted = !value;
    for (const attribute of segments.slice(1)) {
      const index = attribute.indexOf('=');
      const key = (index < 0 ? attribute : attribute.slice(0, index)).trim().toLowerCase();
      const attributeValue = index < 0 ? '' : attribute.slice(index + 1).trim();
      if (key === 'max-age') {
        const seconds = Number(attributeValue);
        if (Number.isFinite(seconds) && seconds <= 0) deleted = true;
      }
    }
    if (deleted) delete cookies[name];
    else cookies[name] = value;
  }

  return cookies;
}

function parseLoginUrlCookies(value) {
  if (typeof value !== 'string' || !value) return {};
  let url;
  try {
    url = new URL(value);
  } catch {
    return {};
  }
  const cookies = {};
  for (const name of LOGIN_COOKIE_NAMES) {
    const cookie = url.searchParams.get(name);
    if (cookie) cookies[name] = cookie;
  }
  return cookies;
}

function hasRequiredLoginCookies(cookies) {
  return ['SESSDATA', 'bili_jct'].every(name => cookies?.[name]);
}

function isTrustedLoginHost(hostname) {
  const host = String(hostname).toLowerCase();
  return host === 'bilibili.com'
    || host.endsWith('.bilibili.com')
    || host === 'biligame.com'
    || host.endsWith('.biligame.com');
}

async function collectCrossDomainCookies(value, initialCookies) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error('B站WEB登录接口返回了无效的跨域登录地址');
  }
  if (url.protocol !== 'https:' || !isTrustedLoginHost(url.hostname)) {
    throw new Error('B站WEB登录接口返回了不受信任的跨域登录地址');
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 15_000);
  let cookies = { ...(initialCookies ?? {}) };
  try {
    for (let redirects = 0; redirects <= 5; redirects += 1) {
      const response = await fetch(url, {
        redirect: 'manual',
        headers: {
          ...DEFAULT_HEADERS,
          ...(Object.keys(cookies).length
            ? { Cookie: Object.entries(cookies).map(([name, cookie]) => `${name}=${cookie}`).join('; ') }
            : {}),
        },
        signal: controller.signal,
      });
      cookies = parseSetCookies(response.headers, cookies);
      await response.body?.cancel();
      if (hasRequiredLoginCookies(cookies)) return cookies;

      if (response.status < 300 || response.status >= 400) break;
      const location = response.headers.get('location');
      if (!location) break;
      url = new URL(location, url);
      if (url.protocol !== 'https:' || !isTrustedLoginHost(url.hostname)) {
        throw new Error('B站WEB跨域登录跳转到了不受信任的地址');
      }
    }
  } catch (error) {
    if (error?.name === 'AbortError') throw new Error('B站WEB跨域登录请求超时');
    throw error;
  } finally {
    clearTimeout(timer);
  }
  return cookies;
}

async function checkCookieState(auth) {
  const url = new URL(API.COOKIE_INFO);
  if (auth.cookies?.bili_jct) url.searchParams.set('csrf', auth.cookies.bili_jct);
  const payload = await request(url, {
    headers: { Cookie: authCookieHeader(auth) },
  });
  const data = assertBiliSuccess(payload, 'B站WEB登录状态接口');
  const timestamp = Number(data?.timestamp);
  return {
    refresh: data?.refresh === true,
    timestamp: Number.isFinite(timestamp) ? timestamp : Date.now(),
  };
}

function createCorrespondPath(timestamp) {
  return crypto.publicEncrypt({
    key: WEB_REFRESH_PUBLIC_KEY,
    padding: crypto.constants.RSA_PKCS1_OAEP_PADDING,
    oaepHash: 'sha256',
  }, Buffer.from(`refresh_${timestamp}`)).toString('hex');
}

async function getRefreshCsrf(auth, timestamp) {
  const correspondPath = createCorrespondPath(timestamp);
  const html = await request(`${API.COOKIE_CORRESPOND}/${correspondPath}`, {
    responseType: 'text',
    maxBytes: 512 * 1024,
    headers: { Cookie: authCookieHeader(auth) },
  });
  const match = html.match(/<div[^>]*\bid=["']1-name["'][^>]*>([^<]+)<\/div>/i);
  const refreshCsrf = match?.[1]?.trim();
  if (!refreshCsrf) throw new Error('B站WEB刷新页面未返回 refresh_csrf');
  return refreshCsrf;
}

function formBody(values) {
  const body = new URLSearchParams();
  for (const [name, value] of Object.entries(values)) body.set(name, String(value));
  return body.toString();
}

async function confirmRefresh(auth, oldRefreshToken) {
  const payload = await request(API.COOKIE_CONFIRM, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/x-www-form-urlencoded;charset=UTF-8',
      Cookie: authCookieHeader(auth),
      Origin: 'https://www.bilibili.com',
    },
    body: formBody({
      csrf: auth.cookies.bili_jct,
      refresh_token: oldRefreshToken,
    }),
  });
  assertBiliSuccess(payload, 'B站WEB凭据刷新确认接口');
}

async function completePendingConfirmation(auth) {
  if (!auth.pendingConfirmRefreshToken) return auth;
  await confirmRefresh(auth, auth.pendingConfirmRefreshToken);
  const confirmed = { ...auth, lastCheckedAt: Date.now() };
  delete confirmed.pendingConfirmRefreshToken;
  await saveAuth(confirmed);
  return confirmed;
}

async function refreshWebAuth(auth, timestamp) {
  if (!auth.refreshToken || !auth.cookies?.bili_jct) {
    throw new Error('保存的B站WEB刷新凭据不完整，请重新扫码获取');
  }
  const refreshCsrf = await getRefreshCsrf(auth, timestamp);
  const oldRefreshToken = auth.refreshToken;
  const { data: payload, response } = await request(API.COOKIE_REFRESH, {
    method: 'POST',
    includeResponse: true,
    headers: {
      'Content-Type': 'application/x-www-form-urlencoded;charset=UTF-8',
      Cookie: authCookieHeader(auth),
      Origin: 'https://www.bilibili.com',
    },
    body: formBody({
      csrf: auth.cookies.bili_jct,
      refresh_csrf: refreshCsrf,
      source: 'main_web',
      refresh_token: oldRefreshToken,
    }),
  });
  const data = assertBiliSuccess(payload, 'B站WEB凭据刷新接口');
  const refreshed = webAuthFromResponse(data, response.headers, auth, oldRefreshToken);

  await saveAuth(refreshed);
  return completePendingConfirmation(refreshed);
}

export async function refreshAuthIfNeeded({ forceCheck = false } = {}) {
  const existing = await loadAuth();
  if (!existing) return { auth: null, refreshed: false };
  const due = Boolean(existing.pendingConfirmRefreshToken)
    || forceCheck
    || Date.now() - Number(existing.lastCheckedAt || 0) >= CHECK_INTERVAL_MS;
  if (!due) return { auth: existing, refreshed: false };

  return withAuthLock(async () => {
    let auth = await loadAuth();
    if (!auth) return { auth: null, refreshed: false };
    auth = await completePendingConfirmation(auth);
    const shouldCheck = forceCheck
      || Date.now() - Number(auth.lastCheckedAt || 0) >= CHECK_INTERVAL_MS;
    if (!shouldCheck) return { auth, refreshed: false };

    const state = await checkCookieState(auth);
    if (state.refresh) {
      return { auth: await refreshWebAuth(auth, state.timestamp), refreshed: true };
    }
    const checkedAuth = { ...auth, lastCheckedAt: Date.now() };
    await saveAuth(checkedAuth);
    return { auth: checkedAuth, refreshed: false };
  });
}
