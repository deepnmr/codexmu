#!/usr/bin/env node
import { accessSync, constants } from 'node:fs';
import { fileURLToPath } from 'node:url';

try {
  if (Number(process.versions.node.split('.')[0]) < 24 || !process.execve) {
    throw new Error('Node.js 24 or newer is required');
  }
  const platform = `${process.platform}-${process.arch}`;
  if (!['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64'].includes(platform)) {
    throw new Error(`unsupported platform: ${platform} (macOS/Linux, ARM64/x64 required)`);
  }
  const binary = fileURLToPath(new URL(`../vendor/${platform}/codexmu`, import.meta.url));
  try {
    accessSync(binary, constants.X_OK);
  } catch {
    throw new Error(`missing executable for ${platform}; reinstall a package built for this platform`);
  }
  // Replace Node so the native CLI owns its PID, terminal, and signals.
  process.execve(binary, [binary, ...process.argv.slice(2)], process.env);
} catch (error) {
  console.error(`codexmu: ${error.message}`);
  process.exitCode = 1;
}
