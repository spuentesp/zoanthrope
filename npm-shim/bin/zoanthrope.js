#!/usr/bin/env node
// Launcher: ensures the platform binary is present (downloaded from
// the GitHub release with SHA256 verification, or via ZOANTHROPE_BIN)
// and forwards argv to it.
'use strict';

const { spawn } = require('child_process');
const { ensureBinary } = require('../scripts/runtime');

ensureBinary()
  .then((binary) => {
    const child = spawn(binary, process.argv.slice(2), {
      cwd: process.cwd(),
      stdio: 'inherit',
    });
    child.on('error', (error) => {
      process.stderr.write(`zoanthrope: failed to start ${binary}: ${error.message}\n`);
      process.exitCode = 1;
    });
    child.on('exit', (code, signal) => {
      if (signal) {
        process.kill(process.pid, signal);
        return;
      }
      process.exitCode = code == null ? 1 : code;
    });
  })
  .catch((error) => {
    process.stderr.write(`zoanthrope: ${error.message}\n`);
    process.exitCode = 1;
  });
