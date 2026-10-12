'use strict';

// Download-and-verify installer for the zoanthrope binary.
// Node builtins only (https, crypto, fs, child_process) — no runtime
// dependencies. Release assets are `zoanthrope-{target}.tar.gz` with
// the bare `zoanthrope` binary at the archive root; `SHA256SUMS` in the
// same release carries one checksum line per tarball.

const crypto = require('crypto');
const fs = require('fs');
const https = require('https');
const os = require('os');
const path = require('path');
const { execFileSync, spawnSync } = require('child_process');

const REPOSITORY = 'spuentesp/zoanthrope';

const BUILD_FROM_SOURCE =
  'Build from source instead: git clone https://github.com/spuentesp/zoanthrope.git && ' +
  'cargo install --path zoanthrope  (or set ZOANTHROPE_BIN to an existing binary).';

function targetFor(platform = process.platform, arch = process.arch) {
  const targets = {
    'darwin-arm64': 'aarch64-apple-darwin',
    'darwin-x64': 'x86_64-apple-darwin',
    'linux-arm64': 'aarch64-unknown-linux-gnu',
    'linux-x64': 'x86_64-unknown-linux-gnu',
  };
  const target = targets[`${platform}-${arch}`];
  if (!target) throw new Error(`No prebuilt zoanthrope binary for ${platform}-${arch}. ${BUILD_FROM_SOURCE}`);
  return target;
}

function normalizeVersion(value) {
  const version = String(value || '').replace(/^v/, '');
  if (!/^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`Invalid ZOANTHROPE_VERSION: ${value}`);
  }
  return version;
}

function selectedVersion(env = process.env) {
  return normalizeVersion(env.ZOANTHROPE_VERSION || require('../package.json').version);
}

function cacheRoot(env = process.env, platform = process.platform) {
  if (env.ZOANTHROPE_CACHE_DIR) return path.resolve(env.ZOANTHROPE_CACHE_DIR);
  if (platform === 'darwin') return path.join(os.homedir(), 'Library', 'Caches', 'zoanthrope');
  return path.join(env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache'), 'zoanthrope');
}

function assetName(target) {
  return `zoanthrope-${target}.tar.gz`;
}

function binaryPath(version, target, env = process.env, platform = process.platform) {
  return path.join(cacheRoot(env, platform), normalizeVersion(version), target, platform === 'win32' ? 'zoanthrope.exe' : 'zoanthrope');
}

function releaseUrl(version, name) {
  return `https://github.com/${REPOSITORY}/releases/download/v${normalizeVersion(version)}/${name}`;
}

function parseChecksum(text, expectedAsset) {
  for (const line of text.split(/\r?\n/)) {
    const match = line.trim().match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/);
    if (match && match[2] === expectedAsset) return match[1].toLowerCase();
  }
  throw new Error(`SHA256SUMS has no checksum for ${expectedAsset}`);
}

function sha256(file) {
  const hash = crypto.createHash('sha256');
  hash.update(fs.readFileSync(file));
  return hash.digest('hex');
}

function verifyArchive(file, expected) {
  const actual = sha256(file);
  if (actual !== expected.toLowerCase()) {
    throw new Error(`Checksum mismatch for ${path.basename(file)}: expected ${expected}, got ${actual}`);
  }
}

