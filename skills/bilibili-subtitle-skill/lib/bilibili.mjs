import protobuf from 'protobufjs';
import {
  API,
  DEFAULT_HEADERS,
  HttpError,
  request,
  assertBiliSuccess,
  authCookieHeader,
} from './core.mjs';

const BV_PATTERN = /^BV[0-9A-Za-z]{10}$/i;
const MODIFIED_Z_THRESHOLD = 3.5;
const SECONDARY_MODIFIED_Z_THRESHOLD = 2.8;
const GAP_RATIO_THRESHOLD = 3;
const MAD_EPSILON = 1e-9;
const SUBTITLE_URL_ENCODINGS = [
  {
    prefix: 'nP](wOFRvU.+<fjS{jn-!$D|Dz&",zT`',
    key: '=CFxYRn{.y|uVyO$uh&sikph?N.ilF/`bilibili',
  },
  {
    prefix: 'Bn"q~|albg@]Go~ACgyDvKnd+)_D}^&J?',
    key: "Cu~L!xs~f^&r@'vh=q]q{eeng*sEg^kp#Jbilibili",
  },
];
const SUBTITLE_SCHEMA = `
  syntax = "proto3";
  message SubtitleResponse { SubtitleData data = 1; }
  message SubtitleData { repeated SubtitleTrack subtitles = 3; }
  message SubtitleTrack {
    int64 id = 1;
    string id_str = 2;
    string lan = 3;
    string lan_doc = 4;
    string subtitle_url = 5;
    int32 type = 7;
    string lan_doc_brief = 8;
    int32 ai_type = 9;
    int32 ai_status = 10;
  }
`;
const SubtitleResponse = protobuf.parse(SUBTITLE_SCHEMA).root.lookupType('SubtitleResponse');

