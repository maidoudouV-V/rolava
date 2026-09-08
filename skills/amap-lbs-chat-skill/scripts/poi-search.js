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
    type: value(poi.type),
    location: value(poi.location),
    amap_url: amapMarkerUrl(poi),
    distance_m: value(poi.distance) !== undefined ? Number(poi.distance) : undefined,
    rating: value(b.rating) !== undefined ? Number(b.rating) : undefined,
    cost_per_person: value(b.cost) !== undefined ? Number(b.cost) : undefined,
    open_today: value(b.opentime_today),
    open_week: value(b.opentime_week),
    tags: value(b.tag),
    business_area: value(b.business_area),
    parking_type: value(b.parking_type),
    district: value(poi.adname),
  };
  return Object.fromEntries(Object.entries(out).filter(([, v]) => v !== undefined && !Number.isNaN(v)));
}

async function main() {
  const args = parseArgs();
  if (!args.keywords && !args.types) {
    throw new Error('至少提供 --keywords 或 --types');
  }

  const limit = integerArg('--limit', args.limit || args.offset, 10, 1, 25);
  const page = integerArg('--page', args.page, 1, 1);
  const params = {
    keywords: args.keywords || '',
    types: args.types || '',
    city: args.city || '',
    cityLimit: args.cityLimit === 'true',
    page,
    limit,
    sort: args.sort || 'distance',
    showFields: 'business',
  };
  if (args.location) params.location = args.location;
  if (args.radius !== undefined) {
    params.radius = integerArg('--radius', args.radius, 5000, 0, 50000);
  }

  const result = await searchPOI(params);
  const places = (result.pois || []).slice(0, limit).map(normalizePOI);
  const payload = {
    count: Number(result.count || places.length),
    page: params.page,
    places,
  };

  console.log(JSON.stringify(payload));
}

main().catch(err => {
  console.error(JSON.stringify({ error: err.message }));
  process.exit(1);
});
