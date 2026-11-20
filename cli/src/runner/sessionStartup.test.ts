import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanupFailedStartupAfterExit, fallbackSessionId, reapExitedProcessGroup, remainingStartupBudget, RUST_WRAPPER_FALLBACK_EXIT_CODE, rustSessionMarker, sessionStartupTimeout, waitForRecoverableStartup, wrapperRetryState } from './sessionStartup';
import { EventEmitter } from 'node:events';
import { spawn } from 'node:child_process';
import { createSpawnDeduplicator } from './run';

afterEach(() => vi.useRealTimers());
describe('failed startup worktree cleanup', () => {
    const child = () => Object.assign(new EventEmitter(), { exitCode: null as number | null, signalCode: null as NodeJS.Signals | null });
    it('waits for confirmed exit after an unconfirmed startup failure and cleans only once', async () => {
        vi.useFakeTimers();
        const process = child(); const cleanup = vi.fn();
        const startup = waitForRecoverableStartup<string>({ timeoutMs: 30_000, timeoutMessage: 'Still starting', sessionId: id => id, onLateReady: vi.fn(),
            onLateFailure: () => cleanupFailedStartupAfterExit(process, () => true, cleanup) });
        await vi.advanceTimersByTimeAsync(30_000); await startup.result;
        startup.fail('Process error'); startup.fail('Duplicate error');
        expect(cleanup).not.toHaveBeenCalled();
        process.exitCode = 1; process.emit('exit', 1); process.emit('exit', 1);
        expect(cleanup).toHaveBeenCalledTimes(1);
    });
    it.each(['exit', 'signal'])('cleans immediately when %s already proved the child dead', proof => {
        const process = child(); const cleanup = vi.fn();
        if (proof === 'exit') process.exitCode = 1; else process.signalCode = 'SIGTERM';
        cleanupFailedStartupAfterExit(process, () => true, cleanup);
        expect(cleanup).toHaveBeenCalledTimes(1);
        expect(process.listenerCount('exit')).toBe(0);
    });
    it('preserves the worktree if the process later reports a successful startup before exit', () => {
        const process = child(); const cleanup = vi.fn(); let confirmed = false;
        cleanupFailedStartupAfterExit(process, () => !confirmed, cleanup);
        confirmed = true; process.emit('exit', 0);
        expect(cleanup).not.toHaveBeenCalled();
    });
});
describe('recoverable Codex startup', () => {
    const setup = () => {
        vi.useFakeTimers();
        const onLateReady = vi.fn();
        return { onLateReady, startup: waitForRecoverableStartup<string>({ timeoutMs: 30_000, timeoutMessage: 'Still starting', sessionId: id => id, onLateReady }) };
    };
    it('accepts fresh Codex startup after the former 15 second cutoff', async () => {
        const { startup, onLateReady } = setup();
        await vi.advanceTimersByTimeAsync(23_324);
        startup.confirm('requested');
        await expect(startup.result).resolves.toEqual({ type: 'success', sessionId: 'requested' });
        expect(onLateReady).not.toHaveBeenCalled();
    });
    it('returns an unconfirmed result at the deadline and accepts exactly one late confirmation', async () => {
        const { startup, onLateReady } = setup();
        await vi.advanceTimersByTimeAsync(30_000);
        await expect(startup.result).resolves.toEqual({ type: 'error', errorMessage: 'Still starting' });
        startup.confirm('requested'); startup.confirm('duplicate'); startup.fail('later close');
        expect(onLateReady).toHaveBeenCalledExactlyOnceWith('requested');
    });
    it.each([false, true])('does not recover a child that exited, including after timeout=%s', async expired => {
        const { startup, onLateReady } = setup();
        if (expired) await vi.advanceTimersByTimeAsync(30_000);
        startup.fail('Child exited'); startup.confirm('late');
        if (!expired) await expect(startup.result).resolves.toEqual({ type: 'error', errorMessage: 'Child exited' });
        expect(onLateReady).not.toHaveBeenCalled();
        expect(vi.getTimerCount()).toBe(0);
    });
    it('replaces an unconfirmed diagnostic with the eventual process failure exactly once', async () => {
        vi.useFakeTimers();
        const onLateFailure = vi.fn();
        const startup = waitForRecoverableStartup<string>({ timeoutMs: 30_000, timeoutMessage: 'Still starting', sessionId: id => id, onLateReady: vi.fn(), onLateFailure });
        await vi.advanceTimersByTimeAsync(30_000); await startup.result;
        startup.fail('Process exited with code 1'); startup.fail('Duplicate exit');
        expect(onLateFailure).toHaveBeenCalledExactlyOnceWith('Process exited with code 1');
    });
    it('keeps the live-child dedupe until late readiness, then permits a new child only after exit', async () => {
        vi.useFakeTimers();
        let calls = 0;
        let startup!: ReturnType<typeof waitForRecoverableStartup<string>>;
        const dedupe = createSpawnDeduplicator(async options => {
            calls++; dedupe.markChildAlive(options.existingSessionId!);
            startup = waitForRecoverableStartup<string>({ timeoutMs: 30_000, timeoutMessage: 'Still starting', sessionId: id => id,
                onLateReady: id => dedupe.recoverChild(id, { type: 'success', sessionId: id }) });
            return startup.result;
        });
        const input = { directory: '/tmp', existingSessionId: 'requested' };
        const first = dedupe(input);
        await vi.advanceTimersByTimeAsync(30_000);
        expect((await first).type).toBe('error');
        expect((await dedupe(input)).type).toBe('error'); expect(calls).toBe(1);
        startup.confirm('requested');
        await expect(dedupe(input)).resolves.toEqual({ type: 'success', sessionId: 'requested' });
        dedupe.onChildExited('requested');
        const second = dedupe(input); startup.fail('exit'); await second;
        expect(calls).toBe(2);
    });
});
describe('startup deadlines', () => {
    it('deduplicates a reserved fresh row against a concurrent existing-row retry', async () => {
        let complete!: (value: { type: 'success'; sessionId: string }) => void;
        const spawn = vi.fn(() => new Promise<{ type: 'success'; sessionId: string }>(resolve => { complete = resolve; }));
        const dedupe = createSpawnDeduplicator(spawn);
        const fresh = dedupe({ directory: '/tmp', reservedSessionId: 'reserved' });
        const retry = dedupe({ directory: '/tmp', existingSessionId: 'reserved' });
        expect(spawn).toHaveBeenCalledTimes(1);
        complete({ type: 'success', sessionId: 'reserved' });
        await expect(fresh).resolves.toEqual(await retry);
    });
    it('gives fresh and resumed Codex the same 30 second budget and other agents 15 seconds', () => {
        expect(sessionStartupTimeout('codex')).toBe(30_000);
        expect(sessionStartupTimeout('claude')).toBe(15_000);
        expect(sessionStartupTimeout(undefined)).toBe(15_000);
    });
    it.each(['', 'bad', '0', '-1', 'Infinity'])('ignores invalid override %s', value => {
        expect(sessionStartupTimeout('codex', value)).toBe(30_000);
    });
    it('honors a positive configured deadline for either agent', () => {
        expect(sessionStartupTimeout('codex', '1234')).toBe(1234);
        expect(sessionStartupTimeout('claude', '1234')).toBe(1234);
    });
});

