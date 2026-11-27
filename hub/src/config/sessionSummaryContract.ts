import {
    getSettingsFile,
    readSettingsOrThrowCached,
    updateSettings
} from './settings'
import { getNamespaceSettings, patchNamespaceSettings } from './namespaceSettings'

/**
 * Hub-persisted opt-in for AGENT_NOTIFY_SUMMARY prompt injection.
 * Default is off (undefined / false). Env `HAPI_SESSION_SUMMARY_CONTRACT` on
 * the CLI process remains an escape hatch and is resolved client-side.
 */
export async function readSessionSummaryContractEnabled(dataDir: string, namespace: string): Promise<boolean> {
    const settings = await readSettingsOrThrowCached(getSettingsFile(dataDir))
    return getNamespaceSettings(settings, namespace).sessionSummaryContract === true
}

export async function writeSessionSummaryContractEnabled(
    dataDir: string,
    namespace: string,
    enabled: boolean
): Promise<boolean> {
    return updateSettings(getSettingsFile(dataDir), (current) => {
        const settings = patchNamespaceSettings(current, namespace, { sessionSummaryContract: enabled })
        return {
            settings,
            result: enabled
        }
    })
}
