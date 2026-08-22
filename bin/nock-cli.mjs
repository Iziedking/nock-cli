#!/usr/bin/env node

import { cp, mkdir, readFile, access } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const PACKAGE_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const SKILL_ROOT = path.join(PACKAGE_ROOT, 'skills', 'nock-cli')
const SKILL_NAME = 'nock-cli'

const AGENT_SKILL_ROOTS = {
  codex: '.codex/skills',
  claude: '.claude/skills',
  cursor: '.cursor/skills',
  windsurf: '.windsurf/skills',
  gemini: '.gemini/skills',
}

function homeDirectory() {
  const home = process.env.HOME || process.env.USERPROFILE
  if (!home) throw new Error('Could not determine the home directory; pass --path explicitly.')
  return home
}

function expandHome(value) {
  if (value === '~') return homeDirectory()
  if (value.startsWith(`~${path.sep}`) || value.startsWith('~/')) {
    return path.join(homeDirectory(), value.slice(2))
  }
  return value
}

function parseArgs(argv) {
  let [command = 'help', ...rest] = argv
  if (command === '--help' || command === '-h') {
    command = 'help'
  } else if (command === '--version' || command === '-V') {
    command = 'version'
  }
  const options = { command, agent: undefined, target: undefined, force: false }

  for (let i = 0; i < rest.length; i += 1) {
    const flag = rest[i]
    if (flag === '--force' || flag === '-f') {
      options.force = true
    } else if (flag === '--agent' || flag === '-a') {
      options.agent = rest[++i]
    } else if (flag === '--path' || flag === '-p') {
      options.target = rest[++i]
    } else if (flag === '--help' || flag === '-h') {
      options.command = 'help'
    } else if (flag === '--version' || flag === '-V') {
      options.command = 'version'
    } else {
      throw new Error(`Unknown option: ${flag}`)
    }
  }

  return options
}

function skillTarget(options) {
  if (options.target) {
    const target = path.resolve(expandHome(options.target))
    return path.basename(target).toLowerCase() === 'skill.md' ? path.dirname(target) : target
  }

  const agent = options.agent || 'codex'
  if (agent === 'all') {
    return Object.entries(AGENT_SKILL_ROOTS).map(([name, root]) => ({
      agent: name,
      target: path.join(homeDirectory(), root, SKILL_NAME),
    }))
  }

  const root = AGENT_SKILL_ROOTS[agent]
  if (!root) {
    throw new Error(`Unknown agent "${agent}". Use --path for a custom agent.`)
  }
  return path.join(homeDirectory(), root, SKILL_NAME)
}

async function exists(file) {
  try {
    await access(file)
    return true
  } catch {
    return false
  }
}

async function installOne(target, force) {
  const marker = path.join(target, 'SKILL.md')
  if (await exists(marker) && !force) {
    throw new Error(`${marker} already exists; pass --force to replace it.`)
  }
  await mkdir(target, { recursive: true })
  await cp(SKILL_ROOT, target, { recursive: true, force: true })
  return marker
}

function help() {
  return `Nock coding-agent skill installer

Usage:
  nock-cli install [--agent codex|claude|cursor|windsurf|gemini|all]
  nock-cli install --path <skill-directory> [--force]
  nock-cli show
  nock-cli path --agent <agent>

Examples:
  npx nock-cli install --agent codex
  nock-cli install --agent claude
  nock-cli install --agent cursor --force
  nock-cli install --path ~/.my-agent/skills/nock-cli

The installer copies SKILL.md and its references. It never reads, creates or
transmits wallet files, private keys, seed phrases or RPC credentials.
`
}

async function main() {
  const options = parseArgs(process.argv.slice(2))

  if (options.command === 'help') {
    process.stdout.write(help())
    return
  }
  if (options.command === 'version') {
    const packageJson = JSON.parse(await readFile(path.join(PACKAGE_ROOT, 'package.json'), 'utf8'))
    process.stdout.write(`${packageJson.name} ${packageJson.version}\n`)
    return
  }
  if (options.command === 'show') {
    process.stdout.write(await readFile(path.join(SKILL_ROOT, 'SKILL.md'), 'utf8'))
    return
  }
  if (options.command === 'path') {
    const target = skillTarget(options)
    if (Array.isArray(target)) throw new Error('Use one agent at a time with path.')
    process.stdout.write(`${target}\n`)
    return
  }
  if (options.command !== 'install') {
    throw new Error(`Unknown command "${options.command}". Run nock-cli --help.`)
  }

  const targets = skillTarget(options)
  if (Array.isArray(targets)) {
    for (const item of targets) {
      const marker = await installOne(item.target, options.force)
      process.stdout.write(`Installed ${item.agent} skill at ${marker}\n`)
    }
    return
  }

  const marker = await installOne(targets, options.force)
  process.stdout.write(`Installed Nock skill at ${marker}\n`)
}

main().catch(error => {
  process.stderr.write(`nock-cli: ${error.message}\n`)
  process.exitCode = 1
})
