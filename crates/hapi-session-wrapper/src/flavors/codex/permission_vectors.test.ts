import { describe, expect, it, mock, afterAll } from 'bun:test'
import { readFileSync, writeFileSync } from 'node:fs'
import { registerAppServerPermissionHandlers as register } from '../../../../../cli/src/codex/utils/appServerPermissionAdapter'

if (process.env.CODEX_PERMISSION_VECTORS) {
    const cases: unknown[] = []
    function recording(args) {
        const destinations = args.client
        register({
            ...args,
            client: {
                registerRequestHandler(method, unused) {
                    destinations.registerRequestHandler(method, async (params) => {
                        const vector = {
                            method,
                            params,
                            callback: !!args.onUserInputRequest,
                            shared: !!args.shared
                        }
                        const handlers = new Map()
                        register({
                            ...args,
                            client: { registerRequestHandler: (k, v) => handlers.set(k, v) },
                            permissionHandler: {
                                async handleToolCall(id, tool, input, ...context) {
                                    const answer = await args.permissionHandler.handleToolCall(
                                        id,
                                        tool,
                                        input,
                                        ...context
                                    )
                                    Object.assign(vector, { tool, input, answer })
                                    return answer
                                }
                            },
                            onUserInputRequest: args.onUserInputRequest
                                ? async (request) => {
                                      const answer = await args.onUserInputRequest(request)
                                      Object.assign(vector, {
                                          tool: 'request_user_input',
                                          input: request.input,
                                          answer
                                      })
                                      return answer
                                  }
                                : undefined
                        })
                        const response = await handlers.get(method)(params)
                        cases.push({ ...vector, response })
                        return response
                    })
                }
            }
        })
    }
    const source = readFileSync('../../cli/src/codex/utils/appServerPermissionAdapter.test.ts', 'utf8')
    const body = new Bun.Transpiler({ loader: 'ts' }).transformSync(
        source.slice(source.indexOf('function createClient('))
    )
    new Function('describe', 'expect', 'it', 'vi', 'registerAppServerPermissionHandlers', body)(
        describe,
        expect,
        it,
        { fn: mock },
        recording
    )
    afterAll(() => writeFileSync(process.env.CODEX_PERMISSION_VECTORS!, JSON.stringify(cases)))
}
