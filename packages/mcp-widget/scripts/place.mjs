// Copy the built page to where `triton-core` `include_str!`s it. The copy is
// committed; `scripts/check.sh` rebuilds and fails on any difference.
import { copyFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const from = fileURLToPath(new URL('../dist/index.html', import.meta.url));
const to = fileURLToPath(
  new URL('../../../crates/triton-core/src/a2ui/mcp_widget.html', import.meta.url),
);
copyFileSync(from, to);
console.log(`placed ${to}`);