describe('Rust wrapper retry', () => {
    type Outcome = { kind: 'missing' } | { kind: 'kill' } | { kind: 'timeout' } | { kind: 'exit'; code: number; afterWebhook: boolean };
    const codes = [0, 1, 2, 86, 137];
    function next(seed: number) { return (Math.imul(seed, 1664525) + 1013904223) >>> 0; }
    function simulate(outcomes: Outcome[]): { spawns: string[]; retriedAfterWebhook: boolean } {
        const spawns: string[] = [];
        let retriedAfterWebhook = false;
        let runtime: 'rust' | 'ts' = outcomes[0]?.kind === 'missing' ? 'ts' : 'rust';
        spawns.push(runtime);
        let retry = wrapperRetryState(runtime);
        for (const outcome of outcomes.slice(outcomes[0]?.kind === 'missing' ? 1 : 0)) {
            if (outcome.kind === 'missing') continue;
            if (outcome.kind === 'timeout') { retry('timeout'); break; }
            const afterWebhook = outcome.kind === 'exit' && outcome.afterWebhook;
            if (afterWebhook) retry('webhook');
            const exitCode = outcome.kind === 'exit' ? outcome.code : null;
            if (!afterWebhook && retry('exit-before-webhook', exitCode)) {
                runtime = 'ts';
                spawns.push(runtime);
                retry = wrapperRetryState(runtime);
                continue;
            }
            if (afterWebhook && retry('exit-before-webhook', exitCode)) retriedAfterWebhook = true;
            break;
        }
        return { spawns, retriedAfterWebhook };
    }

    it('spawns at most twice and retries only after a pre-webhook exit 86 (seeded property)', () => {
        let seed = Number(process.env.HAPI_PBT_SEED ?? 14752);
        for (let run = 0; run < 2000; run++) {
            const outcomes: Outcome[] = [];
            seed = next(seed);
            const length = 1 + (seed % 5);
            for (let i = 0; i < length; i++) {
                seed = next(seed);
                const pick = seed % 4;
                if (pick === 0) outcomes.push({ kind: 'missing' });
                else if (pick === 1) outcomes.push({ kind: seed % 8 < 4 ? 'kill' : 'timeout' });
                else outcomes.push({ kind: 'exit', code: codes[(seed >>> 3) % codes.length]!, afterWebhook: (seed >>> 7) % 2 === 1 });
            }
            const { spawns, retriedAfterWebhook } = simulate(outcomes);
            const first = outcomes[0]!;
            const expectRetry = first.kind === 'exit' && first.code === 86 && !first.afterWebhook;
            expect(spawns.length, JSON.stringify(outcomes)).toBeLessThanOrEqual(2);
            expect(retriedAfterWebhook).toBe(false);
            expect(spawns, JSON.stringify(outcomes)).toEqual(expectRetry ? ['rust', 'ts'] : [first.kind === 'missing' ? 'ts' : 'rust']);
        }
    });

    it.each([
        ['webhook', null], ['timeout', null], ['exit-before-webhook', 1], ['exit-before-webhook', null], ['process-error-before-webhook', 86],
    ] as const)('closes the retry window on %s (exit %s)', (event, code) => {
        const retry = wrapperRetryState('rust');
        expect(retry(event, code)).toBe(false);
        expect(retry('exit-before-webhook', RUST_WRAPPER_FALLBACK_EXIT_CODE)).toBe(false);
    });

    it('never retries a TypeScript wrapper', () => {
        expect(wrapperRetryState('ts')('exit-before-webhook', RUST_WRAPPER_FALLBACK_EXIT_CODE)).toBe(false);
    });

    it('closes the Codex retry window synchronously when the deadline fires', async () => {
        vi.useFakeTimers();
        const replace = vi.fn();
        const startup = waitForRecoverableStartup<string>({
            timeoutMs: 10, timeoutMessage: 'expired', sessionId: id => id, onLateReady: vi.fn(),
            onTimeout: () => { expect(startup.replace(replace)).toBe(false); },
        });
        vi.advanceTimersByTime(10);
        expect(replace).not.toHaveBeenCalled();
        await expect(startup.result).resolves.toEqual({ type: 'error', errorMessage: 'expired' });
    });

    it('resolves a waiting startup with the replacement result', async () => {
        const startup = waitForRecoverableStartup<string>({ timeoutMs: 1_000, timeoutMessage: 'expired', sessionId: id => id, onLateReady: vi.fn() });
        expect(startup.replace(() => null)).toBe(false);
        expect(startup.replace(async () => ({ type: 'success', sessionId: 's' }))).toBe(true);
        expect(startup.replace(async () => ({ type: 'success', sessionId: 'other' }))).toBe(false);
        await expect(startup.result).resolves.toEqual({ type: 'success', sessionId: 's' });
    });

    it('reads the session marker across chunks and ignores malformed or overlong lines', () => {
        const id = '0b9f2a4e-1c3d-4e5f-8a6b-7c8d9e0f1a2b';
        const marker = rustSessionMarker();
        marker.write('noise\nHAPI_RUST_WRAPPER_SESSION_ID=not-a-uuid\nHAPI_RUST_WRAPPER_SESSION_ID=');
        marker.write(`${id}\r`);
        expect(marker.id).toBeUndefined();
        marker.finishLine();
        expect(marker.id).toBe(id);
        const overlong = rustSessionMarker();
        overlong.write(`HAPI_RUST_WRAPPER_SESSION_ID=${id}${'x'.repeat(100)}\n`);
        expect(overlong.id).toBeUndefined();
    });
});

