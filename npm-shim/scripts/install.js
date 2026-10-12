#!/usr/bin/env node
// postinstall: fetch and verify the platform binary so the `zoanthrope`
// bin works immediately after `npm install`.
'use strict';

const { ensureBinary } = require('./runtime');

if (process.env.CI && !process.env.ZOANTHROPE_FORCE_INSTALL) {
  process.stderr.write('Skipping zoanthrope download in CI; set ZOANTHROPE_FORCE_INSTALL=1 to test installation.\n');
} else if (process.env.npm_config_ignore_scripts === 'true') {
  process.stderr.write('Skipping zoanthrope download because npm scripts are disabled.\n');
} else {
  ensureBinary().catch((error) => {
    process.stderr.write(`zoanthrope installation failed: ${error.message}\n`);
    process.exitCode = 1;
  });
}
