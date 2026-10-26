'use strict';
// Original dependency-free Linux-only entry. execve retains the same process
// identity and standard streams; it creates no child or process supervisor.
const path = require('node:path');

try {
  if (process.platform !== 'linux' || typeof process.execve !== 'function') throw new Error();
  const input = name => process.env[`INPUT_${name.toUpperCase()}`] || '';
  const phase = input('phase');
  if (!['preflight', 'stage', 'build', 'cache-receipt'].includes(phase)) throw new Error();
  const python = '/usr/bin/python3';
  const script = path.resolve(__dirname, '..', 'run_native_webrtc_linux.py');
  const workspace = process.env.GITHUB_WORKSPACE;
  if (!workspace || !path.isAbsolute(workspace)) throw new Error();
  const args = [python, '-I', '-B', script, '--phase', phase, '--plan',
    path.join(workspace, 'native/webrtc/linux/pipeline-plan.json'),
    '--plan-sha256', input('plan_sha256')];
  if (phase === 'preflight') {
    if (input('publish_handoff') !== 'false') throw new Error();
  } else if (phase === 'stage') {
    if (input('publish_handoff') === 'true') args.push('--publish-handoff');
    else if (input('publish_handoff') !== 'false') throw new Error();
  } else {
    if (input('publish_handoff') !== 'false') throw new Error();
    args.push('--cache-key', input('cache_key'), '--handoff-sha256', input('handoff_sha256'),
      '--handoff-bytes', input('handoff_bytes'));
    if (phase === 'build') args.push('--cache-id', input('cache_id'), '--admission',
      path.join(workspace, 'native/webrtc/linux/build-admission.json'),
      '--admission-sha256', input('admission_sha256'));
  }
  const env = { ...process.env, PYTHONNOUSERSITE: '1', PYTHONDONTWRITEBYTECODE: '1' };
  delete env.NODE_OPTIONS;
  delete env.PYTHONPATH;
  delete env.PYTHONHOME;
  process.execve(python, args, env);
  throw new Error();
} catch (_) {
  process.stderr.write('MORPHEUS_NATIVE_PRODUCER_ENTRY failed\n');
  process.exitCode = 1;
}
