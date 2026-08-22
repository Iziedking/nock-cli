import { createHash } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const cliRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const packageJson = JSON.parse(await readFile(path.join(cliRoot, 'package.json'), 'utf8'))
const skillRoot = path.join(cliRoot, 'skills', 'nock-cli')
const distRoot = path.join(cliRoot, 'dist')

const files = [
  ['SKILL.md', 'nock-cli-skill/SKILL.md'],
  ['README.md', 'nock-cli-skill/README.md'],
  ['references/implementation.md', 'nock-cli-skill/references/implementation.md'],
  ['../../LICENSE', 'nock-cli-skill/LICENSE'],
]

function crc32(buffer) {
  let crc = 0xffffffff
  for (const byte of buffer) {
    crc ^= byte
    for (let i = 0; i < 8; i += 1) {
      crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0)
    }
  }
  return (crc ^ 0xffffffff) >>> 0
}

function dosDateTime() {
  const now = new Date()
  return {
    time: (now.getHours() << 11) | (now.getMinutes() << 5) | Math.floor(now.getSeconds() / 2),
    date: ((now.getFullYear() - 1980) << 9) | ((now.getMonth() + 1) << 5) | now.getDate(),
  }
}

function u16(value) {
  const result = Buffer.alloc(2)
  result.writeUInt16LE(value)
  return result
}

function u32(value) {
  const result = Buffer.alloc(4)
  result.writeUInt32LE(value)
  return result
}

async function makeZip() {
  const localParts = []
  const centralParts = []
  let offset = 0
  const stamp = dosDateTime()

  for (const [source, entry] of files) {
    const data = await readFile(path.resolve(skillRoot, source))
    const name = Buffer.from(entry)
    const checksum = crc32(data)
    const header = Buffer.concat([
      u32(0x04034b50), u16(20), u16(0), u16(0), u16(stamp.time), u16(stamp.date),
      u32(checksum), u32(data.length), u32(data.length), u16(name.length), u16(0), name,
    ])
    localParts.push(header, data)

    centralParts.push(Buffer.concat([
      u32(0x02014b50), u16(20), u16(20), u16(0), u16(0), u16(stamp.time), u16(stamp.date),
      u32(checksum), u32(data.length), u32(data.length), u16(name.length), u16(0), u16(0),
      u16(0), u16(0), u32(0), u32(offset), name,
    ]))
    offset += header.length + data.length
  }

  const central = Buffer.concat(centralParts)
  const local = Buffer.concat(localParts)
  const end = Buffer.concat([
    u32(0x06054b50), u16(0), u16(0), u16(files.length), u16(files.length),
    u32(central.length), u32(local.length), u16(0),
  ])
  return Buffer.concat([local, central, end])
}

await mkdir(distRoot, { recursive: true })
const zip = await makeZip()
const versioned = path.join(distRoot, `nock-cli-skill-v${packageJson.version}.zip`)
const stable = path.join(distRoot, 'nock-cli-skill.zip')
await writeFile(versioned, zip)
await writeFile(stable, zip)

const digest = createHash('sha256').update(zip).digest('hex')
await writeFile(
  path.join(distRoot, `nock-cli-skill-v${packageJson.version}.sha256`),
  `${digest}  nock-cli-skill-v${packageJson.version}.zip\n`,
)
await writeFile(
  path.join(distRoot, 'nock-cli-skill.sha256'),
  `${digest}  nock-cli-skill.zip\n`,
)
console.log(`Created ${path.relative(cliRoot, versioned)} (${zip.length} bytes)`)
console.log(`SHA-256 ${digest}`)