describe('Rust wrapper fallback helpers', () => {
    const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch { return false; } };
    it.skipIf(process.platform === 'win32')('reaps a member left in an exited leader\'s process group', async () => {
        const leader = spawn('sh', ['-c', 'sleep 30 & echo $!'], { detached: true, stdio: ['ignore', 'pipe', 'ignore'] });
        const output = await new Promise<string>(resolve => { let text = ''; leader.stdout!.on('data', d => { text += d; }); leader.once('exit', () => resolve(text)); });
        const member = Number(output.trim());
        expect(alive(member)).toBe(true);
        await expect(reapExitedProcessGroup(leader.pid!)).resolves.toBe(true);
        expect(alive(member)).toBe(false);
    });
    it.skipIf(process.platform === 'win32')('does not signal a live process that now holds the leader PID', async () => {
        const reused = spawn('sleep', ['30'], { detached: true, stdio: 'ignore' });
        try {
            await new Promise(resolve => reused.once('spawn', resolve));
            await expect(reapExitedProcessGroup(reused.pid!)).resolves.toBe(true);
            expect(reused.exitCode).toBeNull();
            expect(alive(reused.pid!)).toBe(true);
        } finally {
            reused.kill('SIGKILL');
        }
    });
    it('binds a TypeScript retry to an id learned at runtime and reuses an in-flight start', async () => {
        let release!: (result: { type: 'success'; sessionId: string }) => void;
        const spawnOnce = vi.fn(() => new Promise<{ type: 'success'; sessionId: string }>(resolve => { release = resolve; }));
        const dedupe = createSpawnDeduplicator(spawnOnce);
        const inFlight = dedupe({ directory: '/tmp', existingSessionId: 'learned' });
        const start = vi.fn(async () => ({ type: 'success' as const, sessionId: 'learned' }));
        const claimed = dedupe.claim('learned', start);
        release({ type: 'success', sessionId: 'learned' });
        await expect(claimed).resolves.toEqual({ type: 'success', sessionId: 'learned' });
        await inFlight;
        expect(start).not.toHaveBeenCalled();
        const fresh = vi.fn(async () => ({ type: 'success' as const, sessionId: 'other' }));
        await expect(dedupe.claim('other', fresh)).resolves.toEqual({ type: 'success', sessionId: 'other' });
        await expect(dedupe({ directory: '/tmp', existingSessionId: 'other' })).resolves.toEqual({ type: 'success', sessionId: 'other' });
        expect(fresh).toHaveBeenCalledTimes(1);
        expect(spawnOnce).toHaveBeenCalledTimes(1);
    });
});

