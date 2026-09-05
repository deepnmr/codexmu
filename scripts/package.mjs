import { accessSync, chmodSync, constants, copyFileSync, mkdirSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

process.chdir(fileURLToPath(new URL('..', import.meta.url)));
const targets = {
  'aarch64-apple-darwin': 'darwin-arm64',
  'x86_64-apple-darwin': 'darwin-x64',
  'aarch64-unknown-linux-musl': 'linux-arm64',
  'x86_64-unknown-linux-musl': 'linux-x64',
};
const host = `${process.platform}-${process.arch}`;
const binary = platform => `vendor/${platform}/codexmu`;

try {
  const pkg = JSON.parse(readFileSync('package.json', 'utf8'));
  const cargoVersion = readFileSync('Cargo.toml', 'utf8').match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (pkg.version !== cargoVersion) throw new Error('package.json and Cargo.toml versions must match');
  const [mode, target] = process.argv.slice(2);
  if (mode === 'stage') {
    const platform = target ? targets[target] : host;
    if (!Object.values(targets).includes(platform)) throw new Error(`unsupported target: ${target ?? host}`);
    mkdirSync(`vendor/${platform}`, { recursive: true });
    copyFileSync(target ? `target/${target}/release/codexmu` : 'target/release/codexmu', binary(platform));
    chmodSync(binary(platform), 0o755);
  } else if (!['check', 'check-all'].includes(mode)) {
    throw new Error('usage: node scripts/package.mjs stage [rust-target] | check | check-all');
  }
  const platforms = mode === 'check-all' ? Object.values(targets) : [target ? targets[target] : host];
  for (const platform of platforms) {
    accessSync(binary(platform), constants.X_OK);
  }
  const version = execFileSync(binary(host), ['--version'], { encoding: 'utf8' }).trim();
  if (version !== `codexmu ${pkg.version}`) throw new Error(`stale native binary: ${version}`);
  console.log(`Ready: codexmu ${pkg.version} (${platforms.join(', ')})`);
} catch (error) {
  console.error(`codexmu package: ${error.message}`);
  process.exitCode = 1;
}
