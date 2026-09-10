#!/usr/bin/env node
const { searchPOI } = require('../index');

function parseArgs() {
  const args = {};
  for (const arg of process.argv.slice(2)) {
    if (!arg.startsWith('--')) continue;
    const pos = arg.indexOf('=');
    if (pos < 0) args[arg.slice(2)] = 'true';
    else args[arg.slice(2, pos)] = arg.slice(pos + 1);
  }
  return args;
}

function value(v) {
  if (Array.isArray(v) && v.length === 0) return undefined;
  if (v === '' || v === null || v === undefined) return undefined;
  return v;
}

function integerArg(name, raw, fallback, min, max) {
  if (raw === undefined || raw === '') return fallback;
  const number = Number(raw);
  if (!Number.isInteger(number) || number < min || (max !== undefined && number > max)) {
    const range = max === undefined ? `不小于 ${min}` : `${min}-${max}`;
    throw new Error(`${name} 必须是${range}范围内的整数`);
  }
  return number;
}

function amapMarkerUrl(poi) {
  const params = new URLSearchParams({
    src: 'amap-lbs-chat-skill',
    callnative: '0',
  });
  if (value(poi.id)) {
    params.set('poiid', poi.id);
  } else if (value(poi.location)) {
    params.set('position', poi.location);
    if (value(poi.name)) params.set('name', poi.name);
    params.set('coordinate', 'gaode');
  } else {
    return undefined;
  }
  return `https://uri.amap.com/marker?${params.toString()}`;
}

function normalizePOI(poi) {
  const b = poi.business && !Array.isArray(poi.business) ? poi.business : {};
  const out = {
    id: value(poi.id),
    name: value(poi.name),
    address: value(poi.address),
    location: value(poi.location),
    amap_url: amapMarkerUrl(poi),
    distance_m: value(poi.distance) !== undefined ? Number(poi.distance) : undefined,
    rating: value(b.rating) !== undefined ? Number(b.rating) : undefined,
    cost_per_person: value(b.cost) !== undefined ? Number(b.cost) : undefined,
    keytag: value(b.keytag),
    opentime_week: value(b.opentime_week),
  };
  return Object.fromEntries(Object.entries(out).filter(([, v]) => v !== undefined && !Number.isNaN(v)));
}

function compactPOI(poi) {
  const out = {
    id: poi.id,
    name: poi.name,
    distance_m: poi.distance_m,
    rating: poi.rating,
  };
  return Object.fromEntries(Object.entries(out).filter(([, v]) => v !== undefined));
}

async function collectPOIs(search, params, limit, now = Date.now) {
  const startedAt = now();
  const pois = [];
  for (let page = 1; pois.length < limit; page += 1) {
    const remainingMs = 20000 - (now() - startedAt);
    if (remainingMs <= 0) break;

    let result;
    try {
      result = await search({
        ...params,
        page,
        limit: 25,
        timeoutMs: Math.min(15000, remainingMs),
      });
    } catch (error) {
      const timedOut = error?.code === 'ECONNABORTED' || error?.code === 'ETIMEDOUT';
      if (timedOut && now() - startedAt >= 20000) break;
      throw error;
    }

    const currentPage = result.pois || [];
    pois.push(...currentPage.slice(0, limit - pois.length));
    if (currentPage.length < 25 || now() - startedAt >= 20000) break;
  }
  return pois;
}

function formatPOIs(pois) {
  const normalized = pois.map(normalizePOI);
  return normalized.length <= 10 ? normalized : normalized.map(compactPOI);
}

async function main() {
  const args = parseArgs();
  if (!args.keywords && !args.types) {
    throw new Error('至少提供 --keywords 或 --types');
  }
  if (args.page !== undefined) {
    throw new Error('不再支持 --page；请使用 --limit=1-200');
  }

  const limit = integerArg('--limit', args.limit, 10, 1, 200);
  const params = {
    keywords: args.keywords || '',
    types: args.types || '',
    city: args.city || '',
    cityLimit: args.cityLimit === 'true',
    sort: args.sort || 'distance',
    showFields: 'business',
  };
  if (args.location) params.location = args.location;
  if (args.radius !== undefined) {
    params.radius = integerArg('--radius', args.radius, 5000, 0, 50000);
  }

  const places = formatPOIs(await collectPOIs(searchPOI, params, limit));
  const payload = {
    count: places.length,
    places,
  };

  console.log(JSON.stringify(payload));
}

if (require.main === module) {
  main().catch(err => {
    console.error(JSON.stringify({ error: err.message }));
    process.exit(1);
  });
}

module.exports = { collectPOIs, compactPOI, formatPOIs, normalizePOI };
