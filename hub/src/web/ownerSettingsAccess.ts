import type { Context, MiddlewareHandler } from 'hono'
import { getSettingsFile, readSettingsOrThrowCached, type Settings } from '../config/settings'
import { DEFAULT_NAMESPACE } from '../utils/accessToken'
import type { WebAppEnv } from './middleware/auth'

/** Owner-only settings are shared with other namespaces unless the owner turned it off. */
export function isOwnerSettingsShared(settings: Settings): boolean {
    return settings.shareOwnerSettingsWithNamespaces !== false
}

export function createOwnerSettingsAccessMiddleware(dataDir: string): MiddlewareHandler<WebAppEnv> {
    return async (c, next) => {
        if (c.get('namespace') === DEFAULT_NAMESPACE) {
            c.set('ownerSettingsAccess', true)
        } else {
            try {
                const settings = await readSettingsOrThrowCached(getSettingsFile(dataDir))
                c.set('ownerSettingsAccess', isOwnerSettingsShared(settings))
            } catch {
                c.set('ownerSettingsAccess', false)
            }
        }
        await next()
    }
}

export function hasOwnerSettingsAccess(c: Context<WebAppEnv>): boolean {
    return c.get('namespace') === DEFAULT_NAMESPACE || c.get('ownerSettingsAccess') === true
}
