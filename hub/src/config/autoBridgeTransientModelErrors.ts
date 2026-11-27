import {
    getSettingsFile,
    readSettingsOrThrow,
    updateSettings
} from './settings'
import { getNamespaceSettings, patchNamespaceSettings } from './namespaceSettings'

/**
 * Hub-persisted opt-in for Cursor auto-bridge after transient model errors.
 * Default is off (undefined / false). Applied to CLI processes on session
 * create/get so unattended / restarted sessions honor Settings.
 */
export async function readAutoBridgeTransientModelErrorsEnabled(dataDir: string, namespace: string): Promise<boolean> {
    const settings = await readSettingsOrThrow(getSettingsFile(dataDir))
    return getNamespaceSettings(settings, namespace).autoBridgeTransientModelErrors === true
}

export async function writeAutoBridgeTransientModelErrorsEnabled(
    dataDir: string,
    namespace: string,
    enabled: boolean
): Promise<boolean> {
    return updateSettings(getSettingsFile(dataDir), (current) => {
        const settings = patchNamespaceSettings(current, namespace, { autoBridgeTransientModelErrors: enabled })
        return {
            settings,
            result: enabled
        }
    })
}
