#!/usr/bin/env node
import { refreshAuthIfNeeded } from './lib/auth.mjs';
import {
  parseVideoInput,
  getVideoInfo,
  getSubtitleTracks,
  downloadSubtitleJson,
  chooseTrack,
  toPlainText,
  isExpiredSubtitleUrl,
} from './lib/bilibili.mjs';
import { jsonOut, fail } from './lib/core.mjs';

const MAX_SUBTITLE_CHARS = 12_000;
const EDGE_CHARS = 6_000;

function parseArguments(args) {
  let url;
  let part;
  let language;
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === '--url') url = args[++index];
    else if (argument.startsWith('--url=')) url = argument.slice(6);
    else if (argument === '--part') part = args[++index];
    else if (argument.startsWith('--part=')) part = argument.slice(7);
    else if (argument === '--language') language = args[++index];
    else if (argument.startsWith('--language=')) language = argument.slice(11);
    else if (!argument.startsWith('-') && !url) url = argument;
    else throw new Error(`无法识别的参数：${argument}`);
  }
  if (!url) throw new Error('用法：subtitle.mjs --url="B站链接或BV号" [--part=1]');
  return { url, part, language };
}

function truncateSubtitle(text) {
  const characters = Array.from(text);
  if (characters.length <= MAX_SUBTITLE_CHARS) {
    return { subtitle: text, total_chars: characters.length, truncated: false };
  }
  return {
    subtitle: `${characters.slice(0, EDGE_CHARS).join('')}\n\n[字幕中间部分已截断]\n\n${characters.slice(-EDGE_CHARS).join('')}`,
    total_chars: characters.length,
    truncated: true,
  };
}

try {
  const args = parseArguments(process.argv.slice(2));
  const { auth } = await refreshAuthIfNeeded({ forceCheck: true });
  if (!auth) throw new Error('尚未获取B站登录凭据，请先在管理页扫码');
  const parsed = await parseVideoInput(args.url, args.part);
  const info = await getVideoInfo({ ...parsed, auth });

  let tracks = await getSubtitleTracks({ aid: info.aid, cid: info.cid, auth });
  let track = chooseTrack(tracks, args.language);
  if (!track) throw new Error('B站字幕接口未返回可用字幕轨道');

  let subtitle;
  try {
    subtitle = await downloadSubtitleJson(track.url);
  } catch (error) {
    if (!isExpiredSubtitleUrl(error)) throw error;
    tracks = await getSubtitleTracks({ aid: info.aid, cid: info.cid, auth });
    track = chooseTrack(tracks, args.language);
    if (!track) throw new Error('B站字幕接口未返回可用字幕轨道');
    subtitle = await downloadSubtitleJson(track.url);
  }

  const text = toPlainText(subtitle?.body);
  if (!text) throw new Error('B站字幕文件没有可用正文');
  jsonOut({
    title: info.title,
    bvid: info.bvid,
    part: info.page,
    language: track.language,
    source: track.source,
    ...truncateSubtitle(text),
  });
} catch (error) {
  fail(error);
  process.exitCode = 1;
}
