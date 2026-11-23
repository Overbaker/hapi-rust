import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'HAPI-Rust',
  description: 'Run 100+ local Claude Code, Codex & AI coding agent sessions from any browser/PWA with a 4.4 MiB Rust session wrapper and automatic hibernation.',
  base: '/hapi-rust/',
  sitemap: { hostname: 'https://overbaker.github.io/hapi-rust/' },
  srcExclude: ['performance/**'],

  head: [
    ['link', { rel: 'icon', href: '/hapi-rust/favicon.ico' }],
    ['link', { rel: 'alternate', type: 'text/plain', title: 'llms.txt', href: 'https://overbaker.github.io/hapi-rust/llms.txt' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:site_name', content: 'HAPI-Rust' }],
    ['meta', { property: 'og:title', content: 'HAPI-Rust — 4.4 MiB Rust Session Wrapper for Claude Code & Codex' }],
    ['meta', { property: 'og:description', content: 'Local-first remote control for 100+ AI coding agent sessions. 19x less wrapper RAM than Bun/TS, 8–14 ms cold start, and automatic 24h idle hibernation.' }],
    ['meta', { name: 'twitter:card', content: 'summary_large_image' }],
    ['script', { type: 'application/ld+json' }, JSON.stringify({
      '@context': 'https://schema.org',
      '@type': 'SoftwareApplication',
      name: 'HAPI-Rust',
      alternateName: 'Overbaker/hapi-rust',
      applicationCategory: 'DeveloperApplication',
      operatingSystem: 'Linux, Windows, macOS',
      license: 'https://www.gnu.org/licenses/agpl-3.0.html',
      isAccessibleForFree: true,
      codeRepository: 'https://github.com/Overbaker/hapi-rust',
      url: 'https://overbaker.github.io/hapi-rust/',
      description: 'Open-source, local-first remote control and 4.4 MiB Rust session wrapper for running 100+ Claude Code, OpenAI Codex, Antigravity, and Pi sessions with automatic idle hibernation.',
      offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' },
    })],
  ],

  themeConfig: {
    logo: '/logo.svg',

    nav: [
      { text: 'Quick Start', link: '/guide/quick-start' },
      { text: 'Rust Wrapper', link: '/guide/rust-wrapper' },
      { text: 'Releases', link: 'https://github.com/Overbaker/hapi-rust/releases', target: '_blank' }
    ],

    sidebar: [
      {
        text: 'Get Started',
        items: [
          { text: 'Quick Start', link: '/guide/quick-start' },
          { text: 'Rust Session Wrapper', link: '/guide/rust-wrapper' },
          { text: 'Installation', link: '/guide/installation' },
          { text: 'Authentication', link: '/guide/authentication' },
          { text: 'PWA', link: '/guide/pwa' }
        ]
      },
      {
        text: 'Guide',
        items: [
          { text: 'How it Works', link: '/guide/how-it-works' },
          { text: 'Session List', link: '/guide/session-list' },
          { text: 'Composer', link: '/guide/composer' },
          { text: 'Conversation History', link: '/guide/conversation-history' },
          { text: 'Import Local Sessions', link: '/guide/import-local-sessions' },
          { text: 'Peer Sessions', link: '/guide/peer-sessions' },
          { text: 'Session-attached jobs', link: '/guide/session-jobs' },
          { text: 'Usage & Quotas', link: '/guide/usage-and-quotas' },
          { text: 'Voice Assistant', link: '/guide/voice-assistant' },
          { text: 'Why HAPI', link: '/guide/why-hapi' },
          { text: 'FAQ', link: '/guide/faq' }
        ]
      },
      {
        text: 'Agents',
        items: [
          { text: 'Agents', link: '/guide/agents' },
          { text: 'Codex Shared Sessions', link: '/guide/codex-shared-sessions' }
        ]
      },
      {
        text: 'Advanced',
        items: [
          { text: 'CLI Reference', link: '/guide/cli-reference' },
          { text: 'Configuration', link: '/guide/configuration' },
          { text: 'Namespace', link: '/guide/namespace' },
          { text: 'Deployment', link: '/guide/deployment' },
          { text: 'Notifications', link: '/guide/notifications' }
        ]
      },
      {
        text: 'API',
        items: [
          { text: 'Native Companion Contract', link: '/api/native-companion-contract' },
          {
            text: 'Client contract',
            items: [
              { text: 'Overview', link: '/api/client-contract/' },
              { text: 'Auth', link: '/api/client-contract/auth' },
              { text: 'REST', link: '/api/client-contract/rest' },
              { text: 'SSE', link: '/api/client-contract/sse' },
              { text: 'Pagination', link: '/api/client-contract/pagination' },
              { text: 'Messages', link: '/api/client-contract/messages' },
              { text: 'Read state', link: '/api/client-contract/read-state' },
              { text: 'Errors', link: '/api/client-contract/errors' }
            ]
          }
        ]
      }
    ],

    socialLinks: [
      { icon: 'github', link: 'https://github.com/Overbaker/hapi-rust' }
    ],

    footer: {
      message: 'Released under the AGPL-3.0 License. · <a href="/hapi-rust/privacy.html">Privacy Policy</a> · <a href="/hapi-rust/llms.txt">llms.txt</a>',
      copyright: 'Fork of tiann/hapi · Overbaker/hapi-rust'
    },

    search: {
      provider: 'local'
    }
  },

  vite: {
    server: {
      allowedHosts: true
    }
  }
})