describe('Rust wrapper retry target and budget', () => {
    it('keeps the requested row, or the one row both the webhook and stderr name', () => {
        expect(fallbackSessionId({ agent: 'claude', existingSessionId: 'row', learnedSessionId: 'other' })).toEqual({ sessionId: 'row' });
        expect(fallbackSessionId({ agent: 'claude', learnedSessionId: 'learned' })).toEqual({ sessionId: 'learned' });
        expect(fallbackSessionId({ agent: 'claude', webhookSessionId: 'same', learnedSessionId: 'same' })).toEqual({ sessionId: 'same' });
        expect(fallbackSessionId({ agent: 'claude' })).toEqual({ sessionId: undefined });
    });
    it('refuses a retry when the webhook names a different row than the verified stderr id', () => {
        expect(fallbackSessionId({ agent: 'claude', webhookSessionId: 'late-root', learnedSessionId: 'verified' })).toBeNull();
    });
    it('refuses to hand a fresh Codex row to TypeScript but keeps a fresh Codex start or a requested row', () => {
        expect(fallbackSessionId({ agent: 'codex', learnedSessionId: 'fresh-row' })).toBeNull();
        expect(fallbackSessionId({ agent: 'codex' })).toEqual({ sessionId: undefined });
        expect(fallbackSessionId({ agent: 'codex', existingSessionId: 'bound' })).toEqual({ sessionId: 'bound' });
    });
    it('shares one startup deadline across both attempts', () => {
        expect(remainingStartupBudget(10_000, 4_000)).toBe(6_000);
        expect(remainingStartupBudget(10_000, 10_000)).toBeNull();
        expect(remainingStartupBudget(10_000, 12_000)).toBeNull();
    });
});
