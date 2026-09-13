#!/usr/bin/env node

const crypto = require('node:crypto');
const fs = require('node:fs/promises');
const path = require('node:path');

const MAX_OUTPUT_IMAGES = 1;
const MAX_REQUEST_BYTES = 20 * 1024 * 1024;
const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
const MAX_RESPONSE_BYTES = 32 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 50_000;
const RANDOM_NAME_ATTEMPTS = 20;
const RANDOM_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
const PROJECT_ROOT = path.resolve(__dirname, '..', '..', '..');
const IMAGE_DIRECTORY = path.join(PROJECT_ROOT, 'data', 'images');
const RECEIVED_IMAGE_DIRECTORY = path.join(PROJECT_ROOT, 'data', 'received_images');
const VIRTUAL_IMAGE_PREFIX = '/data/images/';
const VIRTUAL_INPUT_ROOTS = [
  { prefix: VIRTUAL_IMAGE_PREFIX, directory: IMAGE_DIRECTORY },
  { prefix: '/data/received_images/', directory: RECEIVED_IMAGE_DIRECTORY },
];
const OUTPUT_LIMIT_PROMPT = '重要限制：一次只能生成 1 张图片。';

class SkillError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function parseArgs(argv) {
  let prompt;
  const imagePaths = [];

  for (const argument of argv) {
    const separator = argument.indexOf('=');
    if (!argument.startsWith('--') || separator < 3) {
      throw new SkillError('INVALID_INPUT', '参数必须使用 --name=value 格式');
    }
    const name = argument.slice(2, separator);
    const value = argument.slice(separator + 1);
    if (!value) {
      throw new SkillError('INVALID_INPUT', `--${name} 不能为空`);
    }
    if (name === 'prompt') {
      if (prompt !== undefined) {
        throw new SkillError('INVALID_INPUT', '--prompt 不能重复');
      }
      prompt = value;
    } else if (name === 'image') {
      imagePaths.push(value);
    } else {
      throw new SkillError('INVALID_INPUT', `不支持的参数：--${name}`);
    }
  }

  if (!prompt || !prompt.trim()) {
    throw new SkillError('INVALID_INPUT', '缺少 --prompt');
  }
  return { prompt: prompt.trim(), imagePaths };
}

function requiredEnvironment(name) {
  const value = process.env[name];
  if (!value || !value.trim()) {
    throw new SkillError('MISSING_CONFIG', `缺少环境变量 ${name}`);
  }
  return value.trim();
}

function loadConfig() {
  const baseUrl = requiredEnvironment('IMAGE_GENERATION_BASE_URL').replace(/\/+$/, '');
  const modelName = requiredEnvironment('IMAGE_GENERATION_MODEL_NAME')
    .replace(/^models\//, '')
    .trim();
  const apiKey = requiredEnvironment('IMAGE_GENERATION_API_KEY');

  let parsedBaseUrl;
  try {
    parsedBaseUrl = new URL(baseUrl);
  } catch {
    throw new SkillError('MISSING_CONFIG', 'IMAGE_GENERATION_BASE_URL 不是有效 URL');
  }
  if (!['http:', 'https:'].includes(parsedBaseUrl.protocol) || !modelName) {
    throw new SkillError('MISSING_CONFIG', '图片生成接口配置无效');
  }
  return { baseUrl, modelName, apiKey };
}

function isInside(root, candidate) {
  const relative = path.relative(root, candidate);
  return relative !== '' && !relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative);
}

async function resolveVirtualImagePath(virtualPath) {
  const inputRoot = VIRTUAL_INPUT_ROOTS.find(root => virtualPath.startsWith(root.prefix));
  if (virtualPath.includes('\\') || !inputRoot) {
    throw new SkillError('INVALID_PATH', '图片路径必须位于 /data/images/ 或 /data/received_images/');
  }

  const relativePath = virtualPath.slice(inputRoot.prefix.length);
  const segments = relativePath.split('/');
  if (!relativePath || segments.some(segment => !segment || segment === '.' || segment === '..')) {
    throw new SkillError('INVALID_PATH', '图片路径无效');
  }

  let imageRoot;
  try {
    imageRoot = await fs.realpath(inputRoot.directory);
  } catch {
    throw new SkillError('FILE_NOT_FOUND', `图片不存在：${virtualPath}`);
  }
  const requestedPath = path.join(imageRoot, ...segments);
  let realPath;
  try {
    realPath = await fs.realpath(requestedPath);
  } catch {
    throw new SkillError('FILE_NOT_FOUND', `图片不存在：${virtualPath}`);
  }
  if (!isInside(imageRoot, realPath)) {
    throw new SkillError('INVALID_PATH', '图片路径超出允许的图片目录');
  }

  const stat = await fs.stat(realPath);
  if (!stat.isFile()) {
    throw new SkillError('INVALID_PATH', `图片路径不是文件：${virtualPath}`);
  }
  return realPath;
}

