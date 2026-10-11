import { defineConfig } from 'vitepress'
import { withMermaid } from 'vitepress-plugin-mermaid'
import { pages, siteUrl } from './pages.mjs'

export default withMermaid(defineConfig({
  title: 'buddies',
  description: 'Local communication and persistent orchestration for AI agents.',
  lang: 'en-US',
  base: '/buddies/',
  srcExclude: ['superpowers/**', 'repository-review-*.md'],
  lastUpdated: true,
  sitemap: { hostname: siteUrl },
  head: [
    ['link', { rel: 'icon', type: 'image/png', href: '/buddies/logo.png' }],
    ['meta', { name: 'theme-color', content: '#4338ca' }],
    ['link', { rel: 'alternate', type: 'text/plain', title: 'AI documentation index', href: '/buddies/llms.txt' }]
  ],
  transformHead({ pageData }) {
    if (!pages.some(page => `${page.path}.md` === pageData.relativePath)) return []
    return [['link', { rel: 'alternate', type: 'text/markdown', title: 'Markdown source', href: `${siteUrl}${pageData.relativePath}` }]]
  },
  themeConfig: {
    logo: '/logo.png',
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Orchestrator', link: '/orchestrator' },
      { text: 'AI agents', link: '/agents' }
    ],
    sidebar: [...new Set(pages.map(page => page.group))].map(group => ({
      text: group,
      items: pages.filter(page => page.group === group && page.path !== 'index').map(page => ({ text: page.title, link: `/${page.path}` }))
    })),
    socialLinks: [{ icon: 'github', link: 'https://github.com/iltumio/buddies' }],
    search: { provider: 'local' },
    editLink: { pattern: 'https://github.com/iltumio/buddies/edit/main/docs/:path' },
    outline: [2, 3],
    footer: { message: 'Released under the MIT License.', copyright: 'buddies · Built for agents that work together.' }
  },
  mermaid: { securityLevel: 'strict' }
}))
