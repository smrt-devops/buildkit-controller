'use strict';
// Minimal replacements for @actions/core (workflow commands and files).
const fs = require('node:fs');
const os = require('node:os');
const crypto = require('node:crypto');

const esc = (s) => String(s).replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A');
const out = (line) => process.stdout.write(line + os.EOL);

function getInput(name) {
  return (process.env[`INPUT_${name.replace(/ /g, '_').toUpperCase()}`] || '').trim();
}
function setSecret(v) { if (v) for (const l of String(v).split('\n')) if (l.trim()) out(`::add-mask::${esc(l.trim())}`); }
const info = (m) => out(m);
const warning = (m) => out(`::warning::${esc(m)}`);
const error = (m) => out(`::error::${esc(m)}`);
function fileCmd(env, key, value) {
  const f = process.env[env];
  if (!f) return;
  const d = `ghadelim_${crypto.randomUUID()}`;
  fs.appendFileSync(f, `${key}<<${d}${os.EOL}${value}${os.EOL}${d}${os.EOL}`);
}
const setOutput = (k, v) => fileCmd('GITHUB_OUTPUT', k, v);
const saveState = (k, v) => fileCmd('GITHUB_STATE', k, v);
const getState = (k) => process.env[`STATE_${k}`] || '';

module.exports = { getInput, setSecret, info, warning, error, setOutput, saveState, getState };
