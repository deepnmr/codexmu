// Run with npm test. No installed Codex, credentials, or network needed.
import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const root = realpathSync(mkdtempSync(join(tmpdir(), 'codexmu-npm-')));
try {
  mkdirSync(join(root, 'bin'));
  const launcher = join(root, 'bin', 'codexmu.mjs');
  copyFileSync(new URL('../bin/codexmu.mjs', import.meta.url), launcher);
  let result = spawnSync(process.execPath, [launcher], { encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /missing executable/);

  const nativeDir = join(root, 'vendor', `${process.platform}-${process.arch}`);
  mkdirSync(nativeDir, { recursive: true });
  const native = join(nativeDir, 'codexmu');
  writeFileSync(native, `#!/usr/bin/env node
console.log(JSON.stringify({ args: process.argv.slice(2), cwd: process.cwd(), env: process.env.CODEXMU_NPM_TEST, pid: process.pid }));
if (process.argv.includes('--wait')) setInterval(() => {}, 1000);
else process.exit(17);
`);
  chmodSync(native, 0o755);
  const args = ['run', '--', '공백 있는 프롬프트', '$(echo untouched)', ''];
  result = spawnSync(process.execPath, [launcher, ...args], {
    cwd: root, env: { ...process.env, CODEXMU_NPM_TEST: 'preserved' }, encoding: 'utf8',
  });
  assert.equal(result.status, 17, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout), { args, cwd: root, env: 'preserved', pid: result.pid });

  const child = spawn(process.execPath, [launcher, '--wait'], { stdio: ['ignore', 'pipe', 'pipe'] });
  const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
  try {
    const exit = once(child, 'exit');
    const [output] = await Promise.race([
      once(child.stdout, 'data'),
      exit.then(() => { throw new Error('launcher exited before native output'); }),
    ]);
    assert.equal(JSON.parse(output).pid, child.pid);
    child.kill('SIGTERM');
    assert.deepEqual(await exit, [null, 'SIGTERM']);
  } finally {
    clearTimeout(timer);
    child.kill('SIGKILL');
  }
  console.log('PASS: npm launcher preserves arguments, environment, cwd, PID, exit status, and signals');
} finally {
  rmSync(root, { recursive: true, force: true });
}
