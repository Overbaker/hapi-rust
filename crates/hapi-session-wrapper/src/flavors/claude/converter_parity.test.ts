import {beforeEach, describe, expect, it} from 'bun:test'
import {readFileSync, readSync, writeSync} from 'node:fs'

function call(action: string, ...args: unknown[]) {
    writeSync(1, JSON.stringify([action,...args]) + '\n')
    let line = ''
    const buffer = Buffer.alloc(4096)
    while (!line.endsWith('\n')) {
        const n = readSync(0,buffer)
        if (!n) throw Error('Rust converter closed')
        line += buffer.toString('utf8',0,n)
    }
    return JSON.parse(line).value
}

class Converter {
    private id: number
    constructor(context: unknown, responses = new Map()) {
        this.id = call('new',context,Object.fromEntries(responses))
    }
    convert(message: unknown) { return call('convert',this.id,message) }
    convertMany(messages: unknown[]) { return messages.map(m => this.convert(m)).filter(Boolean) }
    currentContextWindow() { return call('window',this.id) }
    applyContextUsage(model: string,tokens: number) { call('usage',this.id,model,tokens) }
    updateSelectedModel(model: string) { call('select',this.id,model) }
    resetParentChain() { call('reset',this.id) }
    updateSessionId(id: string) { call('session',this.id,id) }
    startFreshProcessBaseline() { call('fresh',this.id) }
    consumeResultForAccounting(message: unknown) { call('consume',this.id,message) }
    generateInterruptedToolResult(tool: string,parent?: string) {return call('interrupt',this.id,tool,parent)}
}
if (process.env.HAPI_RUST_CONVERTER_TEST === '1') {
    const suite = readFileSync('../../cli/src/claude/utils/sdkToLogConverter.test.ts','utf8')
    const body = new Bun.Transpiler({loader:'ts'}).transformSync(suite.slice(suite.indexOf("describe('SDKToLogConverter'")))
    const oneOff = (message,context,responses) => new Converter(context,responses).convert(message)
    new Function('describe','it','expect','beforeEach','SDKToLogConverter','convertSDKToLog',body)(
        describe,it,expect,beforeEach,Converter,oneOff)
}
