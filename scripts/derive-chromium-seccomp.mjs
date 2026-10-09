#!/usr/bin/env node
// Derives deploy/seccomp/chromium.json from Docker's default seccomp profile.
//
//   curl -fsSLO https://raw.githubusercontent.com/moby/profiles/<commit>/seccomp/default.json
//   node scripts/derive-chromium-seccomp.mjs default.json <commit> > deploy/seccomp/chromium.json
//
// Why the Chromium sidecar needs its own profile (spec §19): Chrome's
// sandbox puts every renderer into its own user, PID and network namespace
// and chroots it into an empty directory. Docker's default profile refuses
// the system calls for that to a container without CAP_SYS_ADMIN, and the
// sidecar runs with every capability dropped. Without them Chrome only
// starts with --no-sandbox, which Kage never passes: the sandbox is what
// stands between a hostile page and the persona's profile directory.
//
// The profile is Docker's default with one rule added that allows `clone`,
// `unshare`, `setns` and `chroot`. Nothing else differs. `clone3` stays
// refused with ENOSYS, as in the default, so that libc falls back to
// `clone`, which the filter can inspect.
import { readFileSync } from 'node:fs';

const [source, commit] = process.argv.slice(2);
if (source === undefined || commit === undefined) {
  process.stderr.write('usage: derive-chromium-seccomp.mjs <default.json> <moby/profiles commit>\n');
  process.exit(2);
}

/** The system calls Chrome's namespace sandbox needs on top of the default. */
export const SANDBOX_SYSCALLS = ['chroot', 'clone', 'setns', 'unshare'];

const profile = JSON.parse(readFileSync(source, 'utf8'));
profile.syscalls.push({
  names: SANDBOX_SYSCALLS,
  action: 'SCMP_ACT_ALLOW',
  comment: `Kage: Chrome's sandbox creates user, PID and network namespaces and chroots inside them. Everything else is Docker's default profile, moby/profiles ${commit}. See scripts/derive-chromium-seccomp.mjs.`,
});
process.stdout.write(`${JSON.stringify(profile, null, 2)}\n`);
