import { useEffect, useId, useRef, useState } from 'react'
import { HoverTooltip } from '@/components/HoverTooltip'
import {
    MACHINE_HEALTH_BAR_FILL_CLASS,
    formatStorageBytes,
    type MachineHealthMetricPresentation,
    type MachineHealthPresentation
} from '@/lib/machineHealth'
import { cn } from '@/lib/utils'
import { useTranslation } from '@/lib/use-translation'

function TooltipMetricStat(props: {
    metric: Pick<MachineHealthMetricPresentation, 'percent' | 'tone'>
    label: string
}) {
    return (
        // `contents`: the cells join the tooltip's shared grid (see MachineHealthTooltipBody).
        <span className="contents">
            <span className="whitespace-nowrap text-[var(--app-hint)]">{props.label}</span>
            <span
                className={cn(
                    'whitespace-nowrap font-semibold tabular-nums',
                    props.metric.tone !== 'ok' ? 'text-[var(--app-fg)]' : 'text-[var(--app-fg)]/90'
                )}
            >
                {props.metric.percent}%
            </span>
            <span
                className="relative h-1.5 w-full overflow-hidden rounded-full bg-[var(--app-border)]/80"
                aria-hidden="true"
            >
                <span
                    className={cn('block h-full rounded-full', MACHINE_HEALTH_BAR_FILL_CLASS[props.metric.tone])}
                    style={{ width: `${Math.max(4, Math.min(100, props.metric.percent))}%` }}
                />
            </span>
        </span>
    )
}

function MachineHealthHint() {
    const { t } = useTranslation()
    const tooltipId = useId()
    const [clickOpen, setClickOpen] = useState(false)
    const containerRef = useRef<HTMLSpanElement>(null)

    useEffect(() => {
        if (!clickOpen) return

        const closeOnOutsidePointer = (event: PointerEvent) => {
            if (!containerRef.current?.contains(event.target as Node)) {
                setClickOpen(false)
            }
        }
        document.addEventListener('pointerdown', closeOnOutsidePointer)
        return () => document.removeEventListener('pointerdown', closeOnOutsidePointer)
    }, [clickOpen])

    const target = (
        <button
            type="button"
            className="inline-flex h-3.5 w-3.5 items-center justify-center rounded-full border border-current text-[9px] font-semibold leading-none text-[var(--app-hint)]"
            aria-label={t('machine.health.tooltip.hint')}
            aria-expanded={clickOpen}
            aria-controls={tooltipId}
            onClick={(event) => {
                event.stopPropagation()
                if (clickOpen) {
                    event.currentTarget.blur()
                }
                setClickOpen((open) => !open)
            }}
            onKeyDown={(event) => {
                if (event.key === 'Escape') {
                    setClickOpen(false)
                }
            }}
        >
            ?
        </button>
    )

    return (
        <HoverTooltip
            id={tooltipId}
            target={target}
            side="bottom"
            align="start"
            open={clickOpen}
            containerRef={containerRef}
            hoverGroup="help"
            tooltipClassName="pointer-events-auto w-56"
        >
            {t('machine.health.tooltip.hint')}
        </HoverTooltip>
    )
}

export function MachineHealthTooltipBody(props: {
    presentation: MachineHealthPresentation
}) {
    const { t } = useTranslation()
    const { presentation } = props
    const statusKey = `machine.health.status.${presentation.status}` as const

    return (
        <span className="block space-y-1.5">
            <span className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5">
                <span className="inline-flex items-center gap-1 font-medium">
                    {t('machine.health.tooltip.title')}
                    <MachineHealthHint />
                </span>
                <span className="text-right text-[var(--app-fg)]">{t(statusKey)}</span>
            </span>
            {/* One grid for every row: the label column fits the widest label (e.g. "负载 (1 分钟)"). */}
            <span className="grid grid-cols-[max-content_2.75rem_minmax(4rem,1fr)] items-center gap-x-2 gap-y-1">
                {presentation.metrics.map((metric) => (
                    <TooltipMetricStat
                        key={metric.id}
                        metric={metric}
                        label={t(`machine.health.metric.${metric.id}`, { n: metric.percent })}
                    />
                ))}
                {presentation.disk ? (
                    <span className="contents" data-testid="machine-health-disk">
                        <TooltipMetricStat
                            metric={presentation.disk}
                            label={t('machine.health.metric.disk')}
                        />
                        <span className="col-span-2 col-start-2 -mt-0.5 text-[11px] tabular-nums text-[var(--app-hint)]">
                            {t('machine.health.tooltip.diskDetail', {
                                used: formatStorageBytes(presentation.disk.usedBytes),
                                total: formatStorageBytes(presentation.disk.totalBytes),
                                available: formatStorageBytes(presentation.disk.availableBytes),
                            })}
                        </span>
                    </span>
                ) : null}
                {presentation.loadAverage ? (
                    <>
                        <span className="whitespace-nowrap text-[var(--app-hint)]">
                            {t('machine.health.tooltip.loadShort')}
                        </span>
                        <span className="col-span-2 whitespace-nowrap font-semibold tabular-nums text-[var(--app-fg)]">
                            {presentation.cpuCount
                                ? t('machine.health.tooltip.loadValue', {
                                    load: presentation.loadAverage,
                                    cores: presentation.cpuCount
                                })
                                : presentation.loadAverage}
                        </span>
                    </>
                ) : null}
                {presentation.uptimeDetail ? (
                    <>
                        <span className="whitespace-nowrap text-[var(--app-hint)]">
                            {t('machine.health.tooltip.uptimeShort')}
                        </span>
                        <span className="col-span-2 whitespace-nowrap font-semibold tabular-nums text-[var(--app-fg)]">
                            {presentation.uptimeDetail}
                        </span>
                    </>
                ) : null}
            </span>
        </span>
    )
}
