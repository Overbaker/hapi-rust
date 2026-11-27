import { useQuery } from '@tanstack/react-query'
import { useAppContext } from '@/lib/app-context'
import { queryKeys } from '@/lib/query-keys'

export function getNamespaceFromToken(token: string): string | null {
    try {
        const payload = token.split('.')[1]
        if (!payload) return null
        const base64 = payload.replace(/-/g, '+').replace(/_/g, '/').padEnd(Math.ceil(payload.length / 4) * 4, '=')
        const decoded = JSON.parse(atob(base64)) as { ns?: unknown }
        return typeof decoded.ns === 'string' ? decoded.ns : null
    } catch {
        return null
    }
}

/**
 * Owner-only settings (PR awareness, voice credentials, storage) are open to
 * other namespaces while the owner keeps sharing on (the default).
 */
export function useOwnerSettingsAccess(): { isOwner: boolean; canUseOwnerSettings: boolean } {
    const { api, token } = useAppContext()
    const isOwner = Boolean(token) && getNamespaceFromToken(token) === 'default'
    const hubSettingsQuery = useQuery({
        queryKey: queryKeys.hubSettings,
        queryFn: async () => {
            if (!api) throw new Error('API unavailable')
            return await api.getHubSettings()
        },
        enabled: Boolean(api) && !isOwner,
        staleTime: 30_000,
        retry: false,
    })
    return { isOwner, canUseOwnerSettings: isOwner || hubSettingsQuery.data?.canUseOwnerSettings === true }
}
