import { memo, useEffect, useRef } from 'react'
import type { Suggestion } from '@/hooks/useActiveSuggestions'
import { HibernatedIcon } from '@/components/icons'

interface AutocompleteProps {
    suggestions: readonly Suggestion[]
    selectedIndex: number
    onSelect: (index: number) => void
}

/**
 * Autocomplete suggestions list component
 */
export const Autocomplete = memo(function Autocomplete(props: AutocompleteProps) {
    const { suggestions, selectedIndex, onSelect } = props
    const listRef = useRef<HTMLDivElement>(null)

    useEffect(() => {
        if (selectedIndex < 0 || selectedIndex >= suggestions.length) return
        const listEl = listRef.current
        if (!listEl) return
        const selectedEl = listEl.querySelector<HTMLButtonElement>(
            `[data-suggestion-index="${selectedIndex}"]`
        )
        selectedEl?.scrollIntoView({ block: 'nearest' })
    }, [selectedIndex, suggestions.length])

    if (suggestions.length === 0) {
        return null
    }

    return (
        <div ref={listRef}>
            {suggestions.map((suggestion, index) => (
                <button
                    key={suggestion.key}
                    type="button"
                    data-suggestion-index={index}
                    aria-expanded={suggestion.action?.expanded}
                    className={`flex w-full cursor-pointer flex-col items-start gap-0.5 px-3 py-2 text-left text-sm transition-colors ${
                        index > 0 && !suggestion.sessionMention && !suggestion.action
                            && (suggestions[index - 1]?.sessionMention || suggestions[index - 1]?.action)
                            ? 'border-t border-[var(--app-divider)]' : ''
                    } ${
                        index === selectedIndex
                            ? 'bg-[var(--app-button)] text-[var(--app-button-text)]'
                            : 'text-[var(--app-fg)] hover:bg-[var(--app-secondary-bg)]'
                    }`}
                    onClick={() => onSelect(index)}
                    onMouseDown={(e) => e.preventDefault()} // Prevent blur on textarea
                >
                    <span className={`flex w-full items-center gap-1.5 font-medium ${suggestion.action ? 'border-t border-[var(--app-divider)] pt-2' : ''}`}>
                        {suggestion.action ? <span aria-hidden="true">{suggestion.action.expanded ? '▾ ' : '▸ '}</span> : null}
                        <span className="min-w-0 truncate">{suggestion.label}</span>
                        {suggestion.badge?.tone === 'hibernated' ? (
                            <span
                                data-testid="autocomplete-hibernated-badge"
                                className={`inline-flex shrink-0 items-center gap-1 rounded-full border px-1.5 py-0.5 text-[10px] font-medium leading-none ${
                                    index === selectedIndex
                                        ? 'border-current/40 bg-current/15 text-current'
                                        : 'border-[var(--app-badge-hibernated-border)] bg-[var(--app-badge-hibernated-bg)] text-[var(--app-badge-hibernated-text)]'
                                }`}
                            >
                                <HibernatedIcon className="h-2.5 w-2.5 shrink-0" />
                                <span>{suggestion.badge.label}</span>
                            </span>
                        ) : null}
                    </span>
                    {suggestion.description && (
                        <span className={`w-full min-h-[2.25rem] text-xs leading-snug line-clamp-2 ${
                            index === selectedIndex
                                ? 'opacity-80'
                                : 'text-[var(--app-hint)]'
                        }`}>
                            {suggestion.description}
                        </span>
                    )}
                </button>
            ))}
        </div>
    )
})
