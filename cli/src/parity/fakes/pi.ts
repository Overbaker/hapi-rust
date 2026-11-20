#!/usr/bin/env bun
import { readFileSync } from 'node:fs'
import { createInterface } from 'node:readline'
if (process.argv.includes('--version')) { console.log('0.99.0'); process.exit(0) }
const args = process.argv.slice(2)
if (args[args.indexOf('--mode') + 1] !== 'rpc' || !readFileSync(args[args.indexOf('--extension') + 1]!, 'utf8').includes('hapi_change_title')) process.exit(2)
const { claimLaunch } = await import('../launchLog')
const launch = claimLaunch(process.env.PARITY_OUT!, 'agent')
const prompts: Record<string, unknown>[] = []
const log = () => launch.save({argv:args, prompts})
log()
const script = JSON.parse(readFileSync(process.env.PARITY_AGENT_SCRIPT!, 'utf8'))
const write = (v: unknown) => process.stdout.write(JSON.stringify(v) + '\n')
let nativeId = 'pi-test-session'
const models = script.models ?? []
for await (const line of createInterface({ input: process.stdin })) {
    const cmd = JSON.parse(line)
    if (cmd.type === 'prompt') {
        const {id: _id, ...prompt} = cmd
        prompts.push(prompt)
        log()
        if (cmd.message !== script.message) process.exit(3)
        for (const event of script.events) write(event)
    }
    if (cmd.type === 'clone') nativeId = 'pi-clone'
    if (cmd.type === 'switch_session' && cmd.sessionPath === '/pi/pi-test-session.jsonl') nativeId = 'pi-test-session'
    const data: Record<string, unknown> = { get_state: {sessionId:nativeId, sessionFile:'/pi/' + nativeId + '.jsonl', isStreaming:false}, get_entries: {entries:[],leafId:null}, get_fork_messages:{messages:[]}, get_commands: {commands:[]}, get_available_models: {models}, clone:{}, switch_session:{}, set_model:{id:cmd.modelId,provider:cmd.provider}, set_thinking_level:{} }
    write({type:'response', command:cmd.type, id:cmd.id, success:cmd.type === 'prompt' || cmd.type in data, data:data[cmd.type], error:cmd.type in data || cmd.type === 'prompt' ? undefined : 'Unknown command'})
}
