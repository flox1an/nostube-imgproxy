import { defineConfig } from 'vitepress'

export default defineConfig({
  lang: 'en-US',
  title: 'Nostube Image Proxy',
  description: 'Resize images, extract video thumbnails and discover Blossom media. Configuration, self-hosting and API documentation.',
  base: '/nostube-imgproxy/',
  cleanUrls: true,
  lastUpdated: true,
  srcExclude: ['agents/**'],
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Configuration', link: '/configuration' },
      { text: 'Operations', link: '/operations' },
      { text: 'API', link: '/reference/api' },
    ],
    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Getting started', link: '/guide/getting-started' },
          { text: 'How it works', link: '/guide/how-it-works' },
        ],
      },
      {
        text: 'Self-hosting',
        items: [
          { text: 'Configuration', link: '/configuration' },
          { text: 'Operations and security', link: '/operations' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'HTTP API', link: '/reference/api' },
          { text: 'Browser presets', link: '/reference/presets' },
        ],
      },
    ],
    search: { provider: 'local' },
    socialLinks: [{ icon: 'github', link: 'https://github.com/flox1an/nostube-imgproxy' }],
    editLink: {
      pattern: 'https://github.com/flox1an/nostube-imgproxy/edit/main/docs/:path',
    },
  },
})
