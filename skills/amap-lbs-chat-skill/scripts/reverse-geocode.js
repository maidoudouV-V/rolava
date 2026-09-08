#!/usr/bin/env node
const { reverseGeocode } = require('../index');

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

function clean(regeocode = {}) {
  const component = regeocode.addressComponent || {};
  const streetNumber = component.streetNumber || {};
  const neighborhood = component.neighborhood || {};
  const building = component.building || {};

  const out = {
    formatted_address: value(regeocode.formatted_address),
    country: value(component.country),
    province: value(component.province),
    city: value(component.city),
    district: value(component.district),
    township: value(component.township),
    neighborhood: value(neighborhood.name),
    building: value(building.name),
    street: value(streetNumber.street),
    number: value(streetNumber.number),
    adcode: value(component.adcode),
    citycode: value(component.citycode),
  };
  return Object.fromEntries(Object.entries(out).filter(([, v]) => v !== undefined));
}

async function main() {
  const args = parseArgs();
  if (!args.location) throw new Error('缺少 --location');

  const result = await reverseGeocode({ location: args.location });
  const address = clean(result.regeocode);
  if (!address.formatted_address) throw new Error('未找到该坐标对应的地址');
  console.log(JSON.stringify(address));
}

main().catch(err => {
  console.error(JSON.stringify({ error: err.message }));
  process.exit(1);
});
