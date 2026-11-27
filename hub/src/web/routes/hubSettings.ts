import { Hono } from 'hono'
import { UpdateHubSettingsRequestSchema, type HubSettingsResponse } from '@hapi/protocol'
import { mergePeerSpawnDefaults } from '@hapi/protocol/peerSpawnDefaults'
import {
    getSettingsFile,
    readSettingsOrThrowCached,
    updateSettings,
    type Settings
} from '../../config/settings'
import { applyPeerSpawnDefaultsPatch } from '../../config/peerSpawnDefaults'
import { writeAutoBridgeTransientModelErrorsEnabled } from '../../config/autoBridgeTransientModelErrors'
import {
    getNamespaceSettings,
    patchNamespaceSettings,
    type NamespaceScopedSettings
} from '../../config/namespaceSettings'
import type { SyncEngine } from '../../sync/syncEngine'
import type { WebAppEnv } from '../middleware/auth'
import { DEFAULT_NAMESPACE } from '../../utils/accessToken'
import { isOwnerSettingsShared } from '../ownerSettingsAccess'

function toHubSettings(all: Settings, namespace: string): HubSettingsResponse {
    const settings = getNamespaceSettings(all, namespace)
    return {
        sessionSummaryContract: settings.sessionSummaryContract === true,
        sessionSummaryInChat: settings.sessionSummaryInChat === true,
        autoBridgeTransientModelErrors: settings.autoBridgeTransientModelErrors === true,
        translateInputByDefault: settings.translateInputByDefault === true,
        peerSpawnDefaults: mergePeerSpawnDefaults(settings.peerSpawnDefaults),
        shareOwnerSettingsWithNamespaces: isOwnerSettingsShared(all),
        canUseOwnerSettings: namespace === DEFAULT_NAMESPACE || isOwnerSettingsShared(all)
    }
}

export function createHubSettingsRoutes(
    dataDir: string,
    getSyncEngine?: () => SyncEngine | null
): Hono<WebAppEnv> {
    const app = new Hono<WebAppEnv>()

    // Every namespace reads and writes its own copy of these settings.
    app.get('/hub-settings', async (c) => {
        c.header('Cache-Control', 'no-store')
        const settings = await readSettingsOrThrowCached(getSettingsFile(dataDir))
        return c.json(toHubSettings(settings, c.get('namespace')))
    })

    app.put('/hub-settings', async (c) => {
        const namespace = c.get('namespace')
        const json = await c.req.json().catch(() => null)
        const parsed = UpdateHubSettingsRequestSchema.safeParse(json)
        if (!parsed.success) {
            return c.json({ error: 'Invalid body' }, 400)
        }
        const providedFieldCount = [
            parsed.data.sessionSummaryContract,
            parsed.data.sessionSummaryInChat,
            parsed.data.autoBridgeTransientModelErrors,
            parsed.data.translateInputByDefault,
            parsed.data.shareOwnerSettingsWithNamespaces
        ].filter((value) => value !== undefined).length
        if (providedFieldCount > 1) {
            return c.json({ error: 'Update one hub setting per request' }, 400)
        }
        if (parsed.data.shareOwnerSettingsWithNamespaces !== undefined && namespace !== DEFAULT_NAMESPACE) {
            return c.json({ error: 'Only the hub owner can change sharing with other namespaces' }, 403)
        }
        if (parsed.data.translateInputByDefault === true) {
            const engine = getSyncEngine?.() ?? null
            if (engine && !engine.isInputTranslationAvailable()) {
                return c.json({ error: 'Input translation is not configured on this hub' }, 400)
            }
        }
        // Auto-bridge is not a plain settings write: SyncEngine fans the pref out
        // to live Cursor CLIs and rolls the disk value back if any session refuses.
        // Keep it outside the updateSettings transaction so the engine owns both
        // halves under its own lock; the write still goes through updateSettings
        // underneath, so the settings read cache stays write-through fresh.
        if (parsed.data.autoBridgeTransientModelErrors !== undefined) {
            const enabled = parsed.data.autoBridgeTransientModelErrors
            const engine = getSyncEngine?.() ?? null
            if (engine) {
                try {
                    // Disk write + fanout + rollback share SyncEngine's lock so
                    // concurrent PUTs cannot leave CLI prefs ahead of settings.json.
                    await engine.applyAutoBridgeTransientModelErrorsSetting(dataDir, namespace, enabled)
                } catch (error) {
                    const message = error instanceof Error
                        ? error.message
                        : 'Failed to update every active Cursor session'
                    return c.json({ error: message }, 409)
                }
            } else {
                await writeAutoBridgeTransientModelErrorsEnabled(dataDir, namespace, enabled)
            }
        }
        const response = await updateSettings(getSettingsFile(dataDir), (current) => {
            const patch: NamespaceScopedSettings = {}
            if (parsed.data.sessionSummaryContract !== undefined) {
                patch.sessionSummaryContract = parsed.data.sessionSummaryContract
            }
            if (parsed.data.sessionSummaryInChat !== undefined) {
                patch.sessionSummaryInChat = parsed.data.sessionSummaryInChat
            }
            if (parsed.data.translateInputByDefault !== undefined) {
                patch.translateInputByDefault = parsed.data.translateInputByDefault
            }
            if (parsed.data.peerSpawnDefaults !== undefined) {
                patch.peerSpawnDefaults = applyPeerSpawnDefaultsPatch(
                    current,
                    namespace,
                    parsed.data.peerSpawnDefaults
                )
            }
            const patched = patchNamespaceSettings(current, namespace, patch)
            const settings = parsed.data.shareOwnerSettingsWithNamespaces === undefined
                ? patched
                : { ...patched, shareOwnerSettingsWithNamespaces: parsed.data.shareOwnerSettingsWithNamespaces }
            return {
                settings,
                result: toHubSettings(settings, namespace)
            }
        })
        c.header('Cache-Control', 'no-store')
        return c.json(response)
    })

    return app
}
