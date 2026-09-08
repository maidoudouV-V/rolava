#!/usr/bin/env node
const { walkingRoute, drivingRoute, transitRoute } = require('../index');

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

function num(v) {
  if (v === undefined || v === null || v === '') return undefined;
  const n = Number(v);
  return Number.isFinite(n) ? n : undefined;
}

function integerArg(name, v, fallback, min, max, allowed) {
  if (v === undefined || v === null || v === '') return fallback;
  const n = Number(v);
  const valid = Number.isInteger(n)
    && (min === undefined || n >= min)
    && (max === undefined || n <= max)
    && (!allowed || allowed.includes(n));
  if (!valid) {
    const range = allowed ? allowed.join('/') : `${min}-${max}`;
    throw new Error(`${name} 必须是 ${range} 范围内的整数`);
  }
  return n;
}

function amapRouteUrl(type, args) {
  const modes = {
    walking: 'walk',
    driving: 'car',
    transfer: 'bus',
  };
  const params = new URLSearchParams({
    from: args.origin,
    to: args.destination,
    mode: modes[type],
    src: 'amap-lbs-chat-skill',
    callnative: '0',
  });

  if (type === 'driving' && args.waypoints) {
    const waypoints = args.waypoints.split(';').filter(Boolean);
    if (waypoints.length === 1) params.set('via', waypoints[0]);
  }
  return `https://uri.amap.com/navigation?${params.toString()}`;
}

function compactSteps(steps = [], maxSteps = 10) {
  const cleaned = steps.map(s => ({
    instruction: s.instruction || undefined,
    road: s.road || s.road_name || undefined,
    distance_m: num(s.distance ?? s.step_distance),
    duration_s: num(s.duration),
    action: s.action || undefined,
  })).filter(s => s.instruction || s.road);

  if (cleaned.length <= maxSteps) return { steps: cleaned, omitted: 0 };
  const headCount = Math.max(maxSteps - 2, 1);
  return {
    steps: [...cleaned.slice(0, headCount), ...cleaned.slice(-2)],
    omitted: cleaned.length - maxSteps,
  };
}

function routePayload(mode, path, extras = {}) {
  const packed = compactSteps(path.steps || [], extras.maxSteps || 10);
  const payload = {
    mode,
    alternatives: extras.alternatives,
    distance_m: num(path.distance),
    duration_s: num(path.duration),
    steps: packed.steps,
  };
  if (packed.omitted) payload.omitted_steps = packed.omitted;
  if (extras.tolls !== undefined) payload.tolls_yuan = num(extras.tolls) || 0;
  if (extras.trafficLights !== undefined) payload.traffic_lights = num(extras.trafficLights) || 0;
  if (extras.restriction !== undefined) payload.restriction = num(extras.restriction);
  return payload;
}

function compactTransit(transit, maxSegments = 10) {
  const segments = [];
  for (const seg of transit.segments || []) {
    const walking = seg.walking || {};
    if (num(walking.distance) > 0) {
      segments.push({ mode: 'walking', distance_m: num(walking.distance), duration_s: num(walking.duration) });
    }

    const buslines = Array.isArray(seg.bus?.buslines) ? seg.bus.buslines : [];
    if (buslines.length) {
      const line = buslines[0];
      const alternativeLines = [...new Set(buslines.slice(1).map(item => item.name).filter(name => name && name !== line.name))];
      const item = {
        mode: line.type && String(line.type).includes('地铁') ? 'subway' : 'transit',
        line: line.name || undefined,
        alternative_lines: alternativeLines.length ? alternativeLines : undefined,
        from: line.departure_stop?.name || undefined,
        to: line.arrival_stop?.name || undefined,
        via_stops: num(line.via_num),
        distance_m: num(line.distance),
        duration_s: num(line.duration),
        entrance: seg.entrance?.name || undefined,
        exit: seg.exit?.name || undefined,
      };
      segments.push(item);
    }

    const railway = seg.railway && !Array.isArray(seg.railway) ? seg.railway : {};
    if (railway.name || railway.trip || railway.departure_stop || railway.arrival_stop) {
      segments.push({
        mode: 'railway',
        line: railway.name || undefined,
        trip: railway.trip || undefined,
        from: railway.departure_stop?.name || undefined,
        to: railway.arrival_stop?.name || undefined,
        departure_time: railway.departure_stop?.time || undefined,
        arrival_time: railway.arrival_stop?.time || undefined,
        distance_m: num(railway.distance),
      });
    }
  }

  if (segments.length <= maxSegments) return { segments, omitted: 0 };
  const headCount = Math.max(maxSegments - 2, 1);
  return {
    segments: [...segments.slice(0, headCount), ...segments.slice(-2)],
    omitted: segments.length - maxSegments,
  };
}

async function main() {
  const args = parseArgs();
  const type = args.type;
  if (!type || !args.origin || !args.destination) {
    throw new Error('需要 --type、--origin、--destination；type 支持 walking/driving/transfer');
  }
  const maxSteps = integerArg('--maxSteps', args.maxSteps, 10, 3, 20);
  let payload;

  if (type === 'walking') {
    const result = await walkingRoute({ origin: args.origin, destination: args.destination });
    const paths = result.route?.paths || [];
    const path = paths[0];
    if (!path) throw new Error('未找到步行路线');
    payload = routePayload('walking', path, { maxSteps, alternatives: paths.length });
  } else if (type === 'driving') {
    const result = await drivingRoute({
      origin: args.origin,
      destination: args.destination,
      waypoints: args.waypoints,
      strategy: integerArg('--strategy', args.strategy, undefined, 0, 20),
    });
    const paths = result.route?.paths || [];
    const path = paths[0];
    if (!path) throw new Error('未找到驾车路线');
    payload = routePayload('driving', path, {
      maxSteps,
      alternatives: paths.length,
      tolls: path.tolls,
      trafficLights: path.traffic_lights,
      restriction: path.restriction,
    });
  } else if (type === 'transfer') {
    if (!args.city) throw new Error('公交路线还需要 --city');
    const result = await transitRoute({
      origin: args.origin,
      destination: args.destination,
      city: args.city,
      cityd: args.cityd,
      strategy: integerArg('--strategy', args.strategy, undefined, undefined, undefined, [0, 1, 2, 3, 5]),
      nightflag: args.nightflag === 'true',
    });
    const transits = result.route?.transits || [];
    if (!transits.length) throw new Error('未找到公交路线');
    const t = transits[0];
    const packed = compactTransit(t, maxSteps);
    payload = {
      mode: 'transfer',
      alternatives: transits.length,
      duration_s: num(t.duration),
      cost_yuan: num(t.cost),
      walking_distance_m: num(t.walking_distance),
      segments: packed.segments,
    };
    if (packed.omitted) payload.omitted_segments = packed.omitted;
  } else {
    throw new Error(`不支持的路线类型: ${type}`);
  }

  payload.amap_url = amapRouteUrl(type, args);

  // 故意不输出 polyline/tmcs/完整几何点；网页链接使用高德官方 URI API 生成。
  console.log(JSON.stringify(payload));
}

main().catch(err => {
  console.error(JSON.stringify({ error: err.message }));
  process.exit(1);
});
