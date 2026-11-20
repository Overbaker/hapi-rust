import { afterEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { SessionChat } from './SessionChat'
import { I18nProvider } from '@/lib/i18n-context'
import { ToastProvider } from '@/lib/toast-context'
import type { ApiClient } from '@/api/client'
import type { Session } from '@/types/api'

vi.mock('@tanstack/react-router', async (importOriginal) => ({
    ...(await importOriginal<Record<string, unknown>>()),
    useNavigate: () => async () => {}
}))

class ResizeObserverStub {
    observe() {}
    unobserve() {}
    disconnect() {}
}
if (typeof globalThis.ResizeObserver === 'undefined') {
    ;(globalThis as Record<string, unknown>).ResizeObserver = ResizeObserverStub
}
if (typeof Element.prototype.scrollTo !== 'function') {
    Element.prototype.scrollTo = function scrollTo() {}
}

function makeSession(overrides: Partial<Session> = {}): Session {
    return {
        id: 'ses-hib-test',
        namespace: 'default',
        seq: 1,
        createdAt: 1000,
        updatedAt: 2000,
        active: false,
        activeAt: 1500,
        metadata: {
            flavor: 'claude',
            path: '/work/project',
            host: 'mac',
            name: 'Feature work',
            claudeSessionId: 'claude-1',
        },
        metadataVersion: 1,
        agentState: null,
        agentStateVersion: 1,
        thinking: false,
        thinkingAt: 0,
        model: null,
        modelReasoningEffort: null,
        effort: null,
        serviceTier: null,
        ...overrides,
    }
}

function renderChat(session: Session, opts?: { isWaking?: boolean; locale?: 'en' | 'zh-CN' }) {
    if (opts?.locale) {
        window.localStorage.setItem('hapi-lang', opts.locale)
    }
    const api = {
        getMachines: vi.fn(async () => ({ machines: [] })),
        getSessions: vi.fn(async () => ({ sessions: [] })),
        getScratchlist: vi.fn(async () => ({ entries: [] })),
        getFeatures: vi.fn(async () => null),
        getHubSettings: vi.fn(async () => ({})),
        fetchTranscriptionProviders: vi.fn(async () => []),
    } as unknown as ApiClient
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    return render(
        <QueryClientProvider client={queryClient}>
            <I18nProvider>
                <ToastProvider>
                    <SessionChat
                        api={api}
                        session={session}
                        messages={[]}
                        messagesWarning={null}
                        hasMoreMessages={false}
                        isSyncingTail={false}
                        isCatchingUpFromCache={false}
                        isLoadingMoreMessages={false}
                        isSending={false}
                        isWaking={opts?.isWaking}
                        viewMode="tail"
                        messagesVersion={0}
                        historyVersion={0}
                        tailRevision={0}
                        onBack={() => {}}
                        onRefresh={() => {}}
                        onLoadMore={async () => 'terminal-stop' as never}
                        onCancelLoadMore={() => {}}
                        onSend={async () => false}
                        onViewModeChange={() => {}}
                        onJumpToTail={() => {}}
                    />
                </ToastProvider>
            </I18nProvider>
        </QueryClientProvider>
    )
}

describe('SessionChat hibernated vs archived chat hints', () => {
    afterEach(() => {
        cleanup()
        window.localStorage.clear()
        vi.restoreAllMocks()
    })

    it('renders the calm Hibernated banner, header pill, and status bar indicator for hibernated sessions', () => {
        renderChat(makeSession({
            active: false,
            metadata: {
                flavor: 'claude',
                path: '/work/project',
                host: 'mac',
                name: 'Sleeper',
                claudeSessionId: 'claude-1',
                lifecycleState: 'hibernated',
            },
        }))

        const banner = screen.getByTestId('session-hibernated-banner')
        expect(banner).toHaveTextContent('Hibernated — send a message to wake it')
        expect(banner).toHaveClass('bg-[var(--app-badge-hibernated-bg)]', 'text-[var(--app-badge-hibernated-text)]')
        expect(banner.querySelector('svg')).not.toBeNull()

        expect(screen.queryByTestId('session-inactive-banner')).toBeNull()
        expect(screen.getByTestId('session-header-hibernated')).toHaveTextContent('Hibernated')
        expect(screen.getByTestId('status-bar-hibernated')).toHaveTextContent('Hibernated')
    })

    it('shows the Chinese hibernated hint when locale is zh-CN', () => {
        renderChat(makeSession({
            active: false,
            metadata: {
                flavor: 'claude',
                path: '/work/project',
                host: 'mac',
                name: 'Sleeper',
                claudeSessionId: 'claude-1',
                lifecycleState: 'hibernated',
            },
        }), { locale: 'zh-CN' })

        expect(screen.getByTestId('session-hibernated-banner')).toHaveTextContent('已休眠 — 发送消息即可唤醒')
        expect(screen.getByTestId('session-header-hibernated')).toHaveTextContent('已休眠')
    })

    it('switches the hibernated banner copy to Waking session… while waking', () => {
        renderChat(makeSession({
            active: false,
            metadata: {
                flavor: 'claude',
                path: '/work/project',
                host: 'mac',
                name: 'Sleeper',
                claudeSessionId: 'claude-1',
                lifecycleState: 'hibernated',
            },
        }), { isWaking: true })

        expect(screen.getByTestId('session-hibernated-banner')).toHaveTextContent('Waking session…')
        expect(screen.queryByTestId('session-inactive-banner')).toBeNull()
    })

    it('keeps archived sessions (both hub and cli-stopped) on the muted inactive banner without hibernated chrome', () => {
        for (const archivedBy of ['hub', 'cli'] as const) {
            renderChat(makeSession({
                active: false,
                metadata: {
                    flavor: 'claude',
                    path: '/work/project',
                    host: 'mac',
                    name: `Archived ${archivedBy}`,
                    claudeSessionId: 'claude-1',
                    lifecycleState: 'archived',
                    archivedBy,
                },
            }))

            const inactiveBanner = screen.getByTestId('session-inactive-banner')
            expect(inactiveBanner).toHaveTextContent('This session is inactive. Send a message to resume.')
            expect(inactiveBanner).toHaveClass('bg-[var(--app-subtle-bg)]', 'text-[var(--app-hint)]')
            expect(screen.queryByTestId('session-hibernated-banner')).toBeNull()
            expect(screen.queryByTestId('session-header-hibernated')).toBeNull()
            expect(screen.queryByTestId('status-bar-hibernated')).toBeNull()
            cleanup()
        }
    })

    it('renders neither inactive nor hibernated banner for active sessions', () => {
        renderChat(makeSession({
            active: true,
            metadata: {
                flavor: 'claude',
                path: '/work/project',
                host: 'mac',
                name: 'Live session',
                claudeSessionId: 'claude-1',
                lifecycleState: 'running',
            },
        }))

        expect(screen.queryByTestId('session-hibernated-banner')).toBeNull()
        expect(screen.queryByTestId('session-inactive-banner')).toBeNull()
        expect(screen.queryByTestId('session-header-hibernated')).toBeNull()
    })
})
