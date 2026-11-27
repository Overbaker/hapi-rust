import { DEFAULT_NAMESPACE } from '../utils/accessToken'
import type { Settings } from './settings'

/**
 * Settings each namespace owns. The default namespace keeps them at the top
 * level of settings.json; every other namespace gets its own entry under
 * `namespaceSettings`, so one namespace's choices never reach another.
 */
export const NAMESPACE_SCOPED_SETTING_KEYS = [
    'sessionSummaryContract',
    'sessionSummaryInChat',
    'autoBridgeTransientModelErrors',
    'translateInputByDefault',
    'peerSpawnDefaults'
] as const

export type NamespaceScopedSettings = Pick<Settings, typeof NAMESPACE_SCOPED_SETTING_KEYS[number]>

export function getNamespaceSettings(settings: Settings, namespace: string): NamespaceScopedSettings {
    if (namespace !== DEFAULT_NAMESPACE) {
        const all = settings.namespaceSettings
        return all && Object.hasOwn(all, namespace) ? all[namespace] ?? {} : {}
    }
    const scoped: NamespaceScopedSettings = {}
    for (const key of NAMESPACE_SCOPED_SETTING_KEYS) {
        if (settings[key] !== undefined) {
            Object.assign(scoped, { [key]: settings[key] })
        }
    }
    return scoped
}

/** Returns a copy of `settings` with `patch` applied to one namespace only. */
export function patchNamespaceSettings(
    settings: Settings,
    namespace: string,
    patch: NamespaceScopedSettings
): Settings {
    if (namespace === DEFAULT_NAMESPACE) {
        return { ...settings, ...patch }
    }
    return {
        ...settings,
        namespaceSettings: {
            ...settings.namespaceSettings,
            [namespace]: { ...getNamespaceSettings(settings, namespace), ...patch }
        }
    }
}
