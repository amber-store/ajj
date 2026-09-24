// Refresh the public source snapshot. No repository metadata or credentials are read.
import { readdir, readFile, writeFile } from 'node:fs/promises';
const root = new URL('../', import.meta.url);
const files = {};
async function add(path) {
  const entries = await readdir(new URL(path, root), { withFileTypes: true });
  for (const entry of entries) {
    const name = `${path}${entry.name}`;
    if (entry.isDirectory()) await add(`${name}/`);
    else if (entry.isFile()) files[name] = await readFile(new URL(name, root), 'utf8');
  }
}
for (const directory of ['src/', 'tests/', '.cargo/', '.github/']) await add(directory);
for (const name of ['.envrc', '.gitignore', 'Cargo.toml', 'Cargo.lock', 'COPYING', 'LICENSE', 'README.md', 'flake.nix', 'flake.lock', 'rustfmt.toml']) files[name] = await readFile(new URL(name, root), 'utf8');
await writeFile(new URL('repository.json', import.meta.url), JSON.stringify(files));
console.log(`Snapshot refreshed: ${Object.keys(files).length} files`);
