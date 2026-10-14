'use strict';

// Pure unit tests for the installer's platform mapping — no download,
// no OS dependency. Run with: node --test scripts/  (or `npm test`).

const test = require('node:test');
const assert = require('node:assert');
const path = require('path');

const { assetName, binaryPath, targetFor, tarExecutable } = require('./runtime');

test('targetFor maps win32-x64 to the MSVC target', () => {
  assert.strictEqual(targetFor('win32', 'x64'), 'x86_64-pc-windows-msvc');
});

test('targetFor maps win32-arm64 to the MSVC target', () => {
  assert.strictEqual(targetFor('win32', 'arm64'), 'aarch64-pc-windows-msvc');
});

test('assetName resolves the windows x86_64 tarball the release ships', () => {
  assert.strictEqual(
    assetName(targetFor('win32', 'x64')),
    'zoanthrope-x86_64-pc-windows-msvc.tar.gz'
  );
});

test('assetName resolves the windows arm64 tarball name', () => {
  assert.strictEqual(
    assetName(targetFor('win32', 'arm64')),
    'zoanthrope-aarch64-pc-windows-msvc.tar.gz'
  );
});

test('binaryPath uses zoanthrope.exe on win32', () => {
  const target = targetFor('win32', 'x64');
  const file = binaryPath('0.1.5', target, {}, 'win32');
  assert.strictEqual(path.basename(file), 'zoanthrope.exe');
  assert.ok(file.includes(target));
});

test('binaryPath stays extension-less on unix', () => {
  const target = targetFor('linux', 'x64');
  const file = binaryPath('0.1.5', target, {}, 'linux');
  assert.strictEqual(path.basename(file), 'zoanthrope');
});

test('targetFor keeps the pre-existing unix/darwin mappings', () => {
  assert.strictEqual(targetFor('linux', 'x64'), 'x86_64-unknown-linux-gnu');
  assert.strictEqual(targetFor('linux', 'arm64'), 'aarch64-unknown-linux-gnu');
  assert.strictEqual(targetFor('darwin', 'x64'), 'x86_64-apple-darwin');
  assert.strictEqual(targetFor('darwin', 'arm64'), 'aarch64-apple-darwin');
});

test('targetFor rejects platforms with no prebuilt binary', () => {
  assert.throws(() => targetFor('win32', 'ia32'), /No prebuilt zoanthrope binary/);
  assert.throws(() => targetFor('freebsd', 'x64'), /No prebuilt zoanthrope binary/);
});

test('Windows extraction uses System32 bsdtar, not the first tar on PATH', () => {
  const env = { SystemRoot: 'C:\\Windows' };
  assert.strictEqual(tarExecutable('win32', env, () => true), 'C:\\Windows\\System32\\tar.exe');
  assert.strictEqual(tarExecutable('win32', env, () => false), 'tar');
  assert.strictEqual(tarExecutable('win32', {}, () => true), 'tar');
  assert.strictEqual(tarExecutable('linux', env, () => true), 'tar');
});
