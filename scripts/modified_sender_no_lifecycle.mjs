// Actual original PNPM caller/prepare functions; no package-manager main is run.
// Package/store/filesystem/lifecycle collaborators are deterministic no-I/O mocks.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { types as utilTypes } from "node:util";

assert.equal(process.argv.length, 3);
const bytes = readFileSync(process.argv[2]);
assert.ok(bytes.length <= 16 * 1024 * 1024);
assert.equal(createHash("sha256").update(bytes).digest("hex"),
  "293dcced3c77f5a633352076fe4c9a768e60c6992ed58bc695b465f839fe9615");
const source = bytes.toString("utf8");
function section(start, end) {
  assert.equal(source.split(start).length, 2);
  const offset = source.indexOf(start);
  const finish = source.indexOf(end, offset);
  assert.ok(finish > offset);
  return source.slice(offset, finish);
}
const definitions = `"use strict";
  const { safeJoinPath, safeReadPackageJsonFromDir, PnpmError, preferredPM,
          runLifecycleHook, packlist, addFilesFromDir, assert3, util13,
          assert5, util15, globalWarn } = bindings;
  ${section("async function preparePackage(", "function safeJoinPath(")}
  ${section("function createGitHostedTarballFetcher(", "var init_gitHostedTarballFetcher")}
  return createGitHostedTarballFetcher;`;
for (const scenario of [{}, { allowBuild: () => true, ignoreScripts: false }]) {
  const counters = { preferred: 0, hooks: 0, permission: 0, warnings: 0 };
  const createFetcher = new Function("bindings", definitions)({
    assert3: assert, util13: { types: utilTypes }, assert5: assert, util15: { types: utilTypes },
    safeJoinPath: () => "/synthetic-owned-package",
    safeReadPackageJsonFromDir: async () => ({
      name: "synthetic-no-lifecycle", version: "1.0.0", scripts: { prepare: "forbidden-hook" },
    }),
    PnpmError: class extends Error {},
    preferredPM: async () => { counters.preferred++; throw new Error("forbidden-prepare-attempt"); },
    runLifecycleHook: async () => { counters.hooks++; throw new Error("forbidden-lifecycle"); },
    packlist: async () => ["package.json"],
    addFilesFromDir: async () => { throw new Error("unexpected-repack"); },
    globalWarn: () => { counters.warnings++; },
  });
  const filesMap = new Map([["package.json", "synthetic"]]);
  const storeIndex = new Map();
  storeIndex.flush = () => {};
  const globals = { ignoreScripts: true, storeIndex };
  const request = { filesIndexFile: "prepared", ...scenario };
  if (request.allowBuild) request.allowBuild = () => { counters.permission++; return true; };
  const fetcher = createFetcher(async () => ({ filesMap, manifest: {}, requiresBuild: true, integrity: "pinned" }), globals);
  const result = await fetcher({
    tempDir: async () => "/synthetic-owned-package", importPackage: () => {},
  }, {
    tarball: "https://codeload.github.com/matrix-org/matrix-js-sdk/tar.gz/fa4772a4ddd335a5a8bfbf8bb21df49b70f7a999",
  }, request);
  assert.equal(result.filesMap, filesMap);
  assert.equal(result.integrity, "pinned");
  assert.deepEqual(counters, { preferred: 0, hooks: 0, permission: 0, warnings: 1 });
}
console.log(JSON.stringify({ original_manager_no_lifecycle_regression: "passed", scenarios: 2, lifecycle_calls: 0 }));
