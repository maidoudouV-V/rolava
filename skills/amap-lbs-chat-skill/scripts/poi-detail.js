#!/usr/bin/env node
const { getPOIDetail } = require('../index');
const { normalizePOI } = require('./poi-search');

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

function formatDetail(result) {
  const places = (result.pois || []).map(normalizePOI);
  return { count: places.length, places };
}

async function main() {
  const args = parseArgs();
  if (!args.id) throw new Error('缺少 --id');
  const result = await getPOIDetail({ id: args.id, showFields: 'business' });
  console.log(JSON.stringify(formatDetail(result)));
}

if (require.main === module) {
  main().catch(err => {
    console.error(JSON.stringify({ error: err.message }));
    process.exit(1);
  });
}

module.exports = { formatDetail };