function detectImageMimeType(bytes) {
  if (bytes.length >= 8 && bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) {
    return 'image/png';
  }
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) {
    return 'image/jpeg';
  }
  if (bytes.length >= 12 && bytes.toString('ascii', 0, 4) === 'RIFF' && bytes.toString('ascii', 8, 12) === 'WEBP') {
    return 'image/webp';
  }
  if (bytes.length >= 6 && ['GIF87a', 'GIF89a'].includes(bytes.toString('ascii', 0, 6))) {
    return 'image/gif';
  }
  if (bytes.length >= 2 && bytes.toString('ascii', 0, 2) === 'BM') {
    return 'image/bmp';
  }
  return null;
}

async function loadInputImages(imagePaths) {
  await fs.mkdir(IMAGE_DIRECTORY, { recursive: true });
  const images = [];
  let encodedBytes = 0;

  for (const virtualPath of imagePaths) {
    const realPath = await resolveVirtualImagePath(virtualPath);
    const stat = await fs.stat(realPath);
    if (encodedBytes + 4 * Math.ceil(stat.size / 3) > MAX_REQUEST_BYTES) {
      throw new SkillError('PAYLOAD_TOO_LARGE', '图片请求内容过大');
    }
    const bytes = await fs.readFile(realPath);
    encodedBytes += 4 * Math.ceil(bytes.length / 3);
    if (encodedBytes > MAX_REQUEST_BYTES) {
      throw new SkillError('PAYLOAD_TOO_LARGE', '图片请求内容过大');
    }
    const mimeType = detectImageMimeType(bytes);
    if (!mimeType) {
      throw new SkillError('UNSUPPORTED_IMAGE', `不支持的图片格式：${virtualPath}`);
    }
    images.push({ mimeType, data: bytes.toString('base64') });
  }
  return images;
}

function buildGeminiRequest(prompt, images) {
  const parts = images.map(image => ({
    inlineData: {
      mimeType: image.mimeType,
      data: image.data,
    },
  }));
  parts.push({ text: `${prompt}\n\n${OUTPUT_LIMIT_PROMPT}` });

  return {
    contents: [{ role: 'user', parts }],
    generationConfig: {
      responseModalities: ['TEXT', 'IMAGE'],
    },
  };
}

async function callGeminiGenerateContent(config, requestBody) {
  const serializedBody = JSON.stringify(requestBody);
  if (Buffer.byteLength(serializedBody) > MAX_REQUEST_BYTES) {
    throw new SkillError('PAYLOAD_TOO_LARGE', '图片请求内容过大');
  }

  const endpoint = `${config.baseUrl}/models/${encodeURIComponent(config.modelName)}:generateContent`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    const response = await fetch(endpoint, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'x-goog-api-key': config.apiKey,
      },
      body: serializedBody,
      signal: controller.signal,
    });
    if (!response.ok) {
      throw new SkillError('UPSTREAM_ERROR', `图片生成接口返回 HTTP ${response.status}`);
    }
    try {
      if (Number(response.headers.get('content-length')) > MAX_RESPONSE_BYTES) {
        throw new SkillError('PAYLOAD_TOO_LARGE', '图片生成响应过大');
      }
      const chunks = [];
      let size = 0;
      for await (const chunk of response.body) {
        size += chunk.length;
        if (size > MAX_RESPONSE_BYTES) {
          controller.abort();
          throw new SkillError('PAYLOAD_TOO_LARGE', '图片生成响应过大');
        }
        chunks.push(chunk);
      }
      return JSON.parse(Buffer.concat(chunks, size).toString('utf8'));
    } catch (error) {
      if (error instanceof SkillError) throw error;
      if (controller.signal.aborted) throw error;
      throw new SkillError('UPSTREAM_ERROR', '图片生成接口返回了无效 JSON');
    }
  } catch (error) {
    if (error instanceof SkillError) throw error;
    if (controller.signal.aborted || (error && error.name === 'AbortError')) {
      throw new SkillError('TIMEOUT', '图片生成请求超时');
    }
    throw new SkillError('UPSTREAM_ERROR', '无法连接图片生成接口');
  } finally {
    clearTimeout(timer);
  }
}

