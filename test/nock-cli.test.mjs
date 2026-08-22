import { execFileSync } from 'node:child_process'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'
import assert from 'node:assert/strict'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const bin = path.join(root, 'bin', 'nock-cli.mjs')
const run = (...args) => execFileSync(process.execPath, [bin, ...args], { encoding: 'utf8' })

test('prints help and version', () => {
  assert.match(run('--help'), /coding-agent skill installer/)
  assert.match(run('--version'), /^nock-cli 0\.1\.1\n$/)
})

test('installs the skill and references into a custom directory', async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), 'nock-cli-skill-'))
  try {
    const target = path.join(temp, 'skills', 'nock-cli')
    assert.match(run('install', '--path', target), /Installed Nock skill/)
    const skill = await readFile(path.join(target, 'SKILL.md'), 'utf8')
    const reference = await readFile(path.join(target, 'references', 'implementation.md'), 'utf8')
    const mark = await readFile(path.join(target, 'assets', 'nock-mark.svg'), 'utf8')
    assert.match(skill, /name: nock-cli/)
    assert.match(reference, /Required invariants/)
    assert.match(mark, /aria-label="Nock"/)
  } finally {
    await rm(temp, { recursive: true, force: true })
  }
})
