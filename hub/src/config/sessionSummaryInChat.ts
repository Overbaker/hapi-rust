import {
    getSettingsFile,
    readSettingsOrThrowCached,
    updateSettings
} from './settings'
import { getNamespaceSettings, patchNamespaceSettings } from './namespaceSettings'

/**
 * Hub-persisted opt-in to show AGENT_NOTIFY_SUMMARY in chat UI.
 * Default is off (undefined / false): render/copy strip the footer;
 * store, parse, FCM, and ledger capture keep the raw line.
 */
export async function readSessionSummaryInChatEnabled(dataDir: string, namespace: string): Promise<boolean> {
    const settings = await readSettingsOrThrowCached(getSettingsFile(dataDir))
    return getNamespaceSettings(settings, namespace).sessionSummaryInChat === true
}

export async function writeSessionSummaryInChatEnabled(
    dataDir: string,
    namespace: string,
    enabled: boolean
): Promise<boolean> {
    return updateSettings(getSettingsFile(dataDir), (current) => {
        const settings = patchNamespaceSettings(current, namespace, { sessionSummaryInChat: enabled })
        return {
            settings,
            result: enabled
        }
    })
}
