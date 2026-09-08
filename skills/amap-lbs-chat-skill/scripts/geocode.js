#!/usr/bin/env node
const { geocode } = require('../index');

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

function clean(g) {
  return Object.fromEntries(Object.entries({
    formatted_address: g.formatted_address,
    location: g.location,
    level: g.level,
    province: g.province,
    city: Array.isArray(g.city) ? undefined : g.city,
    district: g.district,
    adcode: g.adcode,
  }).filter(([,v]) => v !== undefined && v !== '' && !(Array.isArray(v) && v.length === 0)));
}

async function main() {
  const args = parseArgs();
  if (!args.address) throw new Error('缺少 --address');
  const result = await geocode({ address: args.address, city: args.city });
  const matches = (result.geocodes || []).slice(0, 3).map(clean);
  console.log(JSON.stringify({ matches }));
}

main().catch(err => {
  console.error(JSON.stringify({ error: err.message }));
  process.exit(1);
});
