import { fileURLToPath } from 'node:url'
import { startTestHub } from './globalSetup'

export { teardown } from './globalSetup'
export async function setup() {
    await startTestHub(fileURLToPath(new URL('./fixtures/hibernate-clock.mjs', import.meta.url)))
}
