export function summary(samples: number[]) {
    if (!samples.length) throw Error('No samples')
    const sorted = [...samples].sort((a, b) => a - b)
    const at = (p: number) => Number(sorted[Math.ceil(p * sorted.length) - 1].toFixed(3))
    return { n: sorted.length, p50: at(.50), p95: at(.95), p99: at(.99) }
}
