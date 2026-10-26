'use strict';
// Dependency-free Mac entry: preflight reads DATA only; stage/build exec the
// separately bound hosted Python. Build scripts use genuine staged Python3.11.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
// Only source-fixed categories may cross the failed-entry boundary.
const ENTRY_FAILURES = Object.freeze(["entry", "host_guard", "workspace", "plan_read", "plan_parse", "plan_identity", "preflight_input", "preflight_tools", "preflight_xcode_listing", "preflight_xcode_root", "preflight_xcode_version", "preflight_sdk_listing", "preflight_sdk_metadata", "preflight_sdk_inventory", "preflight_python_listing", "preflight_zstd_candidates", "preflight_emit", "producer_input", "python_admission", "producer_source", "exec_support", "producer_args", "exec_handover"]);
let entryFailure = 'entry';
function need(ok) { if (!ok) throw new Error(); }
function fact(name, cap = 128 * 1024 * 1024) {
  const actual = fs.realpathSync(name);
  const fd = fs.openSync(actual, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
  try {
    const st = fs.fstatSync(fd);
    need(st.isFile() && st.uid === 0 && !(st.mode & 0o022) && st.size <= cap);
    const hash = crypto.createHash('sha256'); let size = 0;
    const buffer = Buffer.alloc(1024 * 1024);
    for (;;) { const n = fs.readSync(fd, buffer); if (!n) break; size += n; need(size <= st.size); hash.update(buffer.subarray(0, n)); }
    const after = fs.fstatSync(fd), named = fs.statSync(actual);
    need([after, named].every(v => v.dev === st.dev && v.ino === st.ino && v.size === st.size && v.mtimeMs === st.mtimeMs && v.ctimeMs === st.ctimeMs));
    return { path: actual, bytes: size, sha256: hash.digest('hex') };
  } finally { fs.closeSync(fd); }
}
function read(name, cap) {
  const fd = fs.openSync(name, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
  try { const st = fs.fstatSync(fd); need(st.isFile() && st.size <= cap);
    const result = fs.readFileSync(fd); need(result.length === st.size && result.length <= cap);
    const after = fs.fstatSync(fd); need(after.dev === st.dev && after.ino === st.ino && after.size === st.size && after.mtimeMs === st.mtimeMs && after.ctimeMs === st.ctimeMs);
    return result;
  } finally { fs.closeSync(fd); }
}
function sdkInventory(root) {
  root = fs.realpathSync(root); need(path.dirname(root) === '/Applications');
  const files = [], directories = [], pending = [root]; let total = 0;
  const started = Date.now();
  const relative = file => { const value = path.relative(root, file).split(path.sep).join('/');
    need(value && !value.includes('\\') && !/[\x00-\x1f\x7f]/.test(value) && !value.split('/').some(x => !x || ['.','..','.git'].includes(x))); return value; };
  while (pending.length) {
    const here = pending.pop(), dir = fs.lstatSync(here);
    need(dir.isDirectory() && dir.uid === 0 && !(dir.mode & 0o022) && Date.now()-started <= 240000);
    directories.push(here === root ? '.' : relative(here));
    for (const child of fs.readdirSync(here)) {
      const file = path.join(here, child), st = fs.lstatSync(file), name = relative(file);
      if (st.isSymbolicLink()) {
        const resolved = fs.realpathSync(file); need(resolved === root || resolved.startsWith(root + path.sep));
        files.push({path: name, target: fs.readlinkSync(file), type: 'symlink'});
      } else if (st.isDirectory()) { need(st.uid === 0 && !(st.mode & 0o022)); pending.push(file); }
      else {
        need(st.isFile() && st.uid === 0 && !(st.mode & 0o022) && st.size <= 2*1024**3);
        const fd = fs.openSync(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW);
        try {
          const h = crypto.createHash('sha256'), buffer = Buffer.alloc(1024*1024); let count = 0;
          for (;;) { const n = fs.readSync(fd, buffer); if (!n) break;
            count += n; total += n; need(count <= st.size && total <= 64*1024**3 && Date.now()-started <= 240000); h.update(buffer.subarray(0,n)); }
          const after = fs.fstatSync(fd), named = fs.lstatSync(file);
          need(count === st.size && [after,named].every(x => x.dev === st.dev && x.ino === st.ino && x.size === st.size && x.mode === st.mode && x.uid === st.uid && x.mtimeMs === st.mtimeMs && x.ctimeMs === st.ctimeMs));
          files.push({bytes: count, path: name, sha256: h.digest('hex'), type: 'file'});
        } finally { fs.closeSync(fd); }
      }
      need(files.length + directories.length + pending.length <= 1000000);
    }
  }
  const compare = (a,b) => Buffer.compare(Buffer.from(a,'utf8'), Buffer.from(b,'utf8'));
  files.sort((a,b) => compare(a.path,b.path)); directories.sort(compare);
  const fileBytes = Buffer.from(JSON.stringify(files)+'\n'), dirBytes = Buffer.from(JSON.stringify(directories)+'\n');
  need(fileBytes.length <= 128*1024*1024 && dirBytes.length <= 128*1024*1024);
  return { app: path.basename(root), file_inventory_sha256: crypto.createHash('sha256').update(fileBytes).digest('hex'),
    directory_topology_sha256: crypto.createHash('sha256').update(dirBytes).digest('hex'), files: files.length,
    directories: directories.length, original_file_bytes: total, sdk_bytes_copied: false };
}
try {
  const input = name => process.env[`INPUT_${name.toUpperCase()}`] || '';
  const target = input('target'), phase = input('phase');
  entryFailure = 'host_guard';
  need(process.platform === 'darwin' && ['mac-arm64', 'mac-x64'].includes(target));
  need(process.arch === (target === 'mac-arm64' ? 'arm64' : 'x64'));
  need(process.env.GITHUB_ACTIONS === 'true' && process.env.RUNNER_ENVIRONMENT === 'github-hosted'
    && process.env.GITHUB_REPOSITORY === 'brk275/Morpheus' && process.env.GITHUB_REF === 'refs/heads/main'
    && process.env.GITHUB_ACTOR === 'brk275' && process.env.GITHUB_TRIGGERING_ACTOR === 'brk275'
    && process.env.GITHUB_EVENT_NAME === 'workflow_dispatch');
  entryFailure = 'workspace';
  const workspace = fs.realpathSync(process.env.GITHUB_WORKSPACE);
  const planPath = path.join(workspace, 'native/webrtc/macos', target, 'pipeline-plan.json');
  entryFailure = 'plan_read';
  const raw = read(planPath, 1024 * 1024), planSha = crypto.createHash('sha256').update(raw).digest('hex');
  entryFailure = 'plan_parse';
  const plan = JSON.parse(raw.toString('utf8'));
  entryFailure = 'plan_identity';
  need(plan.schema === 'morpheus-native-macos-pipeline-v1' && plan.target === target);
  if (phase === 'preflight') {
    entryFailure = 'preflight_input';
    need(input('publish_handoff') === 'false');
    entryFailure = 'preflight_tools';
    const tools = {};
    for (const role of ['git', 'xcodebuild', 'xcrun', 'xcode-select', 'sw_vers']) tools[role] = fact('/usr/bin/' + role);
    entryFailure = 'preflight_xcode_listing';
    const xcodes = fs.readdirSync('/Applications').filter(x => /^Xcode(?:_[A-Za-z0-9_.-]+)?\.app$/.test(x));
    need(xcodes.length <= 32);
    const sdk = [];
    for (const app of xcodes) {
      entryFailure = 'preflight_xcode_root';
      const root = path.join('/Applications', app), canonical = fs.realpathSync(root);
      need(path.dirname(canonical) === '/Applications');
      entryFailure = 'preflight_xcode_version';
      const version = fact(path.join(canonical, 'Contents/version.plist'), 1024 * 1024);
      const sdkDir = path.join(canonical, 'Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs');
      entryFailure = 'preflight_sdk_listing';
      const names = fs.readdirSync(sdkDir).filter(n => /^MacOSX\d+\.\d+\.sdk$/.test(n)); need(names.length <= 32);
      for (const name of names) {
        const file = path.join(sdkDir, name, 'System/Library/CoreServices/SystemVersion.plist');
        entryFailure = 'preflight_sdk_metadata';
        const metadata = fact(file, 1024 * 1024); delete metadata.path;
        const appVersion = { ...version }; delete appVersion.path;
        need(sdk.length < 32);
        sdk.push({ app, sdk: name, version_plist: appVersion, sdk_system_version_plist: metadata });
      }
    }
    let fullSdk = 'UNBOUND';
    if (input('xcode_app')) {
      entryFailure = 'preflight_sdk_inventory';
      need(/^Xcode(?:_[A-Za-z0-9_.-]+)?\.app$/.test(input('xcode_app')) && xcodes.includes(input('xcode_app')));
      fullSdk = sdkInventory(path.join('/Applications', input('xcode_app')));
    }
    entryFailure = 'preflight_python_listing';
    const pythonCandidates = [];
    const prefixes = ['/Library/Frameworks/Python.framework/Versions', '/opt/hostedtoolcache/Python'];
    for (const prefix of prefixes) {
      if (!fs.existsSync(prefix)) continue;
      const names = fs.readdirSync(prefix); need(names.length <= 128);
      for (const version of names.filter(x => /^3\.(?:13|14)(?:\.[0-9]+)?$/.test(x))) {
        const candidates = prefix.startsWith('/Library') ? [path.join(prefix,version,'bin/python3.'+version.split('.')[1])] :
          [path.join(prefix,version,process.arch === 'arm64' ? 'arm64' : 'x64','bin/python3.'+version.split('.')[1])];
        for (const candidate of candidates) {
          if (!fs.existsSync(candidate)) continue;
          try { pythonCandidates.push({candidate: fact(candidate), version_executed: false}); }
          catch (_) { pythonCandidates.push({candidate_path: candidate, admissible_root_owned_identity: false}); }
        }
      }
    }
    entryFailure = 'preflight_zstd_candidates';
    const zstdCandidates = [];
    for (const candidate of ['/usr/local/bin/zstd', '/opt/homebrew/bin/zstd']) {
      if (!fs.existsSync(candidate)) continue;
      try { zstdCandidates.push(fact(candidate)); }
      catch (_) { zstdCandidates.push({candidate_path: candidate, admissible_root_owned_identity: false}); }
    }
    // No version commands, Xcode selection, license acceptance, installs or SDK copies.
    entryFailure = 'preflight_emit';
    console.log('MORPHEUS_NATIVE_MAC_HOST_INPUTS ' + JSON.stringify({ schema: 1, target,
      source_commit: process.env.GITHUB_SHA, run_id: process.env.GITHUB_RUN_ID, run_attempt: process.env.GITHUB_RUN_ATTEMPT,
      image_os: process.env.ImageOS || null, image_version: process.env.ImageVersion || null,
      plan_sha256: planSha, tools, sdk_metadata: sdk,
      python_candidates: pythonCandidates, zstd_candidates: zstdCandidates, sdk_full_inventory: fullSdk,
      sdk_redistributed: false, cache_write: false, staged: false, build_admission_granted: false,
      native_sources_complete: false, build_ready: false, distribution_ready: false }));
  } else {
    entryFailure = 'producer_input';
    need(input('xcode_app') === '');
    need(['stage', 'build', 'cache-receipt'].includes(phase) && /^[0-9a-f]{64}$/.test(input('plan_sha256')) && planSha === input('plan_sha256'));
    entryFailure = 'python_admission';
    const python = plan.host_python;
    need(python && typeof python.path === 'string' && (python.path.startsWith('/Library/Frameworks/Python.framework/') || python.path.startsWith('/opt/hostedtoolcache/Python/')));
    const actual = fact(python.path); need(actual.path === python.path && actual.sha256 === python.sha256 && actual.bytes === python.bytes);
    entryFailure = 'producer_source';
    const script = path.resolve(__dirname, '..', 'run_native_webrtc_macos.py');
    const scriptRaw = read(script, 256 * 1024); need(crypto.createHash('sha256').update(scriptRaw).digest('hex') === plan.pipeline_sha256);
    entryFailure = 'exec_support';
    need(typeof process.execve === 'function');
    entryFailure = 'producer_args';
    const args = [python.path, '-I', '-B', script, '--target', target, '--phase', phase, '--plan', planPath, '--plan-sha256', planSha];
    if (phase === 'stage') {
      need(['cache_key','cache_id','handoff_sha256','handoff_bytes','admission_sha256'].every(name => input(name) === ''));
      if (input('publish_handoff') === 'true') args.push('--publish-handoff'); else need(input('publish_handoff') === 'false');
    } else {
      need(input('publish_handoff') === 'false');
      args.push('--cache-key', input('cache_key'), '--handoff-sha256', input('handoff_sha256'), '--handoff-bytes', input('handoff_bytes'));
      if (phase === 'build') args.push('--cache-id', input('cache_id'), '--admission', path.join(workspace, 'native/webrtc/macos', target, 'build-admission.json'), '--admission-sha256', input('admission_sha256'));
    }
    const env = { ...process.env, PYTHONNOUSERSITE: '1', PYTHONDONTWRITEBYTECODE: '1' };
    for (const name of ['NODE_OPTIONS','PYTHONPATH','PYTHONHOME','VPYTHON_BYPASS','VPYTHON_VIRTUALENV_ROOT','CIPD_CACHE_DIR']) delete env[name];
    entryFailure = 'exec_handover';
    process.execve(python.path, args, env); throw new Error();
  }
} catch (_) {
  const category = ENTRY_FAILURES.includes(entryFailure) ? entryFailure : 'entry';
  process.stderr.write('MORPHEUS_NATIVE_MAC_PRODUCER_ENTRY failed category=' + category + '\n');
  process.exitCode = 1;
}
