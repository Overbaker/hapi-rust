import { mock } from 'bun:test'
mock.module('@/ui/logger', () => ({ logger: { debug() {} } }))
const { MessageQueue2 } = await import('../../../cli/src/utils/MessageQueue2')
const queue = new MessageQueue2<number>(String)
const held = new Map()
let seed = 126
const next = () => (seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0) >>> 12
const rows = []
for (let n = 0; n < 2048; n++) {
    const op = next() % 13, id = String(next() % 5), mode = next() % 2
    let result = null
    switch (op) {
        case 0: case 1: queue.push(id, mode, id); break
        case 2: queue.unshift(id, mode, id); break
        case 3: queue.pushIsolated(id, mode, id); break
        case 4: queue.pushIsolateAndClear(id, mode, id); break
        case 5: {
            const r = queue.takeByLocalId(id)
            if (r) held.set(id, r)
            result = !!r
            break
        }
        case 6: result = held.has(id) && queue.beginReservationDispatch(held.get(id)); break
        case 7: result = held.has(id) && queue.markReservationIndeterminate(held.get(id)); break
        case 8: result = held.has(id) && queue.restoreReservation(held.get(id)); break
        case 9: result = held.has(id) && queue.commitReservation(held.get(id)); break
        case 10: result = queue.cancelByLocalId(id); break
        case 11: result = queue.size() ? (await queue.waitForMessagesAndGetAsString())!.message : null; break
        case 12: queue.reset({preserveDispatchingReservations: !!mode}); break
    }
    rows.push([op,id,mode,result,queue.pendingLocalIds()])
}
console.log(JSON.stringify(rows))