function decodeBase64Image(inlineData) {
  let mimeType = typeof inlineData?.mimeType === 'string'
    ? inlineData.mimeType.split(';', 1)[0].trim().toLowerCase()
    : '';
  if (mimeType === 'image/jpg') mimeType = 'image/jpeg';
  const encoded = typeof inlineData?.data === 'string'
    ? inlineData.data.replace(/\s/g, '')
    : '';
  if (!encoded || encoded.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(encoded)) {
    throw new SkillError('UPSTREAM_ERROR', '图片生成接口返回了无效图片数据');
  }

  const bytes = Buffer.from(encoded, 'base64');
  if (bytes.length > MAX_IMAGE_BYTES) {
    throw new SkillError('PAYLOAD_TOO_LARGE', '单张图片不能超过 20 MiB');
  }
  const detectedMimeType = detectImageMimeType(bytes);
  if (!detectedMimeType || detectedMimeType !== mimeType) {
    throw new SkillError('UPSTREAM_ERROR', '图片生成接口返回的图片格式无效');
  }
  return { bytes, mimeType };
}

function extractGeminiResult(responseBody) {
  const parts = responseBody?.candidates?.[0]?.content?.parts;
  if (!Array.isArray(parts)) {
    throw new SkillError('NO_IMAGE', '模型未返回图片');
  }

  const textParts = [];
  const images = [];
  for (const part of parts) {
    if (!part || part.thought === true) continue;
    if (typeof part.text === 'string' && part.text.trim()) {
      textParts.push(part.text.trim());
    }
    if (part.inlineData && images.length < MAX_OUTPUT_IMAGES) {
      images.push(decodeBase64Image(part.inlineData));
    }
  }

  if (images.length === 0) {
    throw new SkillError('NO_IMAGE', '模型未返回图片');
  }
  return {
    images,
    text: textParts.join('\n').trim(),
  };
}

function extensionForMimeType(mimeType) {
  return {
    'image/png': 'png',
    'image/jpeg': 'jpg',
    'image/webp': 'webp',
    'image/gif': 'gif',
    'image/bmp': 'bmp',
  }[mimeType];
}

function randomImageId() {
  let suffix = '';
  for (let index = 0; index < 8; index += 1) {
    suffix += RANDOM_ALPHABET[crypto.randomInt(RANDOM_ALPHABET.length)];
  }
  return `img_${suffix}`;
}

async function saveOneImage(image) {
  const extension = extensionForMimeType(image.mimeType);
  for (let attempt = 0; attempt < RANDOM_NAME_ATTEMPTS; attempt += 1) {
    const fileName = `${randomImageId()}.${extension}`;
    const filePath = path.join(IMAGE_DIRECTORY, fileName);
    try {
      await fs.writeFile(filePath, image.bytes, { flag: 'wx' });
      return {
        filePath,
        virtualPath: `${VIRTUAL_IMAGE_PREFIX}${fileName}`,
      };
    } catch (error) {
      if (error && error.code === 'EEXIST') continue;
      throw error;
    }
  }
  throw new SkillError('SAVE_FAILED', '生成图片文件名连续碰撞');
}

async function saveImages(images) {
  await fs.mkdir(IMAGE_DIRECTORY, { recursive: true });
  const saved = [];
  try {
    for (const image of images) {
      saved.push(await saveOneImage(image));
    }
    return saved.map(image => image.virtualPath);
  } catch (error) {
    await Promise.allSettled(saved.map(image => fs.unlink(image.filePath)));
    if (error instanceof SkillError) throw error;
    throw new SkillError('SAVE_FAILED', '保存生成图片失败');
  }
}

async function main() {
  const { prompt, imagePaths } = parseArgs(process.argv.slice(2));
  const config = loadConfig();
  const inputImages = await loadInputImages(imagePaths);
  const requestBody = buildGeminiRequest(prompt, inputImages);
  const responseBody = await callGeminiGenerateContent(config, requestBody);
  const result = extractGeminiResult(responseBody);
  const output = { paths: await saveImages(result.images) };
  if (result.text) output.text = result.text;
  process.stdout.write(JSON.stringify(output));
}

main().catch(error => {
  const skillError = error instanceof SkillError
    ? error
    : new SkillError('UPSTREAM_ERROR', '图片生成失败');
  process.stderr.write(JSON.stringify({ error: skillError.message, code: skillError.code }));
  process.exitCode = 1;
});
