const axios = require('axios');

const BASE = 'https://restapi.amap.com';

function integerParam(value, { name, fallback, min, max, allowed }) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  const inRange = Number.isInteger(number)
    && (min === undefined || number >= min)
    && (max === undefined || number <= max)
    && (!allowed || allowed.includes(number));
  if (!inRange) {
    const range = allowed
      ? allowed.join('/')
      : max === undefined ? `不小于 ${min}` : `${min}-${max}`;
    throw new Error(`${name} 必须是${range}范围内的整数`);
  }
  return number;
}

function getKey() {
  const key = typeof process.env.AMAP_WEBSERVICE_KEY === 'string'
    ? process.env.AMAP_WEBSERVICE_KEY.trim()
    : '';
  if (!key) {
    throw new Error('缺少高德 Web Service Key：请设置 AMAP_WEBSERVICE_KEY 环境变量');
  }
  return key;
}

async function amapGet(path, params = {}, timeoutMs = 15000) {
  const response = await axios.get(`${BASE}${path}`, {
    params: { key: getKey(), ...params },
    timeout: timeoutMs,
  });
  const data = response.data;
  if (!data) throw new Error('高德 API 返回空响应');
  if (data.status !== undefined && String(data.status) !== '1') {
    throw new Error(`高德 API 请求失败：${data.info || data.infocode || 'unknown error'}`);
  }
  return data;
}

/**
 * POI 2.0 搜索。
 * 有 location 时使用周边搜索，否则使用关键词搜索。
 * 默认只请求 business 扩展字段，不请求 photos/polyline 等大字段。
 */
async function searchPOI(params = {}) {
  const nearby = Boolean(params.location);
  const path = nearby ? '/v5/place/around' : '/v5/place/text';
  const pageSize = integerParam(params.limit ?? params.offset, {
    name: 'limit', fallback: 10, min: 1, max: 25,
  });
  const pageNum = integerParam(params.page, {
    name: 'page', fallback: 1, min: 1,
  });
  const request = {
    keywords: params.keywords || undefined,
    types: params.types || undefined,
    region: params.city || params.region || undefined,
    city_limit: params.cityLimit === true ? 'true' : 'false',
    show_fields: params.showFields || 'business',
    page_num: pageNum,
    page_size: pageSize,
  };

  if (nearby) {
    request.location = params.location;
    request.radius = integerParam(params.radius, {
      name: 'radius', fallback: 5000, min: 0, max: 50000,
    });
    request.sortrule = params.sort === 'weight' ? 'weight' : 'distance';
  }

  const result = await amapGet(path, request, params.timeoutMs);
  return {
    ...result,
    pois: (result.pois || []).map((poi) => {
      const cleaned = { ...poi };
      delete cleaned.typecode;
      delete cleaned.tel;
      if (cleaned.business && !Array.isArray(cleaned.business)) {
        cleaned.business = { ...cleaned.business };
        delete cleaned.business.tel;
      }
      return cleaned;
    }),
  };
}

async function getPOIDetail(params = {}) {
  const rawId = typeof params.id === 'string' ? params.id.trim() : '';
  if (!rawId) throw new Error('getPOIDetail 缺少 id');
  const ids = rawId.split('|').map(id => id.trim());
  if (ids.some(id => !id)) throw new Error('getPOIDetail 的 id 不能包含空值');
  if (ids.length > 10) throw new Error('getPOIDetail 的 id 最多支持 10 个');
  return amapGet('/v5/place/detail', {
    id: ids.join('|'),
    show_fields: params.showFields || 'business',
  });
}

async function geocode(params = {}) {
  if (!params.address) throw new Error('geocode 缺少 address');
  return amapGet('/v3/geocode/geo', {
    address: params.address,
    city: params.city || undefined,
    output: 'JSON',
  });
}

async function reverseGeocode(params = {}) {
  if (!params.location) throw new Error('reverseGeocode 缺少 location');
  return amapGet('/v3/geocode/regeo', {
    location: params.location,
    extensions: 'base',
    output: 'JSON',
  });
}

async function walkingRoute(params = {}) {
  return amapGet('/v3/direction/walking', {
    origin: params.origin,
    destination: params.destination,
    origin_id: params.originId || undefined,
    destination_id: params.destinationId || undefined,
    output: 'JSON',
  });
}

async function drivingRoute(params = {}) {
  const request = {
    origin: params.origin,
    destination: params.destination,
    strategy: integerParam(params.strategy, {
      name: 'driving strategy', fallback: 10, min: 0, max: 20,
    }),
    extensions: 'base',
    output: 'JSON',
  };
  if (params.waypoints) request.waypoints = params.waypoints;
  return amapGet('/v3/direction/driving', request);
}

async function transitRoute(params = {}) {
  if (!params.city) throw new Error('公交路线需要 city');
  return amapGet('/v3/direction/transit/integrated', {
    origin: params.origin,
    destination: params.destination,
    city: params.city,
    cityd: params.cityd || undefined,
    strategy: integerParam(params.strategy, {
      name: 'transit strategy', fallback: 0, allowed: [0, 1, 2, 3, 5],
    }),
    nightflag: params.nightflag ? 1 : 0,
    output: 'JSON',
  });
}

module.exports = {
  searchPOI,
  getPOIDetail,
  geocode,
  reverseGeocode,
  walkingRoute,
  drivingRoute,
  transitRoute,
};
