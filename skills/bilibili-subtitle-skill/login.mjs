#!/usr/bin/env node
import { startQrLogin, pollQrLogin, refreshAuthIfNeeded } from './lib/auth.mjs';
import { jsonOut, fail } from './lib/core.mjs';

const [action] = process.argv.slice(2);

try {
  if (action === 'start') {
    jsonOut(await startQrLogin());
  } else if (action === 'poll') {
    jsonOut(await pollQrLogin());
  } else if (action === 'refresh') {
    const { auth, refreshed } = await refreshAuthIfNeeded({ forceCheck: true });
    jsonOut({ status: auth ? 'ready' : 'missing', refreshed });
  } else {
    throw new Error('用法：login.mjs start | poll | refresh');
  }
} catch (error) {
  fail(error);
  process.exitCode = 1;
}