function verifyBinary(file, version) {
  const result = spawnSync(file, ['--version'], { encoding: 'utf8', timeout: 10_000 });
  if (result.error) throw new Error(`Cannot run zoanthrope binary: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`zoanthrope binary exited ${result.status} during version check`);
  const reported = String(result.stdout).trim();
  const expected = `zoanthrope ${normalizeVersion(version)}`;
  if (reported !== expected) {
    throw new Error(`Downloaded binary reports ${JSON.stringify(reported)}, expected ${JSON.stringify(expected)}`);
  }
}

function download(url, destination, redirects = 5) {
  return new Promise((resolve, reject) => {
    const request = https.get(url, { headers: { 'User-Agent': '@spuentesp/zoanthrope' } }, (response) => {
      if ([301, 302, 303, 307, 308].includes(response.statusCode) && response.headers.location) {
        response.resume();
        if (redirects === 0) return reject(new Error(`Too many redirects downloading ${url}`));
        return download(response.headers.location, destination, redirects - 1).then(resolve, reject);
      }
      if (response.statusCode !== 200) {
        response.resume();
        return reject(new Error(`HTTP ${response.statusCode} downloading ${url}`));
      }
      const output = fs.createWriteStream(destination, { flags: 'wx' });
      let received = 0;
      const total = Number(response.headers['content-length']) || 0;
      response.on('data', (chunk) => {
        received += chunk.length;
        if (process.stderr.isTTY && total) {
          process.stderr.write(`\rDownloading ${path.basename(destination)} ${Math.floor(received * 100 / total)}%`);
        }
      });
      response.pipe(output);
      output.on('finish', () => output.close(() => {
        if (process.stderr.isTTY && total) process.stderr.write('\n');
        resolve();
      }));
      output.on('error', reject);
      response.on('error', reject);
    });
    request.setTimeout(30_000, () => request.destroy(new Error(`Download timed out: ${url}`)));
    request.on('error', reject);
  });
}

async function ensureBinary(options = {}) {
  const env = options.env || process.env;
  const platform = options.platform || process.platform;
  const arch = options.arch || process.arch;
  // Explicit override wins: no download, no version gate (the caller
  // owns that binary). Keeps local development and CI smoke tests easy.
  if (env.ZOANTHROPE_BIN) return path.resolve(env.ZOANTHROPE_BIN);
  const version = normalizeVersion(options.version || selectedVersion(env));
  const target = targetFor(platform, arch);
  const finalBinary = binaryPath(version, target, env, platform);
  if (fs.existsSync(finalBinary)) {
    verifyBinary(finalBinary, version);
    return finalBinary;
  }
  const directory = path.dirname(finalBinary);
  fs.mkdirSync(directory, { recursive: true });
  const temporary = path.join(directory, `.install-${process.pid}-${crypto.randomBytes(6).toString('hex')}`);
  fs.mkdirSync(temporary);
  const asset = assetName(target);
  const archive = path.join(temporary, asset);
  const sums = path.join(temporary, 'SHA256SUMS');
  try {
    const fetch = options.download || download;
    await fetch(releaseUrl(version, 'SHA256SUMS'), sums);
    await fetch(releaseUrl(version, asset), archive);
    verifyArchive(archive, parseChecksum(fs.readFileSync(sums, 'utf8'), asset));
    execFileSync('tar', ['xzf', archive, '-C', temporary], { stdio: ['ignore', 'ignore', 'inherit'] });
    const extracted = path.join(temporary, 'zoanthrope');
    if (!fs.existsSync(extracted)) throw new Error(`${asset} does not contain the zoanthrope executable at its root`);
    if (platform !== 'win32') fs.chmodSync(extracted, 0o755);
    verifyBinary(extracted, version);
    try {
      fs.renameSync(extracted, finalBinary);
    } catch (error) {
      // A concurrent npm install of the same version may have won the
      // race; the versioned cache directory keeps releases apart.
      if (!fs.existsSync(finalBinary)) throw error;
      verifyBinary(finalBinary, version);
    }
    return finalBinary;
  } catch (error) {
    if (error && /^HTTP 404/.test(error.message || '')) {
      throw new Error(
        `No release asset for zoanthrope v${version} (${target}). ${BUILD_FROM_SOURCE}`
      );
    }
    throw error;
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}

module.exports = {
  BUILD_FROM_SOURCE,
  assetName,
  binaryPath,
  cacheRoot,
  ensureBinary,
  normalizeVersion,
  parseChecksum,
  releaseUrl,
  selectedVersion,
  sha256,
  targetFor,
  verifyArchive,
  verifyBinary,
};
