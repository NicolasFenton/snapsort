#!/usr/bin/env node
// Assemble the npm packages for snapsort from compiled binaries.
//
// Release (CI): one directory per Rust target triple, each holding the binary,
// e.g. artifacts/aarch64-apple-darwin/snapsort:
//
//   node scripts/npm/assemble.mjs --artifacts artifacts --out dist/npm
//
// Local testing: package only target/release/snapsort for this machine and
// produce installable tarballs:
//
//   cargo build --release
//   node scripts/npm/assemble.mjs --host --pack
//
// The version always comes from Cargo.toml, so the crate and every npm package
// stay in lockstep.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const TEMPLATE_DIR = path.join(ROOT, "npm/snapsort");

const { values: args } = parseArgs({
  options: {
    artifacts: { type: "string" },
    host: { type: "boolean", default: false },
    out: { type: "string", default: "dist/npm" },
    pack: { type: "boolean", default: false },
    "expect-version": { type: "string" },
  },
});

function die(message) {
  console.error(`assemble: ${message}`);
  process.exit(1);
}

function cargoVersion() {
  const manifest = fs.readFileSync(path.join(ROOT, "Cargo.toml"), "utf8");
  const pkg = manifest.split(/^\[/m).find((section) => section.startsWith("package]"));
  const match = pkg?.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) die("could not read [package] version from Cargo.toml");
  return match[1];
}

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

/** Set `repository` when building in GitHub Actions; npm trusted publishing and provenance require it. */
function repositoryField() {
  const repo = process.env.GITHUB_REPOSITORY;
  return repo ? { repository: { type: "git", url: `git+https://github.com/${repo}.git` } } : {};
}

function copyDocs(dir) {
  for (const doc of ["README.md", "LICENSE"]) {
    const src = path.join(ROOT, doc);
    if (fs.existsSync(src)) fs.copyFileSync(src, path.join(dir, doc));
  }
}

if (!args.host && !args.artifacts) die("pass --artifacts <dir> or --host");

const version = cargoVersion();
if (args["expect-version"] && args["expect-version"] !== version) {
  die(`tag version ${args["expect-version"]} does not match Cargo.toml version ${version}`);
}

const platforms = JSON.parse(fs.readFileSync(path.join(TEMPLATE_DIR, "platforms.json"), "utf8"));
const hostKey = `${process.platform}-${process.arch}`;
if (args.host && !platforms[hostKey]) die(`this machine (${hostKey}) is not a supported platform`);

const outDir = path.resolve(ROOT, args.out);
fs.rmSync(outDir, { recursive: true, force: true });
fs.mkdirSync(outDir, { recursive: true });

const packageDirs = [];

for (const [key, { package: name, target }] of Object.entries(platforms)) {
  if (args.host && key !== hostKey) continue;

  const [os, cpu] = key.split("-");
  const exe = os === "win32" ? "snapsort.exe" : "snapsort";
  const binary = args.host
    ? path.join(ROOT, "target/release", exe)
    : path.join(path.resolve(ROOT, args.artifacts), target, exe);
  if (!fs.existsSync(binary)) die(`missing binary for ${key}: ${binary}`);

  const dir = path.join(outDir, key);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  const dest = path.join(dir, "bin", exe);
  fs.copyFileSync(binary, dest);
  fs.chmodSync(dest, 0o755);

  writeJson(path.join(dir, "package.json"), {
    name,
    version,
    description: `The ${target} binary for snapsort`,
    license: "MIT",
    ...repositoryField(),
    os: [os],
    cpu: [cpu],
    files: ["bin/"],
    preferUnplugged: true,
  });
  copyDocs(dir);
  packageDirs.push(dir);
}

// The launcher package, with its optional dependencies pinned to this version.
const mainDir = path.join(outDir, "snapsort");
fs.mkdirSync(path.join(mainDir, "bin"), { recursive: true });
fs.copyFileSync(path.join(TEMPLATE_DIR, "bin/snapsort.js"), path.join(mainDir, "bin/snapsort.js"));
fs.chmodSync(path.join(mainDir, "bin/snapsort.js"), 0o755);
fs.copyFileSync(path.join(TEMPLATE_DIR, "platforms.json"), path.join(mainDir, "platforms.json"));
const template = JSON.parse(fs.readFileSync(path.join(TEMPLATE_DIR, "package.json"), "utf8"));
writeJson(path.join(mainDir, "package.json"), {
  ...template,
  version,
  ...repositoryField(),
  optionalDependencies: Object.fromEntries(
    Object.values(platforms).map(({ package: name }) => [name, version]),
  ),
});
copyDocs(mainDir);
packageDirs.push(mainDir);

console.log(`Assembled snapsort ${version}:`);
for (const dir of packageDirs) console.log(`  ${path.relative(ROOT, dir)}`);

if (args.pack) {
  console.log("Tarballs:");
  for (const dir of packageDirs) {
    const output = execFileSync("npm", ["pack", "--json", "--pack-destination", outDir], {
      cwd: dir,
      encoding: "utf8",
      shell: process.platform === "win32",
    });
    const [{ filename }] = JSON.parse(output);
    console.log(`  ${path.relative(ROOT, path.join(outDir, filename))}`);
  }
}
