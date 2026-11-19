import type { SpawnSessionResult } from '@/modules/common/rpcTypes';
import type { ChildProcess } from 'node:child_process';

/** A failed startup may still own a live process using its working directory. */
export function cleanupFailedStartupAfterExit(
    child: Pick<ChildProcess, 'exitCode' | 'signalCode'> & { once(event: 'exit', listener: () => void): unknown },
    shouldCleanup: () => boolean,
    cleanup: () => void,
): void {
    const onExit = () => { if (shouldCleanup()) cleanup(); };
    if (child.exitCode !== null || child.signalCode !== null) onExit();
    else child.once('exit', onExit);
}

export function sessionStartupTimeout(agent: string | undefined, configured?: string): number {
    const explicit = Number(configured);
    if (Number.isFinite(explicit) && explicit > 0) return explicit;
    return agent === 'codex' ? 30_000 : 15_000;
}

/** A caller's deadline does not prove that a shared execution failed to start. */
export function waitForRecoverableStartup<T>(options: {
    timeoutMs: number;
    timeoutMessage: string;
    sessionId: (session: T) => string;
    onTimeout?: () => void;
    onLateReady: (session: T) => void;
    onLateFailure?: (errorMessage: string) => void;
}) {
    let phase: 'waiting' | 'unconfirmed' | 'finished' = 'waiting';
    let resolve!: (result: SpawnSessionResult | Promise<SpawnSessionResult>) => void;
    const result = new Promise<SpawnSessionResult>(done => { resolve = done; });
    const timer = setTimeout(() => {
        if (phase !== 'waiting') return;
        phase = 'unconfirmed';
        options.onTimeout?.();
        resolve({ type: 'error', errorMessage: options.timeoutMessage });
    }, options.timeoutMs);
    return {
        result,
        replace(startRetry: () => Promise<SpawnSessionResult> | null): boolean {
            if (phase !== 'waiting') return false;
            const retry = startRetry();
            if (!retry) return false;
            clearTimeout(timer);
            phase = 'finished';
            resolve(retry);
            return true;
        },
        confirm(session: T): void {
            if (phase === 'finished') return;
            const late = phase === 'unconfirmed';
            phase = 'finished'; clearTimeout(timer);
            if (late) options.onLateReady(session);
            resolve({ type: 'success', sessionId: options.sessionId(session) });
        },
        fail(errorMessage: string): void {
            if (phase === 'finished') return;
            const late = phase === 'unconfirmed';
            phase = 'finished'; clearTimeout(timer);
            if (late) options.onLateFailure?.(errorMessage);
            resolve({ type: 'error', errorMessage });
        },
    };
}

export type WrapperFailure = 'no-pid' | 'exit-before-webhook' | 'process-error-before-webhook'
export type WrapperFallback = { reason: WrapperFailure; exitCode: number | null }

export function wrapperRetryState(runtime: 'rust' | 'ts') {
    let eligible = runtime === 'rust'
    return (event: WrapperFailure | 'webhook' | 'timeout'): boolean => {
        const retry = eligible && event !== 'webhook' && event !== 'timeout'
        eligible = false
        return retry
    }
}

export const hubUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export function rustSessionMarker() {
    const prefix = 'HAPI_RUST_WRAPPER_SESSION_ID=';
    const maxLength = prefix.length + 36 + 1;
    let line = '';
    let overlong = false;
    let sessionId: string | undefined;
    function finishLine() {
        if (!sessionId && !overlong && line.startsWith(prefix)) {
            const id = line.slice(prefix.length).replace(/\r$/, '');
            if (id.length === 36 && hubUuid.test(id)) sessionId = id;
        }
        line = '';
        overlong = false;
    }
    return {
        write(chunk: string) {
            let start = 0;
            while (!sessionId && start < chunk.length) {
                const newline = chunk.indexOf('\n', start);
                const end = newline < 0 ? chunk.length : newline;
                if (line.length + end - start > maxLength) overlong = true;
                if (!overlong) line += chunk.slice(start, end);
                if (newline < 0) break;
                finishLine();
                start = newline + 1;
            }
        },
        finishLine,
        get id() { return sessionId; },
    };
}
