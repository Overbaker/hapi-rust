import assert from 'node:assert/strict'
import {createServer} from 'node:http'
import {createRequire} from 'node:module'
import {mkdtempSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync} from 'node:fs'
import {dirname,join,resolve} from 'node:path'
import {tmpdir} from 'node:os'

const binary = process.argv[2]
const scenario = process.argv[3]
const {Server} = createRequire(resolve('../../hub/package.json'))('socket.io')
const home = mkdtempSync(join(tmpdir(), 'hapi-session-e2e-'))
const row = {id:'s',namespace:'default',seq:0,createdAt:1,updatedAt:1,active:false,activeAt:0,thinking:false,thinkingAt:0,
    metadata:{},metadataVersion:1,agentState:{},agentStateVersion:1}
const paths: string[] = []
const emitted: Array<[string,any]> = []
let socket
let seq = 0
const http = createServer(async (req,res) => {
    const path = req.url!.split('?')[0]
    const chunks = []
    for await (const chunk of req) chunks.push(chunk)
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null
    paths.push(path)
    res.setHeader('content-type','application/json')
    if (path === '/session-started') {
        assert.equal(body.sessionId,'s')
        assert.equal(body.metadata.wrapperRuntime,'rust')
        return res.end('{}')
    }
    assert.equal(req.headers.authorization,'Bearer fixture')
    if (path === '/cli/machines') return res.end(JSON.stringify({machine:{...row,id:body.id,metadata:body.metadata,runnerState:null,runnerStateVersion:1}}))
    if (path.endsWith('/messages')) return res.end('{"messages":[]}')
    if (body) Object.assign(row,{metadata:body.metadata,agentState:body.agentState})
    res.end(JSON.stringify({session:row}))
})
const io = new Server(http,{transports:['websocket']})
io.of('/cli').on('connection', s => {
    socket = s
    assert.deepEqual(s.handshake.auth,{clientType:'session-scoped',token:'fixture',sessionId:'s'})
    s.onAny((event,payload) => emitted.push([event,payload]))
    s.on('ping', ack => ack())
    s.on('messages-steer-state',(_,ack) => ack({ok:true}))
    for (const [event,key] of [['update-metadata','metadata'],['update-state','agentState']]) {
        s.on(event,(payload,ack) => {
            const version = key+'Version'
            const matches = payload.expectedVersion === row[version]
            if (matches) {row[key]=payload[key];row[version]++}
            ack({result:matches?'success':'version-mismatch',version:row[version],[key]:row[key]})
        })
    }
})
await new Promise(resolve => http.listen(0,'127.0.0.1',resolve))
const port = http.address().port
writeFileSync(join(home,'settings.json'),JSON.stringify({machineId:'machine'}))
writeFileSync(join(home,'runner.state.json'),JSON.stringify({httpPort:port,pid:process.pid}))
if (scenario==='fallback') writeFileSync(join(home,'tmp'),'occupied')
const folder = join(home,'project')
mkdirSync(folder)
const projects = join(home,'claude','projects')
const transcript = join(projects,folder.replace(/[^a-zA-Z0-9]/g,'-'),'native-session.jsonl')
const log = join(home,'agent.jsonl')
writeFileSync(log,'')
const records = () => readFileSync(log,'utf8').trim().split('\n').filter(Boolean).map(JSON.parse.bind(JSON))
const prompts = () => records().filter(r=>r.input?.type==='user')
const spawns = () => records().filter(r=>r.argv && !r.argv.includes('--print') && !r.argv.includes('--version'))
const messages = () => emitted.filter(([e])=>e==='message').map(([,v])=>v.message.content)
const events = () => messages().filter(c=>c.type==='event').map(c=>c.data)
const ready = () => events().filter(e=>e.type==='ready').length
const notice = (msg:string) => events().some(e=>e.message===msg)
const launchArgs = ['claude','--started-by','runner','--hapi-starting-mode','remote']
if (scenario==='init-mode') launchArgs.push('--permission-mode','auto')
if (scenario==='resume') launchArgs.push('--resume','native-session')
if (scenario==='resume' || scenario==='rewind-reopen-no-turns') {
    const directory = dirname(transcript)
    mkdirSync(directory,{recursive:true})
    writeFileSync(transcript,'{"uuid":"seed"}\n')
}
const reopenWithoutTurns = scenario==='rewind-reopen-no-transcript' || scenario==='rewind-reopen-no-turns'
const dispatchWithoutTranscript = scenario==='rewind-dispatch-no-transcript'
if (scenario==='rewind-reopen' || reopenWithoutTurns || dispatchWithoutTranscript) {
    const resumeId = scenario==='rewind-reopen-no-transcript'?'deleted-session':'native-session'
    launchArgs.push('--existing-session-id','s','--resume',resumeId)
    row.metadata = {path:folder,host:'fixture',flavor:'claude',claudeSessionId:resumeId,
        conversationHistoryEntryIds:{first:'prompt-1'},
        claudeRewind:{targetLocalId:'second',truncateFromLocalId:'second',keepBoundary:'answer-1',
            droppedPrompts:['prompt-2'],discardedThroughSeq:2}}
    if (scenario==='rewind-reopen') {
        row.metadata.claudeRewind.hubCutApplied = true
    }
}
if (scenario==='rewind-reopen' || dispatchWithoutTranscript) {
    mkdirSync(dirname(transcript),{recursive:true})
    writeFileSync(transcript,[
        {type:'user',uuid:'prompt-1',message:{content:'keep this turn'}},
        {type:'assistant',uuid:'answer-1',parentUuid:'prompt-1'},
        {type:'user',uuid:'prompt-2',parentUuid:'answer-1',message:{content:'discard this turn'}},
        {type:'assistant',uuid:'answer-2',parentUuid:'prompt-2'},
    ].map(row=>JSON.stringify(row)+'\n').join(''))
}
if (scenario==='fork') launchArgs.push('--resume','native-parent','--fork-session','--resume-session-at','kept-answer')
const child = Bun.spawn([binary,...launchArgs],{cwd:folder,stderr:'pipe',stdout:'pipe',env:{
    ...process.env,HAPI_API_URL:'http://127.0.0.1:'+port,CLI_API_TOKEN:'fixture',HAPI_HOME:home,
    HAPI_INVOKED_CWD:folder,HAPI_WRAPPER_CLI_VERSION:'1.test',HAPI_WRAPPER_RUNTIME_PATH:home,
    HAPI_CLAUDE_PATH:join(import.meta.dir,'fake_claude.py'),CLAUDE_CONFIG_DIR:join(home,'claude'),FAKE_CLAUDE_LOG:log,
    FAKE_SCENARIO:scenario,FAKE_REJECT_PLAN:scenario==='mode'?'1':'',CLAUDE_REMOTE_RESPAWN_BACKOFF_MS:'5',
}})
async function wait(label:string,predicate:()=>boolean) {
    const end=Date.now()+9000
    while(!predicate()) {
        if (child.exitCode!==null || Date.now()>end) throw Error(label+' failed: '+JSON.stringify({exit:child.exitCode,events:events(),records:records()}))
        await Bun.sleep(10)
    }
}
const send = (text:string,localId:string,attachments?:any[]) => {
    socket.emit('update',{body:{t:'new-message',message:{id:localId,localId,seq:++seq,createdAt:Date.now(),content:{role:'user',content:{type:'text',text,...(attachments?{attachments}:{})},meta:{sentFrom:'web'}}}}})
}
const rpc = async (method:string,params={}) => {
    const result=await socket.timeout(5000).emitWithAck('rpc-request',{method:'s:'+method,params:JSON.stringify(params)})
    return result?JSON.parse(result):undefined
}
const markHubCut = () => {
    delete row.metadata.conversationHistoryEntryIds.second
    if (row.metadata.conversationHistoryPoints) delete row.metadata.conversationHistoryPoints.second
    row.metadata.claudeRewind.hubCutApplied = true
    socket.emit('update',{body:{t:'update-session',sid:'s',metadata:{version:++row.metadataVersion,value:row.metadata}}})
}
const nativeRows = () => readFileSync(transcript,'utf8').trim().split('\n').map(line=>JSON.parse(line))
try {
    if(scenario==='fallback') {
        assert.equal(await child.exited,86)
        assert.match(await new Response(child.stderr).text(),/^HAPI_RUST_WRAPPER_SESSION_ID=s$/m)
        assert.ok(!paths.includes('/session-started'))
    }else{
        await wait('webhook and connection',()=>socket && paths.includes('/session-started'))
        if(scenario==='plain'){
            send('hello runner','first')
            await wait('ready plus title',()=>ready()===1 && row.metadata.summary?.text==='Native title')
            assert.equal(prompts().length,1)
            assert.equal(prompts()[0].input.message.content,'hello runner')
            assert.deepEqual(row.metadata.conversationHistoryPoints,{first:true})
            assert.deepEqual(row.metadata.conversationHistoryEntryIds,{first:prompts()[0].input.uuid})
            assert.equal(records().find(r=>r.argv?.includes('--print')).sid,'s')
            assert.deepEqual(spawns()[0].argv.slice(0,3),['--output-format','stream-json','--verbose'])
            assert.ok(messages().some(c=>c.data?.message?.content?.[0]?.text==='Answer: hello runner'))
            assert.ok(!messages().some(c=>c.type==='text'))
            const carrier=messages().findIndex(c=>c.data?.subtype==='turn_duration')
            assert.ok(carrier>=0 && carrier<messages().findIndex(c=>c.data?.type==='ready'))
            assert.deepEqual(row.metadata.slashCommands,['status'])
            assert.deepEqual(await rpc('listSkills'),{success:true,skills:[{name:'review'}]})
            assert.ok(!emitted.some(([event, v]) => event === 'session-alive' && v.nativeResumeReady))
            assert.deepEqual(await rpc('fork-conversation'),{nativeSessionId:'native-session',forkSession:true})
            assert.equal(row.agentState.startingMode,'remote')
            assert.ok(emitted.some(([e,v])=>e==='messages-consumed' && v.localIds[0]==='first'))
        }else if(scenario==='tool'){
            const decision = id => records().find(r=>r.input?.response?.request_id===id)?.input.response.response
            const request = id => row.agentState.requests?.[id]
            const set = permissionMode => rpc('set-session-config',{permissionMode})
            const run = async (text,count) => {
                send(text,text)
                await wait(text,()=>ready()===count && decision(text)?.behavior==='allow')
            }
            await set('bypassPermissions')
            send('tool-plan','plan')
            await wait('plan continuation',()=>ready()===1 && prompts().filter(r=>r.input.message.content==='PlEaZe Continue with plan.').length===2)
            assert.equal(decision('tool-plan').behavior,'deny')
            await run('tool-task',2)
            const rows = messages().map(c=>c.data).filter(Boolean)
            const root = rows.find(r=>r.isSidechain && r.message?.content==='find the bug')
            assert.ok(root?.uuid)
            assert.equal(root.parentUuid,null)
            assert.equal(rows.find(r=>r.message?.content?.[0]?.text==='task reply').parentUuid,root.uuid)
            await set('acceptEdits')
            await run('tool-edit',3)
            await set('default')
            send('tool-grant','grant')
            await wait('permission registered',()=>request('tool-grant'))
            await rpc('permission',{id:'tool-grant',approved:true,mode:'default',allowTools:['Bash(git :*)']})
            await wait('grant',()=>ready()===4)
            const grant = messages().map(c=>c.data?.message?.content?.[0]).find(b=>b?.tool_use_id==='tool-grant')
            assert.equal(grant.permissions.mode,'default')
            assert.deepEqual(grant.permissions.allowedTools,['Bash(git :*)'])
            await run('tool-reuse',5)
            await rpc('abort')
            send('tool-cancel','cancel')
            await wait('cancel registered',()=>request('tool-cancel'))
            writeFileSync(join(folder,'cancel'),'')
            await wait('cancelled',()=>row.agentState.completedRequests?.['tool-cancel']?.status==='canceled')
            await rpc('abort')
            await wait('interrupted',()=>messages().some(c=>c.data?.message?.content?.some?.(b=>b.tool_use_id==='tool-cancel' && b.is_error)))
            send('tool-steer','before')
            await wait('steer registered',()=>request('tool-steer'))
            send('steered','after')
            assert.deepEqual(await rpc('steer-queued-message',{localId:'after'}),{steered:true})
            await wait('steer denied',()=>decision('tool-steer')?.behavior==='deny' && ready()>=6)
            assert.equal(prompts().at(-1).pid,prompts().at(-2).pid)
        }else if(scenario==='skill-drop'){
            send('hold','first')
            await wait('active',()=>row.agentState.steeringActive && prompts().length===1)
            send('$review issue','')
            writeFileSync(join(folder,'catalog-ready'),'')
            assert.equal((await rpc('listSkills')).success,true)
            await rpc('set-session-config',{permissionMode:'plan'})
            await wait('drop',()=>events().some(e=>e.message?.includes('Dropping the queued message')))
            assert.ok(prompts().some(r=>r.input.message.content==='/review issue'))
            send('after drop','last')
            await wait('fresh transcript',()=>ready()===1)
            const transcript = emitted.find(([e,v])=>e==='message' && v.message.content?.text==='/review issue')
            assert.ok(transcript)
            assert.ok(!transcript[1].message.meta?.isTranscriptEcho)
        }else if(scenario==='mode-order'){
            send('hold','first')
            await wait('active',()=>prompts().length===1 && row.agentState.steeringActive)
            send('queued','second')
            await rpc('set-session-config',{permissionMode:'plan'})
            await wait('queued turn',()=>ready()===1)
            assert.deepEqual(records().map(r=>r.input?.request?.mode ?? r.input?.message?.content).filter(Boolean),
                ['hold','plan','default','queued'])
        }else if(scenario==='mode'){
            send('hold','first')
            await wait('steerable',()=>row.agentState.steeringActive)
            const original = await rpc('set-session-config',{})
            for (const [payload,error] of [
                [{permissionMode:'plan',model:123},'Invalid model'],
                [{model:'opus',effort:false},'Invalid effort'],
                [{permissionMode:'unknown'},'Invalid permission mode'],
            ]) {
                assert.deepEqual(await rpc('set-session-config',payload),{error})
                assert.deepEqual(await rpc('set-session-config',{}),original)
            }
            assert.ok(!records().some(r=>r.input?.request?.subtype==='set_permission_mode'))
            await rpc('set-session-config',{permissionMode:'plan'})
            await wait('native mode rejection',()=>notice('plan disabled'))
            send('steered','next')
            await wait('queued row',()=>prompts().length===1)
            assert.deepEqual(await rpc('steer-queued-message',{localId:'next'}),{steered:true})
            await wait('steered echo',()=>emitted.some(([e,v])=>e==='messages-consumed' && v.steered && v.localIds[0]==='next') && ready()===1)
            // TS order: the steered ACK precedes the output of the turn that picked it up.
            const consumed=emitted.findIndex(([e,v])=>e==='messages-consumed' && v.steered && v.localIds[0]==='next')
            const answer=emitted.findIndex(([e,v])=>e==='message' && v.message.content.data?.message?.content?.[0]?.text==='Answer: steered')
            const duration=emitted.findLastIndex(([e,v])=>e==='message' && v.message.content.data?.subtype==='turn_duration')
            assert.ok(answer>consumed && duration>consumed,`consumed=${consumed} answer=${answer} turn_duration=${duration}`)
            assert.equal(spawns().length,1)
            await rpc('set-session-config',{effort:'HIGH'})
            send('follow-up','third')
            await wait('effort',()=>ready()===2)
            assert.ok(records().some(v=>v.input?.request?.settings?.effortLevel==='high'))
            assert.equal(spawns().length,1)
            await rpc('set-session-config',{model:'different'})
            send('after restart','fourth')
            await wait('mode restart',()=>ready()===3)
            assert.ok(spawns()[1].argv.includes('different'))
        }else if(scenario==='resume'){
            assert.ok(!emitted.some(([event, v]) => event === 'session-alive' && v.nativeResumeReady))
            assert.deepEqual(await rpc('fork-conversation'),{nativeSessionId:'native-session',forkSession:true})
            send('/compact','first')
            await wait('confirmed native resume', () => emitted.some(([event, v]) =>
                event === 'session-alive' && v.nativeResumeReady?.nativeId === 'native-session'
                && v.nativeResumeReady.pid === child.pid && v.nativeResumeReady.startMarker === row.metadata.hostStartMarker))
            await wait('resumed summary',()=>events().some(e=>e.type==='compact-summary' && e.summary==='new summary'))
        }else if(scenario==='compact'){
            send('hello','a');await wait('first result',()=>ready()===1)
            send('/compact','b');await wait('summary',()=>ready()===2)
            assert.deepEqual(events().find(e=>e.type==='compact-summary'),{type:'compact-summary',summary:'new summary',tokensBefore:300,tokensAfter:100})
            assert.ok(notice('📦 Compaction started'))
            assert.ok(!messages().some(c=>c.data?.subtype==='compact_boundary'||c.text?.includes('Compacted')))
            send('/clear','c');await wait('clear',()=>notice('Context was reset'))
            send('$review trees','d',[{id:'f',filename:'f',mimeType:'text/plain',size:1,path:'/tmp/a'}])
            await wait('skill',()=>ready()===3)
            assert.equal(prompts().at(-1).input.message.content,'/review trees\n\n@/tmp/a')
            assert.ok(!spawns().at(-1).argv.includes('--resume'))
        }else if(scenario==='steer-cleared'){
            send('hold','first')
            await wait('active',()=>prompts().length===1 && row.agentState.steeringActive)
            send('unpicked','next')
            assert.deepEqual(await rpc('steer-queued-message',{localId:'next'}),{steered:true})
            send('/clear','clear')
            await rpc('abort')
            await wait('cleared',()=>notice('Context was reset'))
            assert.ok(!emitted.some(([e,v])=>e==='messages-indeterminate' && v.localIds.includes('next')))
        }else if(scenario==='steer-abort'||scenario==='steer-close'){
            send(scenario==='steer-abort'?'hold-tree':'hold','first')
            await wait('tool child',()=>prompts().length===1 && (scenario!=='steer-abort'||records().some(r=>r.descendant)) && row.agentState.steeringActive)
            const descendant=records().find(r=>r.descendant)?.descendant
            send('unpicked','next')
            assert.deepEqual(await rpc('steer-queued-message',{localId:'next'}),{steered:true})
            if(scenario==='steer-abort'){
                assert.equal(await rpc('abort'),undefined)
                try {
                    const stat=readFileSync('/proc/'+descendant+'/stat','utf8')
                    assert.ok(['Z','X'].includes(stat[stat.lastIndexOf(') ')+2]),'abort acknowledged while a tool process is still running')
                } catch (error) {
                    if (!['ENOENT','ESRCH'].includes(error.code)) throw error
                }
                await wait('indeterminate',()=>emitted.some(([e,v])=>e==='messages-indeterminate' && v.localIds[0]==='next'))
                send('fresh','another')
                await wait('new turn',()=>ready()===1)
            }else{
                await wait('restored steer',()=>emitted.some(([e,v])=>e==='messages-steer-state' && v.state==='queued') && ready()===1)
                assert.deepEqual(prompts().map(p=>p.input.message.content),['hold','unpicked','unpicked'])
            }
        }else if(scenario.startsWith('effort-')){
            send('first','one')
            await wait('first',()=>ready()===1)
            await rpc('set-session-config',{effort:'medium'})
            send('second','two')
            await wait('applied to replacement',()=>ready()===2)
            assert.equal(prompts().length,2)
            assert.equal(spawns().length,2)
            assert.ok(spawns()[1].argv.includes('medium'))
        }else if(scenario==='plan'){
            send('/plan','one')
            await wait('mode event',()=>events().some(e=>e.type==='permission-mode-changed' && e.mode==='plan'))
            assert.equal(prompts().length,0)
            send('/plan "inspect it"','two',[{id:'f',filename:'f',mimeType:'text/plain',size:1,path:'/tmp/notes'}])
            await wait('plan turn',()=>ready()===1)
            assert.equal(prompts()[0].input.message.content,'@/tmp/notes\n\ninspect it')
            send('/plan off','three')
            await wait('off',()=>events().some(e=>e.type==='permission-mode-changed' && e.mode==='default'))
            send('/compact fail unavailable','four')
            await wait('compact failure detail',()=>notice('📦 Compaction failed: unavailable'))
            send('/compact fail','five')
            await wait('compact failure',()=>notice('📦 Compaction failed'))
        }else if(scenario==='init-mode'){
            send('turn','a')
            await wait('default fallback',()=>ready()===1 && notice('Claude could not start in auto mode, so it is running in default mode'))
            assert.deepEqual((await rpc('set-session-config',{})).applied.permissionMode,'default')
        }else if(scenario==='stale-mode'){
            send('hold','a')
            await wait('hold',()=>prompts().length===1 && row.agentState.steeringActive)
            for(const permissionMode of ['auto','default','auto']) await rpc('set-session-config',{permissionMode})
            await wait('all switches',()=>records().filter(r=>r.input?.request?.subtype==='set_permission_mode').length===3)
            send('next','b')
            // A round trip confirms earlier switch replies have reached the launcher.
            await rpc('abort')
            await wait('next turn',()=>ready()===1)
            assert.ok(!notice('stale switch'))
            assert.equal((await rpc('set-session-config',{})).applied.permissionMode,'auto')
        }else if(scenario==='compact-wait'){
            send('first','a')
            await wait('initial',()=>ready()===1)
            send('/compact wait','b')
            await wait('autonomous output during lookup',()=>messages().some(c=>c.data?.message?.content?.[0]?.text==='autonomous'))
            assert.equal(ready(),1)
            await rpc('abort')
            assert.ok(!events().some(e=>e.type==='compact-summary'))
            send('after compact abort','c')
            await wait('fresh turn',()=>ready()===2)
        }else if(scenario==='failure'){
            send('crash','a')
            await wait('failure cap',()=>events().some(e=>e.message?.includes('3 times in a row')))
            assert.equal(prompts().length,3)
            send('hold','b');await wait('in flight',()=>row.agentState.steeringActive && prompts().length===4)
            await rpc('abort')
            assert.ok(notice('Aborted by user'))
            send('after abort','c');await wait('after abort',()=>ready()===1)
        }else if(scenario==='history'){
            send('keep this turn','first')
            await wait('first turn',()=>ready()===1)
            send('exclude this turn','second')
            await wait('second turn',()=>ready()===2 && row.metadata.conversationHistoryEntryIds?.second)
            const relocated = join(projects,'worktree')
            mkdirSync(relocated)
            renameSync(transcript,join(relocated,'native-session.jsonl'))
            assert.deepEqual(await rpc('fork-conversation',{messageLocalId:'second'}),{
                nativeSessionId:'native-session',forkSession:true,resumeSessionAt:'answer-1',
            })
            assert.deepEqual(await rpc('fork-conversation'),{
                nativeSessionId:'native-session',forkSession:true,resumeSessionAt:'answer-2',
            })
            assert.match((await rpc('fork-conversation',{messageLocalId:'first'})).error,/before the first message/)
        }else if(scenario==='rewind'){
            send('keep this turn','first')
            await wait('first turn',()=>ready()===1)
            send('discard this turn','second')
            await wait('second turn',()=>ready()===2 && row.metadata.conversationHistoryEntryIds?.second)
            assert.deepEqual(await rpc('rewind-conversation',{messageLocalId:'first'}),{
                success:false,outcome:'rejected',error:'Cannot rewind the first message',
            })
            const discarded = row.metadata.conversationHistoryEntryIds.second
            assert.deepEqual(await rpc('rewind-conversation',{messageLocalId:'second',discardedThroughSeq:2}),{
                success:true,truncateFromLocalId:'second',
            })
            assert.equal(row.metadata.capabilities.conversationHistory.rewindToMessage,true)
            assert.deepEqual(row.metadata.claudeRewind,{
                targetLocalId:'second',truncateFromLocalId:'second',keepBoundary:'answer-1',
                droppedPrompts:[discarded],discardedThroughSeq:2,
            })
            await wait('rewind process before input',()=>spawns().length===2)
            const {argv} = spawns()[1]
            assert.equal(argv[argv.indexOf('--resume')+1],'native-session')
            assert.equal(argv[argv.indexOf('--resume-session-at')+1],'answer-1')
            assert.match((await rpc('fork-conversation')).error,/rewind is already pending/)
            send('continue from the first turn','third')
            await Bun.sleep(100)
            assert.ok(!emitted.some(([event,v])=>event==='messages-consumed' && v.localIds.includes('third')))
            writeFileSync(join(folder,'release-resume'),'')
            await wait('new turn',()=>ready()===3)
            assert.equal(nativeRows().find(r=>r.uuid===prompts()[2].input.uuid).parentUuid,'answer-1')
            assert.ok(row.metadata.claudeRewind)
            markHubCut()
            await wait('rewind settled',()=>!row.metadata.claudeRewind)
            assert.equal(row.metadata.conversationHistoryEntryIds.third,prompts()[2].input.uuid)
            assert.equal(row.metadata.conversationHistoryEntryIds.second,undefined)
            await rpc('set-session-config',{model:'another-model'})
            send('keep the new turn too','fourth')
            await wait('ready after model restart',()=>ready()===4)
            assert.ok(!spawns()[2].argv.includes('--resume-session-at'))
            assert.equal(nativeRows().find(r=>r.uuid===prompts()[3].input.uuid).parentUuid,'answer-3')
        }else if(reopenWithoutTurns || dispatchWithoutTranscript){
            if (dispatchWithoutTranscript) {
                await wait('eager rearm',()=>spawns().length===1)
                assert.ok(spawns()[0].argv.includes('--resume-session-at'))
                assert.equal(await rpc('abort'),undefined)
                rmSync(transcript)
            }
            const failure = () => events().find(e=>e.message?.startsWith('Process exited unexpectedly'))?.message
            send('continue without a transcript','third')
            await wait('follow-up turn',()=>ready()===1 || failure())
            assert.equal(failure(),undefined)
            assert.equal(spawns().length,dispatchWithoutTranscript?2:1)
            const {argv} = spawns().at(-1)
            assert.ok(!argv.includes('--resume'))
            assert.ok(!argv.includes('--resume-session-at'))
            assert.equal(prompts()[0].input.message.content,'continue without a transcript')
            await wait('reopen metadata committed',()=>row.metadata.wrapperRuntime==='rust')
            assert.ok(row.metadata.claudeRewind)
            markHubCut()
            await wait('rewind settled after hub cut',()=>!row.metadata.claudeRewind)
            assert.equal(row.metadata.claudeSessionId,'native-session')
        }else if(scenario==='rewind-reopen'){
            await wait('eager rearm',()=>spawns().length===1 && row.metadata.claudeRewind?.hubCutApplied!==true)
            const {argv} = spawns()[0]
            assert.equal(argv[argv.indexOf('--resume-session-at')+1],'answer-1')
            markHubCut()
            assert.match((await rpc('fork-conversation')).error,/rewind is already pending/)
            send('continue in reopened session','third')
            await wait('proof and hub cut',()=>ready()===1 && !row.metadata.claudeRewind)
            assert.equal(nativeRows().find(r=>r.uuid===prompts()[0].input.uuid).parentUuid,'answer-1')
        }else if(scenario==='fork'){
            await wait('eager fork',()=>spawns().length===2)
            assert.equal(prompts().length,0)
            for (const {argv} of spawns()) {
                assert.equal(argv[argv.indexOf('--resume')+1],'native-parent')
                assert.equal(argv[argv.indexOf('--resume-session-at')+1],'kept-answer')
            }
            send('child','a')
            await wait('child turn',()=>ready()===1)
            assert.ok(!emitted.some(([event, v]) => event === 'session-alive' && v.nativeResumeReady))
            await wait('child native id',()=>row.metadata.claudeSessionId==='native-child')
            await rpc('set-session-config',{model:'other'})
            send('continue child','b')
            await wait('relaunch',()=>ready()===2)
            assert.ok(spawns()[2].argv.includes('native-child'))
            assert.ok(!spawns()[2].argv.includes('--fork-session'))
            assert.ok(!spawns()[2].argv.includes('--resume-session-at'))
        }
        child.kill('SIGTERM')
        assert.equal(await child.exited,0)
        assert.ok(emitted.some(([e])=>e==='session-end'))
    }
}finally{
    if(child.exitCode===null) child.kill('SIGKILL')
    await child.exited
    await io.close()
    rmSync(home,{recursive:true,force:true})
}