export async function parseVideoInput(rawInput, explicitPart = undefined) {
  const input = String(rawInput ?? '').trim();
  if (!input) throw new Error('缺少B站视频链接或BV号');
  if (BV_PATTERN.test(input)) {
    return { bvid: normalizeBvid(input), page: parsePage(explicitPart ?? 1) };
  }

  let source = input;
  if (!/^https?:\/\//i.test(source)) source = `https://${source}`;
  let url;
  try {
    url = new URL(source);
  } catch {
    throw new Error('不是有效的B站视频链接或BV号');
  }
  const host = url.hostname.toLowerCase();
  if (host === 'b23.tv' || host.endsWith('.b23.tv')) {
    url = await resolveShortLink(url);
  }
  if (!isBilibiliHost(url.hostname)) throw new Error('只支持 bilibili.com 或 b23.tv 链接');
  const match = url.pathname.match(/\/(BV[0-9A-Za-z]{10})(?:\/|$)/i);
  if (!match) throw new Error('B站链接中没有有效的BV号');
  return {
    bvid: normalizeBvid(match[1]),
    page: parsePage(explicitPart ?? url.searchParams.get('p') ?? 1),
  };
}

async function resolveShortLink(url) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 12_000);
  try {
    const response = await fetch(url, {
      redirect: 'follow',
      headers: DEFAULT_HEADERS,
      signal: controller.signal,
    });
    await response.body?.cancel();
    if (!response.ok) throw new Error(`B站短链接解析失败（HTTP ${response.status}）`);
    const resolved = new URL(response.url);
    if (!isBilibiliHost(resolved.hostname)) throw new Error('B站短链接跳转到了不受支持的站点');
    return resolved;
  } catch (error) {
    if (error?.name === 'AbortError') throw new Error('B站短链接解析超时');
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

function isBilibiliHost(hostname) {
  const host = String(hostname).toLowerCase();
  return host === 'bilibili.com' || host.endsWith('.bilibili.com');
}

function normalizeBvid(value) {
  return `BV${String(value).slice(2)}`;
}

function parsePage(value) {
  const page = Number(value);
  if (!Number.isInteger(page) || page < 1 || page > 10_000) {
    throw new Error('分P编号必须是大于0的整数');
  }
  return page;
}

export async function getVideoInfo({ bvid, page = 1, auth }) {
  const url = new URL(API.VIDEO_VIEW);
  url.searchParams.set('bvid', bvid);
  const payload = await request(url, {
    headers: auth ? { Cookie: authCookieHeader(auth) } : undefined,
  });
  const data = assertBiliSuccess(payload, 'B站视频信息接口');
  const pages = Array.isArray(data?.pages) ? data.pages : [];
  const selected = pages[page - 1];
  if (!selected?.cid) throw new Error(`视频不存在第 ${page} 个分P`);
  return {
    aid: String(data.aid),
    bvid: data.bvid || bvid,
    cid: String(selected.cid),
    title: String(data.title || selected.part || bvid),
    page,
  };
}

export async function getSubtitleTracks({ aid, cid, auth }) {
  const url = new URL(API.SUBTITLE_WEB_VIEW);
  url.searchParams.set('oid', cid);
  url.searchParams.set('pid', aid);
  url.searchParams.set('context_ext', JSON.stringify({ video_type: 1 }));
  url.searchParams.set('type', '1');
  url.searchParams.set('cur_production_type', '0');
  url.searchParams.set('preferred_language', 'ai-zh');
  url.searchParams.set('playlist_switch', '0');
  const bytes = await request(url, {
    responseType: 'buffer',
    maxBytes: 2 * 1024 * 1024,
    headers: {
      Accept: 'application/x-protobuf,application/octet-stream,*/*',
      Cookie: authCookieHeader(auth),
    },
  });
  let decoded;
  try {
    decoded = SubtitleResponse.toObject(SubtitleResponse.decode(bytes), {
      longs: String,
      arrays: true,
      defaults: false,
    });
  } catch {
    throw new Error('B站字幕接口返回的Protobuf内容无法解析');
  }
  return (decoded?.data?.subtitles ?? [])
    .map(track => ({
      id: String(track.idStr || track.id || ''),
      language: String(track.lan || ''),
      language_name: String(track.lanDoc || track.lanDocBrief || ''),
      url: normalizeSubtitleUrl(track.subtitleUrl),
      source: isAiTrack(track) ? 'ai' : 'human',
    }))
    .filter(track => track.url);
}

export function chooseTrack(tracks, preferredLanguage = undefined) {
  const available = Array.isArray(tracks) ? tracks : [];
  const preferred = String(preferredLanguage || '').toLowerCase();
  if (preferred) {
    const exact = available.find(track => track.language.toLowerCase() === preferred);
    if (exact) return exact;
  }
  const humanChinese = available.find(track => (
    track.source === 'human' && /^(zh|cn|chi)(?:-|$)/i.test(track.language)
  ));
  if (humanChinese) return humanChinese;
  const aiChinese = available.find(track => /^ai-(zh|cn|chi)(?:-|$)/i.test(track.language));
  if (aiChinese) return aiChinese;
  return available.find(track => track.source === 'human') ?? available[0] ?? null;
}

function isAiTrack(track) {
  if (Object.hasOwn(track, 'type')) return Number(track.type) === 1;
  if (String(track.lan || '').toLowerCase().startsWith('ai-')) return true;
  return Object.hasOwn(track, 'aiType') && Number(track.aiType) !== 0;
}

export async function downloadSubtitleJson(url) {
  return request(url, {
    maxBytes: 10 * 1024 * 1024,
  });
}

export function normalizeSubtitleUrl(value) {
  let url = String(value || '').trim();
  if (!url) return null;
  if (url.startsWith('//')) url = `https:${url}`;
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== 'https:') return null;
  if (parsed.hostname.toLowerCase() !== 'subtitle.bilibili.com') return parsed.toString();

  const encodedPath = parsed.pathname.slice(1);
  if (/%(?![0-9a-f]{2})/i.test(encodedPath)) {
    throw new Error('B站字幕地址包含无效的百分号编码');
  }
  let cipher;
  try {
    cipher = decodeURIComponent(encodedPath);
  } catch {
    throw new Error('B站字幕地址包含无效的百分号编码');
  }
  for (const { prefix, key } of SUBTITLE_URL_ENCODINGS) {
    const plain = Array.from(cipher, (character, index) => (
      String.fromCharCode(character.charCodeAt(0) ^ key.charCodeAt(index % key.length))
    )).join('');
    if (!plain.startsWith(prefix)) continue;
    const path = plain.slice(prefix.length);
    if (!path.startsWith('/bfs/')
      || path.length === '/bfs/'.length
      || /[\u0000-\u001f\u007f]/.test(path)
      || /[?#\\]/.test(path)
      || path.split('/').some(segment => segment === '.' || segment === '..')) {
      throw new Error('B站字幕地址解码后的路径无效');
    }
    return `https://aisubtitle.hdslb.com${path}${parsed.search}`;
  }
  throw new Error('暂不支持此B站字幕地址编码，请更新程序后重试');
}

function subtitleTime(value) {
  if (value === null || value === undefined || value === '') return null;
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds >= 0 ? seconds : null;
}

function subtitleCues(body) {
  return (Array.isArray(body) ? body : [])
    .map(item => {
      const content = String(item?.content ?? '').replace(/\s+/g, ' ').trim();
      const from = subtitleTime(item?.from);
      const rawTo = subtitleTime(item?.to);
      const to = from === null || rawTo === null ? (rawTo ?? from) : Math.max(from, rawTo);
      return { content, from, to };
    })
    .filter(cue => cue.content);
}

function median(values) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0
    ? (sorted[middle - 1] + sorted[middle]) / 2
    : sorted[middle];
}

