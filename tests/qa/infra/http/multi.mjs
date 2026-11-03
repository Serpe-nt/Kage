#!/usr/bin/env node
// QA http topic: the round's responder on two ports at once (8091 and 8092),
// each an independent copy (program 8092 by POSTing to :8092/__set).
// Wired in by /scripts/qa/infra/http/ports.yaml as the responder's command.
import { spawn } from 'node:child_process';
const kids = ['8091', '8092'].map((port) =>
  spawn(process.execPath, ['/qa/responder.mjs', '--port', port], { stdio: 'inherit' }),
);
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => { kids.forEach((k) => k.kill()); process.exit(0); });
