// Where does bili keep its config, and what are the model-limit keys?
import fs from 'fs';
const F = 'C:\\Users\\17277\\.dsh\\profiles\\tauri\\node_modules\\billion-context\\dist\\index.js';
const txt = fs.readFileSync(F, 'utf8');
console.log('size', txt.length);
const needles = [
  'outputHeadroomMaxPct',
  'models.output',
  'output":',
  'knownOutputCeiling',
  'headroomWindow',
  'nativeWindow',
  'configPath',
  'configFile',
  '.bili',
  'billion-context/config',
  'bili-config',
  'resolveConfigDir',
  'acp-config',
];
for (const n of needles) {
  let i = -1, c = 0;
  while ((i = txt.indexOf(n, i + 1)) !== -1 && c < 3) {
    c++;
    console.log(`\n### [${n}] @${i}\n${txt.slice(Math.max(0, i - 450), i + 350).replace(/\s+/g, ' ')}`);
  }
  if (c === 0) console.log(`\n### [${n}] NOT FOUND`);
}