function naturalGapAnchors(cues) {
  const gaps = [];
  const durations = [];
  let previousEnd = null;

  for (let index = 0; index < cues.length; index += 1) {
    const cue = cues[index];
    if (cue.from === null) continue;
    if (cue.to !== null && cue.to > cue.from) durations.push(cue.to - cue.from);
    if (previousEnd !== null) {
      gaps.push({ index, seconds: Math.max(0, cue.from - previousEnd) });
    }
    previousEnd = Math.max(previousEnd ?? cue.from, cue.to ?? cue.from);
  }

  if (gaps.length === 0) return new Set();
  const values = gaps.map(gap => gap.seconds);
  const positiveValues = values.filter(value => value > MAD_EPSILON);
  const typicalGap = median(positiveValues) ?? 0;
  const typicalDuration = median(durations) ?? 0;
  const logValues = positiveValues.map(value => Math.log1p(value));
  const logCenter = median(logValues) ?? 0;
  const logMad = median(logValues.map(value => Math.abs(value - logCenter))) ?? 0;
  const meaningfulGap = Math.max(
    typicalGap * GAP_RATIO_THRESHOLD,
    typicalDuration / 2,
  );
  const scoredGaps = logMad > MAD_EPSILON
    ? gaps.map(gap => ({
        ...gap,
        score: 0.6744897501960817 * (Math.log1p(gap.seconds) - logCenter) / logMad,
      }))
    : [];
  let threshold = null;

  for (const gap of scoredGaps) {
    if (gap.seconds >= meaningfulGap && gap.score >= MODIFIED_Z_THRESHOLD) {
      threshold = Math.min(threshold ?? gap.seconds, gap.seconds);
    }
  }

  if (threshold === null) {
    const sorted = [...new Set(values)].sort((left, right) => left - right);
    for (let index = 1; index < sorted.length; index += 1) {
      const lower = sorted[index - 1];
      const upper = sorted[index];
      const separated = lower <= MAD_EPSILON
        ? upper > MAD_EPSILON
        : upper / lower >= GAP_RATIO_THRESHOLD;
      if (separated && upper >= typicalDuration / 2) {
        threshold = upper;
        break;
      }
    }
  }

  if (threshold !== null) {
    for (const gap of scoredGaps) {
      if (gap.seconds >= meaningfulGap && gap.score >= SECONDARY_MODIFIED_Z_THRESHOLD) {
        threshold = Math.min(threshold, gap.seconds);
      }
    }
  }

  return new Set(
    threshold === null
      ? []
      : gaps.filter(gap => gap.seconds >= threshold).map(gap => gap.index),
  );
}

function minuteAnchors(cues) {
  const timedCues = cues
    .map((cue, index) => ({ index, seconds: cue.from }))
    .filter(cue => cue.seconds !== null);
  if (timedCues.length === 0) return new Set();

  const anchors = new Set([timedCues[0].index, timedCues.at(-1).index]);
  const finalTime = timedCues.at(-1).seconds;
  let cueIndex = 0;
  for (let minute = 60; minute < finalTime; minute += 60) {
    while (cueIndex < timedCues.length && timedCues[cueIndex].seconds < minute) {
      cueIndex += 1;
    }
    if (cueIndex < timedCues.length) anchors.add(timedCues[cueIndex].index);
  }
  return anchors;
}

function formatTimestamp(seconds) {
  const totalSeconds = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor(totalSeconds % 3600 / 60);
  const remainingSeconds = totalSeconds % 60;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, '0')}:${String(remainingSeconds).padStart(2, '0')}`
    : `${minutes}:${String(remainingSeconds).padStart(2, '0')}`;
}

export function toTimedText(body) {
  const cues = subtitleCues(body);
  if (cues.length === 0) return '';
  let anchors = naturalGapAnchors(cues);
  if (anchors.size > 0) {
    if (cues[0].from !== null) anchors.add(0);
    if (cues.at(-1).from !== null) anchors.add(cues.length - 1);
  } else {
    anchors = minuteAnchors(cues);
  }
  return cues
    .map((cue, index) => (
      anchors.has(index) && cue.from !== null
        ? `[${formatTimestamp(cue.from)}] ${cue.content}`
        : cue.content
    ))
    .join(' ');
}

export function isExpiredSubtitleUrl(error) {
  return error instanceof HttpError && error.status === 403;
}
