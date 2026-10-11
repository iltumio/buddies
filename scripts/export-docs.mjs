import { readFile, writeFile, mkdir } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { pages, siteUrl } from '../docs/.vitepress/pages.mjs'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const output = resolve(root, 'docs/.vitepress/dist')
const published = new Set(pages.map(page => `${page.path}.md`))

function markdownLink(href, pagePath) {
  if (/^(https?:|mailto:|#)/.test(href)) return href
  const url = href.startsWith('/')
    ? new URL(href.slice(1), siteUrl)
    : new URL(href, `${siteUrl}${pagePath}.html`)
  const relative = decodeURIComponent(url.pathname.slice(new URL(siteUrl).pathname.length))
  const target = relative.replace(/\.html$/, '').replace(/\.md$/, '') || 'index'
  if (published.has(`${target}.md`)) url.pathname = `${new URL(siteUrl).pathname}${target}.md`
  return url.href
}

const exports = []
for (const page of pages) {
  const source = await readFile(resolve(root, `docs/${page.path}.md`), 'utf8')
  let markdown = source.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, '')
  if (page.path === 'index') markdown = `# buddies\n\nLocal communication and persistent orchestration for AI agents.\n\n${markdown}`
  // Rewrite Markdown link destinations outside fenced code, preserving runnable examples.
  let fenced = false
  markdown = markdown.split('\n').map(line => {
    if (/^\s*(```|~~~)/.test(line)) fenced = !fenced
    if (fenced) return line
    return line.replace(/\]\(([^\s)]+)\)/g, (_, href) => `](${markdownLink(href, page.path)})`)
  }).join('\n')
  const destination = resolve(output, `${page.path}.md`)
  await mkdir(dirname(destination), { recursive: true })
  await writeFile(destination, markdown)
  exports.push({ ...page, markdown })
}
let index = '# buddies\n\n> Local communication and persistent orchestration for AI agents.\n\nRead only the topics relevant to your task. These links serve plain Markdown exported from the published documentation.\n\n'
for (const group of new Set(pages.map(page => page.group))) {
  index += `## ${group}\n\n`
  index += pages.filter(page => page.group === group).map(page => `- [${page.title}](${siteUrl}${page.path}.md)`).join('\n') + '\n\n'
}
index += `## Optional\n\n- [Complete documentation bundle](${siteUrl}llms-full.txt): all published pages in one file.\n`
await writeFile(resolve(output, 'llms.txt'), index)
await writeFile(resolve(output, 'llms-full.txt'), exports.map(page => `<!-- Source: ${siteUrl}${page.path}.md -->\n\n${page.markdown}`).join('\n\n---\n\n'))
await writeFile(resolve(output, '.nojekyll'), '')
console.log(`Exported ${exports.length} Markdown pages, llms.txt and llms-full.txt`)
