import { readFile, access } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { pages, siteUrl } from '../docs/.vitepress/pages.mjs'
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const output = resolve(root, 'docs/.vitepress/dist')
const index = await readFile(resolve(output, 'llms.txt'), 'utf8')
const bundle = await readFile(resolve(output, 'llms-full.txt'), 'utf8')
for (const page of pages) {
  await access(resolve(output, `${page.path}.html`))
  const markdown = await readFile(resolve(output, `${page.path}.md`), 'utf8')
  const url = `${siteUrl}${page.path}.md`
  if (!markdown.startsWith('# ') || !index.includes(url) || !bundle.includes(markdown)) {
    throw new Error(`Incomplete agent export: ${page.path}`)
  }
  if (/https:\/\/iltumio\.github\.io\/(?!buddies\/)/.test(markdown)) throw new Error(`Link outside Pages base: ${page.path}`)
  for (const match of markdown.matchAll(/\]\((https:\/\/iltumio\.github\.io\/buddies\/[^\s)]+)\)/g)) {
    const path = decodeURIComponent(new URL(match[1]).pathname.slice('/buddies/'.length))
    await access(resolve(output, path || 'index.html'))
  }
}
await access(resolve(output, 'logo.png'))
await access(resolve(output, 'sitemap.xml'))
console.log(`Checked ${pages.length} HTML pages and linked Markdown exports`)
