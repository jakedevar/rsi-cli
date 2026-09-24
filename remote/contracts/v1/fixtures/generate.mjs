// Reviewable fixture authoring, independent of both codecs. Expected canonical
// values are authored here, never captured from either runtime's serialization.
import { readFileSync, writeFileSync } from 'node:fs';
const id = n => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const epoch = id(900), project = id(1), session = id(10), time = '2026-09-08T12:34:56.123456789Z';
const methods = ['RemoteGetInfoV1','RemoteListProjectsV1','RemoteListSessionsV1','RemoteGetSessionV1','RemoteGetHistoryPageV1','RemoteGetDecisionsV1'];
const pending = ['question_publications','tracked_question_slot','session_question_slot','durable_question_fallback','native_runtime','native_publications','native_historical_fallback','legacy_approvals'];
const sources = [[], ['configured_projects','store_projects'], ['active_sessions','completed_sessions','store_sessions'], ['active_sessions','completed_sessions','store_sessions'], ['store_history'], pending];
const copy = v => structuredClone(v);
const known = value => ({state:'known',value});
const cov = ss => ss.map((source,i) => ({source,state:'complete',has_more:false,lower_bound:'0',observed_at:time,observation_order:i+1}));
const limits = i => ({page_items:[1,25,50,1,25,16][i],name_bytes:i===3?4096:512,event_text_bytes:8192,decision_text_bytes:8192,item_bytes:65536,envelope_bytes:524288});
const obs = i => ({version:'1.0',daemon_epoch:epoch,observed_at:time,next_cursor:null,complete:true,projection_limits:limits(i),degraded:[],coverage:cov(sources[i])});
const doc = (type,value) => ({type,value});
const request = (i, params={}) => doc('request',{method:methods[i],params});
const response = (i, fields) => doc('response',{method:methods[i],result:{...obs(i),...fields}});
const field = (source_field,text,source_extent='full_field',state='complete',observed_bytes=String(Buffer.byteLength(text))) => ({text,state,source_field,source_extent,observed_bytes});
const summary = (n=10,provider='Claude') => ({id:id(n),project_id:project,parent_id:id(20),continued_from:id(21),kind:known('Standard'),own_title:`Own session ${n}`,provider:known(provider),status:known('Running'),updated_at:time,attention:{requires_local_action:false,incomplete:false,live_signals_lower_bound:'0'}});
const key = (id='9007199254740993',sequence=1) => ({sequence,id});
const event = (n='9007199254740993',sequence=1,text='Unit and Browser') => ({id:n,sequence,kind:known('Message'),role:known('User'),created_at:time,text,content_bytes:String(Buffer.byteLength(text)),truncated:false,tool_name:null,tool_pair_key:null,tool_id_display:null,pairing_state:'missing',content_state:'complete'});
const history = (events=[event()],window={kind:'latest'},position={state:'unchanged'}) => response(4,{project_id:project,session_id:session,items:events,window,head:events.length?key(events.at(-1).id,events.at(-1).sequence):null,interval:{lower_exclusive:null,upper_inclusive:events.length?key(events.at(-1).id,events.at(-1).sequence):null},position});
const sourceObs = source => ({source,observed_at:time,state:'complete',incarnation:null,spawn_generation:null,witness_present:null,resolution_observed:null,resolution_persisted:null,writer_live:null,writer_capacity:null,display_alternative:null});
const nativeDisplay = (method='commandExecution/approval',description='Run tests',extent='bounded_snapshot') => ({kind:'native_approval',method:field('method',method,extent),description:field('description',description,extent)});
const native = (n=40,display=nativeDisplay(),source='native_runtime') => ({id:`native:${id(n)}`,identity_class:'publication',kind:'native_approval',publication_state:known('published'),closure_state:'open',delivery_state:'unknown',source_observations:[sourceObs(source)],omitted_source_observations:'0',disagreement:false,display,questions:[],omitted_questions:'0',details_state:'complete',requires_local_action:true,can_answer:false});
const question = () => ({...native(),id:`question:${id(50)}`,kind:'generic_questions',source_observations:[sourceObs('question_publications')],display:{kind:'generic_questions'},questions:[{header:'Checks',question:'Which checks?',options:[{label:'Unit',description:'Fast tests'},{label:'Browser',description:'Browser interactions'}],multi_select:true,omitted_options:'0'},{header:'Branch',question:'Which branch?',options:[{label:'Current branch',description:'Keep sandbox custody'}],multi_select:false,omitted_options:'0'}]});
const decisions = (items=[native()],selected={state:'none'}) => response(5,{project_id:project,session_id:session,mode:'attention',items,selected});
const binding = (selection_generation='1',attachment_generation='1') => ({gateway_epoch:id(901),policy_epoch:id(902),view_id:id(903),view_epoch:id(904),selection_generation,attachment_generation,cache_epoch:id(905)});
const state = (selection={kind:'none'},gen='1') => ({binding:binding(gen),selection,lease:{state:'attached',expires_at:time,duration_ms:25000},barrier:{sequence:'9007199254740993',pages:[]},ready:true});
const page = (slot='projects') => ({slot,page_key:`Remote:${slot}`,entry_incarnation:'9007199254740993',page_revision:'9007199254740994'});
const cases=[];
const add = (name,input,visible=[],extra={}) => {cases.push({id:name,valid:true,input,visible,...extra});return input;};
const bad = (name,base,patches=[],extra={}) => cases.push({id:name,valid:false,base,patches,...extra});
const set = (path,value) => ({op:'set',path:path.split('/'),value});
const remove = path => ({op:'remove',path:path.split('/')});

add('request-info',request(0));
add('request-projects',request(1,{project_ids:[project],limit:25,cursor:null}));
add('request-projects-defaults',request(1,{project_ids:[project]}),[],{expected:request(1,{project_ids:[project],limit:25,cursor:null})});
add('client-discovery-without-project-ids',doc('view_request',{method:methods[1],params:{}}),[],{expected:doc('view_request',{method:methods[1],params:{limit:25,cursor:null}})});
add('request-sessions',request(2,{project_id:project,limit:50,cursor:null}));
add('request-detail',request(3,{project_id:project,session_id:session}));
add('request-history',request(4,{project_id:project,session_id:session,window:{kind:'latest'},limit:25,cursor:null}));
add('request-decisions',request(5,{project_id:project,session_id:session,limit:16,mode:'attention',cursor:null,selected_decision_id:null}));
for (const [name,window] of Object.entries({older:{kind:'older',anchor:key()},newer:{kind:'newer',anchor:key(),through:key('9007199254740994')},interval:{kind:'interval',lower_exclusive:key(),upper_inclusive:key('9007199254740994')},locate:{kind:'locate',event_id:'9007199254740993',old_key:key(),offset:8}})) {
  add(`request-history-${name}`,request(4,{project_id:project,session_id:session,window,limit:50,cursor:null}));
}
add('request-newer-default-through',request(4,{project_id:project,session_id:session,window:{kind:'newer',anchor:key()}}),[],{expected:request(4,{project_id:project,session_id:session,window:{kind:'newer',anchor:key(),through:null},limit:25,cursor:null})});
add('info-capabilities',response(0,{item:{protocol:'1.0',daemon_boot_id:epoch,required_capabilities:methods}}),[],{against:'request-info'});
const zero=response(1,{items:[]}); zero.value.result.coverage=cov(['configured_projects']);
add('projects-zero-configured',zero);
add('projects-zero-source',response(1,{items:[]}));
add('projects-one',response(1,{items:[{id:project,name:'Named project α'}]}),['Named project α'],{against:'request-projects'});
add('projects-many',response(1,{items:[{id:project,name:'Named project α'},{id:id(2),name:'Another own name'},{id:id(3),name:'Empty research project'}]}),['Named project α','Another own name','Empty research project']);
add('selected-empty-project',response(2,{project:{id:project,name:'Selected empty project'},items:[]}),['Selected empty project'],{against:'request-sessions'});
const filtered=response(2,{project:{id:project,name:'Filtered project'},items:[]}); filtered.value.result.next_cursor={kind:'sessions',token:'examined-candidate-cursor'}; filtered.value.result.coverage[2].has_more=true;
add('filtered-empty-continuation',filtered,['Filtered project']);
const providers=['Claude','Codex','Pioneer','Local','Antigravity','CodexAppServer','Harness'];
const manySessions=providers.map((p,i)=>summary(10+i,p));manySessions[0].kind=known('Group');manySessions[0].own_title='Own group identity';
add('session-own-parent-continuation-providers',response(2,{project:{id:project,name:'Identity project'},items:manySessions}),['Own group identity',id(20),id(21),...providers]);
const future=summary();future.provider={state:'unknown',label:'ProviderZ'};future.status={state:'unknown',label:'PausedByFuture'};future.kind={state:'unknown',label:'FutureLeaf'};
add('session-unknown-labels',response(2,{project:{id:project,name:'Future project'},items:[future]}),['Unknown: ProviderZ','Unknown: PausedByFuture','Unknown: FutureLeaf']);
add('detail-runtime-only-empty-history',response(3,{item:{summary:summary(),own_title:'Own session 10',query:field('query','Investigate contracts'),model:field('model','Example model'),sequences:[{source:'active_sessions',sequence:2147483647,event_id:null},{source:'store_history',sequence:null,event_id:null}],pending_coverage:cov(pending)}}),['Own session 10','Investigate contracts'],{against:'request-detail'});
add('history-successful-empty',history([]));
const events=[event(),event('9007199254740994',1,'Saved assistant output'),event('9223372036854775807',2147483647,'Unknown event stays visible')];events[1].role=known('Assistant');events[2].kind={state:'unknown',label:'TerminalError'};events[2].role={state:'unknown',label:'SystemRole'};
add('history-lossless-order-saved-answers',history(events),['Unit and Browser','Saved assistant output','Unknown: TerminalError','Unknown: SystemRole','9223372036854775807']);
const tools=[event('1',1,'Call'),event('2',2,'Result'),event('3',3,'Ambiguous reused call'),event('4',4,'Unpaired oversized result')];
for (let i=0;i<tools.length;i++) { tools[i].role=null;tools[i].kind=known(i%2?'ToolResult':'ToolUse');tools[i].tool_name='Read'; }
tools[0].tool_pair_key=tools[0].tool_id_display='exact-call-id';tools[0].pairing_state='exact';tools[1].tool_pair_key=tools[1].tool_id_display='exact-call-id';tools[1].pairing_state='exact';tools[2].tool_pair_key=tools[2].tool_id_display='reused-id';tools[2].pairing_state='ambiguous';tools[3].tool_id_display='共'.repeat(85);tools[3].pairing_state='oversized';
add('history-tool-pair-display-separation',history(tools),['Call','Result','Ambiguous reused call','Unpaired oversized result','exact-call-id','共'.repeat(85)]);
const oversized=[event('5',5,'First oversized result'),event('6',6,'Second oversized result')];
for(const e of oversized) {e.kind=known('ToolResult');e.tool_name='Read';e.tool_id_display='a'.repeat(256);e.pairing_state='oversized';}
add('distinct-oversized-tool-ids-shared-prefix',history(oversized),['First oversized result','Second oversized result'],{source_tool_ids:['a'.repeat(256)+'x','a'.repeat(256)+'y']});
add('exact-key-unmatched-result',history([tools[1]]),['Result','exact-call-id']);
const truncated=event('2',-2147483648,'界'.repeat(2730)+'ab');truncated.content_state='preview';truncated.truncated=true;truncated.content_bytes='18446744073709551615';
add('history-utf8-limit-truncation',history([truncated]),['18446744073709551615']);
const offloaded=event('1',1,'Saved offload preview');offloaded.content_state='offloaded';offloaded.content_bytes='100000';
add('history-offloaded-preview',history([offloaded]),['Saved offload preview']);
const relocated=event('1',2,'New text');
const locateWindow={kind:'locate',event_id:'1',old_key:key('1',1),offset:8};
add('request-locate-anchor-one',request(4,{project_id:project,session_id:session,window:locateWindow,limit:25,cursor:null}));
add('history-relocation',history([relocated],locateWindow,{state:'relocated',event_id:'1',old_key:key('1',1),new_key:key('1',2),offset:3}),['New text'],{against:'request-locate-anchor-one'});
add('history-reset',history([],{kind:'latest'},{state:'reset',anchor:null,reason:'anchor_missing'}));
add('decisions-two-native-no-history',decisions([native(),native(41,nativeDisplay('fileChange/approval','Apply patch'))]),['commandExecution/approval','Run tests','fileChange/approval','Apply patch','Native runtime','Source-provided preview']);
const legacy={...native(),id:`legacy:${id(42)}`,identity_class:'legacy',kind:'legacy_approval',publication_state:known('Pending'),display:{kind:'legacy_approval',tool_name:field('tool_name','Read')},source_observations:[sourceObs('legacy_approvals')]};
add('legacy-label-without-history',decisions([legacy]),['Read','Legacy approval']);
const q=question(); add('generic-multiple-questions-options',decisions([q]),['Which checks?','Unit','Browser','Which branch?','Current branch','Answer not observed']);
const slots=['tracked','session'].map((mirror,i)=>({...question(),id:`question-slot:${session}:9007199254740993:${mirror}`,identity_class:'slot',publication_state:null,source_observations:[sourceObs(i?'session_question_slot':'tracked_question_slot')]}));
slots.push({...question(),id:`question-fallback:${session}`,identity_class:'slot',publication_state:null,source_observations:[sourceObs('durable_question_fallback')]});
add('runtime-durable-slots-keep-identity',decisions(slots),['Occurrence unknown',slots[0].id,slots[1].id,slots[2].id]);
const completedSlot={...slots[1],id:`question-slot:${session}:completed:session`};
add('completed-question-slot',decisions([completedSlot]),[completedSlot.id]);
const mirrors=native();mirrors.source_observations.push(sourceObs('native_publications'));mirrors.source_observations[0].incarnation=id(89);mirrors.source_observations[0].spawn_generation='18446744073709551615';mirrors.source_observations[0].witness_present=true;mirrors.source_observations[0].resolution_observed=true;mirrors.source_observations[0].resolution_persisted=false;mirrors.source_observations[0].writer_live=false;mirrors.source_observations[0].writer_capacity=0;mirrors.closure_state='ambiguous';mirrors.delivery_state='enqueued';mirrors.publication_state=known('enqueued');
add('native-independent-observation-axes',decisions([mirrors]),['Native runtime','Native publication']);
add('native-historical-fallback',decisions([native(43,nativeDisplay('historical/approval','Historical description'),'native_historical_fallback')]),['Native historical fallback','Historical description','Source-provided preview']);
const disagreement=native();disagreement.disagreement=true;disagreement.source_observations.push({...sourceObs('native_publications'),display_alternative:nativeDisplay('different/approval','Durable alternative')});
add('native-display-disagreement',decisions([disagreement]),['commandExecution/approval','different/approval','Durable alternative']);
const unavailable=native(44,{kind:'native_approval',method:field('method','Native approval — method unavailable','bounded_snapshot','unavailable',null),description:field('description','Description unavailable','bounded_snapshot','unavailable',null)});unavailable.details_state='unavailable';
const missingLegacy=copy(legacy);missingLegacy.display.tool_name=field('tool_name','Legacy approval — tool name unavailable','unknown','unavailable',null);missingLegacy.details_state='unavailable';
add('missing-native-legacy-display',decisions([unavailable,missingLegacy]),['Native approval — method unavailable','Description unavailable','Legacy approval — tool name unavailable']);
const cut=native(45,nativeDisplay('界'.repeat(170),'Description'));cut.display.method.state='truncated';cut.display.method.observed_bytes='2048';cut.details_state='truncated';
add('truncated-native-unicode-preview',decisions([cut]),['界'.repeat(170),'Truncated preview','Source-provided preview']);
const unavailableQ=question();unavailableQ.questions=[];unavailableQ.details_state='unavailable';
add('generic-details-unavailable',decisions([unavailableQ]),['Question details unavailable','Answer not observed']);
const omittedQ=question();omittedQ.details_state='truncated';omittedQ.omitted_questions='2';omittedQ.questions[0].omitted_options='4';
add('generic-omission-counts',decisions([omittedQ]),['Which checks?','Unit','Browser']);
const closed=native();closed.closure_state='closed';closed.publication_state=known('superseded');closed.requires_local_action=false;
add('selected-closed-identity-display',decisions([],{state:'present',decision:closed,stale:false}),[closed.id,'Run tests']);
const busy=decisions([],{state:'present',decision:native(),stale:true});busy.value.result.complete=false;busy.value.result.degraded=['busy','stale'];busy.value.result.coverage[4].state='busy';
add('selected-busy-stale-display',busy,['Run tests']);
add('selected-tombstone',decisions([],{state:'tombstone',id:legacy.id,identity_class:'legacy',last_display:legacy.display,message:'Decision no longer available'}),[legacy.id,'Read','Decision no longer available']);
add('selected-slot-tombstone',decisions([],{state:'tombstone',id:slots[0].id,identity_class:'slot',last_display:null,message:'Decision no longer available'}),[slots[0].id,'Occurrence unknown']);
const selectedUnavailable=decisions([],{state:'unavailable',id:legacy.id,last_display:legacy.display,reason:'busy'});selectedUnavailable.value.result.complete=false;selectedUnavailable.value.result.degraded=['busy'];selectedUnavailable.value.result.coverage[7].state='busy';
add('selected-unavailable',selectedUnavailable,[legacy.id,'Read']);
add('decisions-complete-empty',decisions([]));
const incomplete=decisions([]);incomplete.value.result.complete=false;incomplete.value.result.degraded=['limited'];incomplete.value.result.coverage[4].state='limited';incomplete.value.result.coverage[4].lower_bound='1';incomplete.value.result.coverage[4].has_more=true;
add('decisions-incomplete-empty',incomplete);
for (const code of ['invalid_request','not_found','stale_cursor','admission','resource_limit','busy','source_unavailable','upgrade_required','auth_required','access_denied','selection_required','selection_unavailable','conflict']) add(`error-${code}`,doc('error',{code,correlation_id:id(80),retry:{action:'retry',after_ms:500}}));
add('create-none',doc('create_view',{client_slot_id:id(91),selection:{kind:'none'}}));
const allocated=state();allocated.binding.attachment_generation='0';allocated.ready=false;allocated.lease.state='allocation';allocated.lease.duration_ms=10000;allocated.barrier.sequence='0';
add('allocated-none',doc('allocated_view',{state:allocated}));
add('attach-initial-zero',doc('attach_view',{binding:allocated.binding,expected_attachment_generation:'0',operation_id:id(92)}));
const attached=copy(allocated);attached.binding.attachment_generation='1';
add('attachment-ack',doc('attachment_ack',{state:attached,operation_id:id(92),previous_attachment_generation:'0'}));
add('view-none-ready',doc('view_state',state()));
add('notice-ready-none',doc('notice',{event:'ready',data:{state:state(),stream_id:{gateway_epoch:id(901),view_epoch:id(904),sequence:'9007199254740993'}}}));
const selection={kind:'project',project_id:project,session:null};
add('select-project',doc('select_view',{binding:binding(),expected_selection_generation:'1',operation_id:id(93),selection}));
add('selection-ack-project',doc('selection_ack',{state:state(selection,'2'),operation_id:id(93),previous_selection_generation:'1'}));
add('view-project-ready',doc('view_state',state(selection,'2')));
const sessionSelection={kind:'project',project_id:project,session:{session_id:session,history_window:{kind:'latest'},selected_decision_id:null}};
const sv=state(sessionSelection,'3');sv.barrier.pages=[page('history'),page('tail')];
add('view-session-ready',doc('view_state',sv));
add('selection-ack-return-none',doc('selection_ack',{state:state({kind:'none'},'4'),operation_id:id(94),previous_selection_generation:'3'}));
add('selection-ack-return-A',doc('selection_ack',{state:state(sessionSelection,'5'),operation_id:id(95),previous_selection_generation:'4'}));
const nativeBinding={window_id:id(97),window_generation:'9007199254740993',connection_generation:'9007199254740994'};
const boundProjects={binding:binding(),request_id:id(96),page_key:'RemoteListProjectsV1:configured:25',read:{method:methods[1],params:{limit:25,cursor:null}},native:nativeBinding};
add('bound-discovery',doc('bound_read',boundProjects),[],{view:'view-none-ready'});
const boundHistory={...boundProjects,binding:sv.binding,page_key:'RemoteGetHistoryPageV1:latest:25',read:request(4,{project_id:project,session_id:session,window:{kind:'latest'},limit:25,cursor:null}).value};
add('bound-history',doc('bound_read',boundHistory),[],{view:'view-session-ready'});
add('bound-response-discovery',doc('bound_response',{...boundProjects,read:response(1,{items:[{id:project,name:'Named project α'}]}).value,entry_incarnation:'9007199254740993',page_revision:'9007199254740994',cache_status:'refetched'}),['Named project α'],{against:'bound-discovery'});
add('ack-view',doc('ack_view',{binding:binding(),highest_contiguous_sequence:'9007199254740993',foreground_activity:false,native:nativeBinding}));
add('close-view',doc('close_view',{binding:binding(),operation_id:id(99)}));
for (const kind of ['page_changed','page_refetch_required']) add(`notice-${kind}`,doc('notice',{event:kind,data:{binding:binding(),barrier:{sequence:'3',pages:[page()]}}}));
add('notice-reset',doc('notice',{event:'reset',data:{binding:binding(),sequence:'3',reason:'epoch_changed'}}));
for (const kind of ['auth_required','service_unavailable']) add(`notice-${kind}`,doc('notice',{event:kind,data:{binding:binding(),sequence:'3'}}));
add('notice-selection-reset',doc('notice',{event:'selection_reset',data:{state:sv,stream_id:{gateway_epoch:id(901),view_epoch:id(904),sequence:sv.barrier.sequence}}}));
const nonce='A'.repeat(43);
add('bootstrap',doc('bootstrap',{nonce,expires_at:time}));add('create-app-session',doc('create_app_session',{nonce}));add('app-session',doc('app_session',{csrf_token:nonce,gateway_epoch:id(901),policy_epoch:id(902),idle_expires_at:time,absolute_expires_at:time}));

for (const name of ['info','projects','sessions','detail','history','decisions']) bad(`reject-extra-request-${name}`,`request-${name}`,[set('value/params/authority','operator')]);
bad('reject-extra-response','projects-one',[set('value/result/host_path','/private')]);
for (const key of ['version','daemon_epoch','observed_at','next_cursor','complete','projection_limits','degraded','coverage','items']) bad(`reject-missing-envelope-${key}`,'projects-one',[remove(`value/result/${key}`)]);
for (const key of ['own_title','parent_id','continued_from','attention','provider','status']) bad(`reject-missing-session-${key}`,'session-own-parent-continuation-providers',[remove(`value/result/items/0/${key}`)]);
for (const key of ['publication_state','delivery_state','closure_state','source_observations','display','questions','can_answer']) bad(`reject-missing-decision-${key}`,'decisions-two-native-no-history',[remove(`value/result/items/0/${key}`)]);
bad('reject-limit-zero','request-sessions',[set('value/params/limit',0)]);bad('reject-limit-over','request-sessions',[set('value/params/limit',101)]);
bad('reject-configured-over32','request-projects',[set('value/params/project_ids',Array.from({length:33},(_,i)=>id(i+1)))]);
bad('reject-duplicate-projects','request-projects',[set('value/params/project_ids',[project,project])]);
bad('reject-client-configured-project-ids','client-discovery-without-project-ids',[set('value/params/project_ids',[project])]);
bad('reject-uppercase-uuid','request-detail',[set('value/params/session_id','aaaaaaaa-AAAA-4000-8000-000000000010')]);
bad('reject-lossy-event-id','history-lossless-order-saved-answers',[set('value/result/items/0/id',9007199254740992)]);
for (const number of ['01','+1','-0','1e2','9223372036854775808']) bad(`reject-i64-${number}`,'history-lossless-order-saved-answers',[set('value/result/items/0/id',number)]);
bad('reject-u64-overflow','history-utf8-limit-truncation',[set('value/result/items/0/content_bytes','18446744073709551616')]);
bad('reject-sequence-overflow','history-lossless-order-saved-answers',[set('value/result/items/0/sequence',2147483648)]);
for (const timestamp of ['2026-09-08T12:34:56Z','2026-02-30T12:34:56.123456789Z','2026-09-08T24:00:00.123456789Z']) bad(`reject-timestamp-${timestamp}`,'projects-one',[set('value/result/observed_at',timestamp)]);
bad('reject-unknown-known-label','session-unknown-labels',[set('value/result/items/0/provider',{state:'known',value:'ProviderZ'})]);
bad('reject-unknown-label-oversized','session-unknown-labels',[set('value/result/items/0/provider/label','界'.repeat(43))]);
bad('reject-conflicting-window','request-history-older',[set('value/params/window/through',key())]);
bad('reject-inverted-window','request-history-interval',[set('value/params/window/upper_inclusive',key())]);
bad('reject-locate-wrong-id','request-history-locate',[set('value/params/window/event_id','2')]);
bad('reject-locate-offset','request-history-locate',[set('value/params/window/offset',8193)]);
bad('reject-wrong-cursor-kind','request-projects',[set('value/params/cursor',{kind:'sessions',token:'abc'})]);
bad('reject-cursor-whitespace','request-projects',[set('value/params/cursor',{kind:'projects',token:'abc def'})]);
bad('reject-complete-busy','decisions-incomplete-empty',[set('value/result/complete',true)]);
bad('reject-incomplete-no-degradation','decisions-incomplete-empty',[set('value/result/degraded',[])]);
bad('reject-missing-pending-source','decisions-complete-empty',[set('value/result/coverage',cov(pending.slice(1)))]);
bad('reject-history-reverse-order','history-lossless-order-saved-answers',[set('value/result/items',copy(events).reverse())]);
bad('reject-duplicate-history-id','history-lossless-order-saved-answers',[set('value/result/items/1/id',events[0].id)]);
bad('reject-short-display-as-key','history-tool-pair-display-separation',[set('value/result/items/3/tool_pair_key','共'.repeat(85))]);
bad('reject-pair-key-display-mismatch','history-tool-pair-display-separation',[set('value/result/items/0/tool_id_display','prefix')]);
bad('reject-content-length','history-lossless-order-saved-answers',[set('value/result/items/0/content_bytes','0')]);
bad('reject-event-over8192','history-lossless-order-saved-answers',[set('value/result/items/0/text','x'.repeat(8193))]);
bad('reject-response-projection-name-cap','projects-one',[set('value/result/projection_limits/name_bytes',1)]);
bad('reject-response-projection-event-cap','history-lossless-order-saved-answers',[set('value/result/projection_limits/event_text_bytes',1)]);
bad('reject-response-envelope-cap','projects-one',[set('value/result/projection_limits/envelope_bytes',1)]);
bad('reject-response-item-cap','projects-one',[set('value/result/projection_limits/item_bytes',1)]);
bad('reject-response-row-cap','projects-many',[set('value/result/projection_limits/page_items',1)]);
bad('reject-wrong-project-row','session-own-parent-continuation-providers',[set('value/result/items/0/project_id',id(2))]);
bad('reject-bad-capabilities','info-capabilities',[set('value/result/item/required_capabilities',methods.slice(1))]);
bad('reject-capability-epoch-mismatch','info-capabilities',[set('value/result/item/daemon_boot_id',id(999))]);
bad('reject-approval-invented-question','decisions-two-native-no-history',[set('value/result/items/0/questions',q.questions)]);
bad('reject-can-answer','decisions-two-native-no-history',[set('value/result/items/0/can_answer',true)]);
bad('reject-native-raw-target','decisions-two-native-no-history',[set('value/result/items/0/display/params',{command:'anything'})]);
bad('reject-native-source-full-original','decisions-two-native-no-history',[set('value/result/items/0/display/method/source_extent','full_field')]);
bad('reject-native-wrong-display-field','decisions-two-native-no-history',[set('value/result/items/0/display/method/source_field','tool_name')]);
bad('reject-native-missing-fallback','missing-native-legacy-display',[set('value/result/items/0/display/method/text','Unknown')]);
bad('reject-decision-source-kind','legacy-label-without-history',[set('value/result/items/0/source_observations/0/source','native_runtime')]);
bad('reject-historical-live-writer','native-historical-fallback',[set('value/result/items/0/source_observations/0/writer_live',true)]);
bad('reject-slot-project-identity','runtime-durable-slots-keep-identity',[set('value/result/items/0/id',`question-slot:${id(11)}:1:tracked`)]);
bad('reject-slot-publication-claim','runtime-durable-slots-keep-identity',[set('value/result/items/0/identity_class','publication')]);
bad('reject-question-count','generic-multiple-questions-options',[set('value/result/items/0/questions',Array(9).fill(q.questions[0]))]);
bad('reject-options-count','generic-multiple-questions-options',[set('value/result/items/0/questions/0/options',Array(9).fill(q.questions[0].options[0]))]);
bad('reject-omitted-with-complete','generic-multiple-questions-options',[set('value/result/items/0/omitted_questions','1')]);
bad('reject-tombstone-erased-identity','selected-tombstone',[remove('value/result/selected/id')]);
bad('reject-tombstone-invented-replacement','selected-tombstone',[set('value/result/selected/message','Nothing here')]);
bad('reject-error-diagnostics','error-busy',[set('value/diagnostic','raw error')]);
bad('reject-error-retry-bound','error-busy',[set('value/retry/after_ms',30001)]);
bad('reject-create-selected','create-none',[set('value/selection',selection)]);
bad('reject-none-project-field','create-none',[set('value/selection/project_id',project)]);
bad('reject-latest-anchor-field','request-history',[set('value/params/window/anchor',key())]);
bad('reject-unchanged-anchor-field','history-successful-empty',[set('value/result/position/anchor',key())]);
bad('reject-generic-display-approval-field','generic-multiple-questions-options',[set('value/result/items/0/display/tool_name',field('tool_name','Invented'))]);
bad('reject-selected-none-id-field','decisions-complete-empty',[set('value/result/selected/id',legacy.id)]);
bad('reject-unknown-label-extra-field','session-unknown-labels',[set('value/result/items/0/provider/extra','hidden')]);
bad('reject-complete-hidden-continuation','filtered-empty-continuation',[set('value/result/next_cursor',null)]);
bad('reject-history-failure-as-empty','history-successful-empty',[set('value/result/complete',false),set('value/result/degraded',['unavailable']),set('value/result/coverage/0/state','unavailable')]);
bad('reject-zero-prior-selection-generation','selection-ack-project',[set('value/previous_selection_generation','0'),set('value/state/binding/selection_generation','1')]);
bad('reject-cross-session-selected-slot','request-decisions',[set('value/params/selected_decision_id',`question-slot:${id(11)}:1:tracked`)]);
bad('reject-inconsistent-degradation','decisions-incomplete-empty',[set('value/result/degraded',['busy'])]);
bad('reject-generation-number','select-project',[set('value/binding/selection_generation',1)]);
bad('reject-cas-wrong-generation','select-project',[set('value/expected_selection_generation','2')]);
bad('reject-attach-wrong-generation','attach-initial-zero',[set('value/expected_attachment_generation','1')]);
bad('reject-ack-no-advance','selection-ack-project',[set('value/state/binding/selection_generation','1')]);
bad('reject-ready-zero-attachment','notice-ready-none',[set('value/data/state/binding/attachment_generation','0')]);
bad('reject-ready-epoch-mismatch','notice-ready-none',[set('value/data/stream_id/view_epoch',id(999))]);
bad('reject-none-history-slot','view-none-ready',[set('value/barrier/pages',[page('history')])]);
bad('reject-unbounded-barrier','view-session-ready',[set('value/barrier/pages',Array(9).fill(page()))]);
bad('reject-revision-wrap','bound-response-discovery',[set('value/page_revision','0')]);
bad('reject-allocation-lease','allocated-none',[set('value/state/lease/duration_ms',25000)]);
bad('reject-body-in-notice','notice-page_changed',[set('value/data/transcript','private history')]);
bad('reject-native-zero-generation','bound-discovery',[set('value/native/window_generation','0')]);
bad('reject-request-native-url','bound-discovery',[set('value/native/url','https://example.com')]);
bad('reject-bootstrap-nonce','bootstrap',[set('value/nonce','B'.repeat(43))]);
bad('reject-cross-project-exchange','projects-many',[],{against:'request-projects'});
bad('reject-cross-method-exchange','projects-one',[],{against:'request-info'});
bad('reject-none-object-read','bound-history',[set('value/binding',binding())],{view:'view-none-ready'});
bad('reject-old-generation-read','bound-history',[set('value/binding/selection_generation','1')],{view:'view-session-ready'});
bad('reject-not-ready-read','bound-discovery',[set('value/binding',allocated.binding)],{view:'view-none-ready'});
bad('reject-late-A-B-A-response','bound-response-discovery',[set('value/binding/selection_generation','5')],{against:'bound-discovery'});
bad('reject-response-request-id','bound-response-discovery',[set('value/request_id',id(100))],{against:'bound-discovery'});
bad('reject-response-native-generation','bound-response-discovery',[set('value/native/connection_generation','9007199254740995')],{against:'bound-discovery'});
for (const [name,raw] of Object.entries({duplicate:'{"type":"request","type":"request","value":{"method":"RemoteGetInfoV1","params":{}}}',float:'{"type":"request","value":{"method":"RemoteListSessionsV1","params":{"project_id":"'+project+'","limit":1.0}}}',exponent:'{"type":"request","value":{"method":"RemoteListSessionsV1","params":{"project_id":"'+project+'","limit":1e1}}}',negativeZero:'{"type":"request","value":{"method":"RemoteListSessionsV1","params":{"project_id":"'+project+'","limit":-0}}}',surrogate:'{"type":"request","value":{"method":"RemoteListSessionsV1","params":{"project_id":"\\ud800"}}}'})) cases.push({id:`reject-raw-${name}`,valid:false,raw});
cases.push({id:'reject-raw-request-size',valid:false,raw:' '.repeat(16385)+JSON.stringify(request(0))});
cases.push({id:'reject-raw-utf8',valid:false,raw_hex:'fffe'});
cases.push({id:'reject-raw-bom',valid:false,raw_hex:'efbbbf'+Buffer.from(JSON.stringify(request(0))).toString('hex')});

// I0A-R001: closed objects never accept Rust's positional struct form.
bad('r001-info-params-array','request-info',[set('value/params',[])]);
bad('r001-session-params-array','request-sessions',[set('value/params',[project,50,null])]);
bad('r001-project-row-array','projects-one',[set('value/result/items/0',[project,'Named project α'])]);
bad('r001-key-array','request-history-locate',[set('value/params/window/old_key',[1,'9007199254740993'])]);
bad('r001-nested-display-array','legacy-label-without-history',[set('value/result/items/0/display/tool_name',Object.values(legacy.display.tool_name))]);
bad('r001-source-observation-array','legacy-label-without-history',[set('value/result/items/0/source_observations/0',Object.values(sourceObs('legacy_approvals')))]);
bad('r001-binding-array','bound-discovery',[set('value/binding',Object.values(binding()))]);
bad('r001-adjacent-method-array','request-info',[set('value',['RemoteGetInfoV1',{}])]);
cases.push({id:'r001-adjacent-document-array',valid:false,input:['request',{method:methods[0],params:{}}]});

// I0A-R002: every locate disposition refers to the requested occurrence/key.
add('r002-locate-unchanged',history([event('1',1,'Original anchor')],locateWindow),['Original anchor'],{against:'request-locate-anchor-one'});
add('r002-locate-missing-reset',history([],locateWindow,{state:'reset',anchor:null,reason:'anchor_missing'}),[],{against:'request-locate-anchor-one'});
bad('r002-relocation-other-event','history-relocation',[
  set('value/result/items/0/id','2'),set('value/result/head/id','2'),set('value/result/interval/upper_inclusive/id','2'),
  set('value/result/position/event_id','2'),set('value/result/position/old_key/id','2'),set('value/result/position/new_key/id','2'),
],{against:'request-locate-anchor-one'});
bad('r002-relocation-other-old-key','history-relocation',[set('value/result/position/old_key/sequence',0)],{against:'request-locate-anchor-one'});
bad('r002-unchanged-missing-anchor','r002-locate-missing-reset',[set('value/result/position',{state:'unchanged'})],{against:'request-locate-anchor-one'});
bad('r002-unchanged-moved-anchor','history-relocation',[set('value/result/position',{state:'unchanged'})],{against:'request-locate-anchor-one'});
bad('r002-exchange-other-request','history-relocation',[],{against:'request-history-locate'});

// I0A-R003: persisting native previews never upgrades them to full fields.
add('r003-native-publication-preview',decisions([native(47,nativeDisplay('publication/approval','Stored preview'),'native_publications')]),['publication/approval','Stored preview','Source-provided preview']);
const nativeTombstone=decisions([],{state:'tombstone',id:closed.id,identity_class:'publication',last_display:closed.display,message:'Decision no longer available'});
add('r003-native-tombstone-preview',nativeTombstone,[closed.id,'Run tests','Source-provided preview']);
const nativeUnavailable=copy(selectedUnavailable);nativeUnavailable.value.result.selected={state:'unavailable',id:closed.id,last_display:closed.display,reason:'busy'};
add('r003-native-unavailable-preview',nativeUnavailable,[closed.id,'Run tests','Source-provided preview']);
for (const base of ['decisions-two-native-no-history','native-historical-fallback','r003-native-publication-preview']) for (const f of ['method','description']) {
  bad(`r003-full-${base}-${f}`,base,[set(`value/result/items/0/display/${f}/source_extent`,'full_field')]);
}
for (const f of ['method','description']) {
  bad(`r003-full-alternative-${f}`,'native-display-disagreement',[set(`value/result/items/0/source_observations/1/display_alternative/${f}/source_extent`,'full_field')]);
  for (const base of ['r003-native-tombstone-preview','r003-native-unavailable-preview']) bad(`r003-full-${base}-${f}`,base,[set(`value/result/selected/last_display/${f}/source_extent`,'full_field')]);
}

// I0A-R004: source observations witness the identity; optional facts stay null.
const exactSlot=copy(slots[0]);exactSlot.source_observations[0].spawn_generation='9007199254740993';
add('r004-slot-present-generation',decisions([exactSlot]),[exactSlot.id,'Which checks?']);
const equalMirrors=copy(exactSlot);equalMirrors.source_observations.push({...sourceObs('session_question_slot'),spawn_generation:'9007199254740993'});
add('r004-complete-runtime-mirrors',decisions([equalMirrors]),[equalMirrors.id,'Checks','Unit','Browser']);
const completedObserved=copy(completedSlot);completedObserved.source_observations[0].spawn_generation='7';
add('r004-completed-generation-observation',decisions([completedObserved]),[completedObserved.id,'Which checks?']);
bad('r004-runtime-as-publication','generic-multiple-questions-options',[set('value/result/items/0/source_observations',[sourceObs('tracked_question_slot')])]);
bad('r004-runtime-as-durable-fallback','runtime-durable-slots-keep-identity',[set('value/result/items/2/source_observations',[sourceObs('tracked_question_slot')])]);
bad('r004-slot-missing-primary','r004-complete-runtime-mirrors',[set('value/result/items/0/source_observations',[equalMirrors.source_observations[1]])]);
for (const n of [0,1]) bad(`r004-slot-generation-mismatch-${n}`,'r004-complete-runtime-mirrors',[set(`value/result/items/0/source_observations/${n}/spawn_generation`,'2')]);
bad('r004-publication-cannot-absorb-slot','generic-multiple-questions-options',[set('value/result/items/0/source_observations',[sourceObs('question_publications'),sourceObs('tracked_question_slot')])]);
bad('r004-fallback-cannot-absorb-slot','runtime-durable-slots-keep-identity',[set('value/result/items/2/source_observations',[sourceObs('durable_question_fallback'),sourceObs('tracked_question_slot')])]);
bad('r004-mirrors-disagree','r004-complete-runtime-mirrors',[set('value/result/items/0/disagreement',true)]);
bad('r004-mirrors-truncated','r004-complete-runtime-mirrors',[set('value/result/items/0/details_state','truncated')]);
bad('r004-mirrors-incomplete','r004-complete-runtime-mirrors',[set('value/result/complete',false),set('value/result/degraded',['busy']),set('value/result/items/0/source_observations/1/state','busy')]);

// I0A-R005: known vocabularies and generic/legacy closure mappings are exact.
for (const [kind,template,labels] of [
  ['generic',q,['unresolved','published','cleared']],['legacy',legacy,['Pending','Approved','Denied']],
  ['native',native(),['unresolved','published','enqueued','expired','superseded']],
]) {
  for (const label of labels) {
    const d=copy(template);d.publication_state=known(label);
    d.closure_state=kind==='native'?'ambiguous':['cleared','Approved','Denied'].includes(label)?'closed':'open';
    d.requires_local_action=d.closure_state!=='closed';
    const base=`r005-${kind}-${label}`;add(base,decisions([d]),[label]);
    if (kind!=='native') bad(`${base}-wrong-closure`,base,[set('value/result/items/0/closure_state',d.closure_state==='closed'?'open':'closed')]);
  }
  const d=copy(template);d.publication_state={state:'unknown',label:`Future ${kind} state`};d.closure_state='unknown';
  add(`r005-${kind}-unknown-state`,decisions([d]),[`Unknown: Future ${kind} state`]);
}
bad('r005-legacy-pending-hidden-attention','legacy-label-without-history',[set('value/result/items/0/closure_state','closed'),set('value/result/items/0/requires_local_action',false)]);
bad('r005-legacy-pending-no-inspection','legacy-label-without-history',[set('value/result/items/0/requires_local_action',false)]);
bad('r005-question-native-label','generic-multiple-questions-options',[set('value/result/items/0/publication_state',known('enqueued'))]);
bad('r005-legacy-generic-label','legacy-label-without-history',[set('value/result/items/0/publication_state',known('cleared'))]);
bad('r005-native-legacy-label','decisions-two-native-no-history',[set('value/result/items/0/publication_state',known('Pending'))]);
bad('r005-native-generic-label','decisions-two-native-no-history',[set('value/result/items/0/publication_state',known('cleared'))]);
const closedEnqueued=copy(mirrors);closedEnqueued.closure_state='closed';closedEnqueued.requires_local_action=false;
add('r005-native-independent-closed-enqueued',decisions([closedEnqueued]),['enqueued','closed','Run tests']);

// I0A-R006: current nested refusals affect envelope evidence and attention.
const fixtureInput = name => copy(cases.find(c=>c.id===name).input);
for (const base of ['client-discovery-without-project-ids','info-capabilities','notice-ready-none']) {
  bad(`r001-tagged-wrapper-array-${base}`,base,[set('value',Object.values(fixtureInput(base).value))]);
}
for (const reason of ['busy','limited','unavailable']) {
  const detail=fixtureInput('detail-runtime-only-empty-history');
  detail.value.result.complete=false;detail.value.result.degraded=[reason];detail.value.result.item.pending_coverage[4].state=reason;
  detail.value.result.item.summary.attention.incomplete=true;detail.value.result.item.summary.attention.requires_local_action=true;
  const base=`r006-detail-${reason}`;add(base,detail,['Own session 10','Investigate contracts']);
  bad(`${base}-wrong-degradation`,base,[set('value/result/degraded',['truncated'])]);
  bad(`${base}-missing-attention`,base,[set('value/result/item/summary/attention/incomplete',false),set('value/result/item/summary/attention/requires_local_action',false)]);
  bad(`${base}-missing-incomplete`,base,[set('value/result/item/summary/attention/incomplete',false)]);
}
bad('r006-detail-reported-counterexample','detail-runtime-only-empty-history',[set('value/result/complete',false),set('value/result/degraded',['truncated']),set('value/result/item/pending_coverage/4/state','busy')]);
bad('r006-duplicate-pending-order','detail-runtime-only-empty-history',[set('value/result/item/pending_coverage/1/observation_order',1)]);
bad('r006-current-card-complete-envelope','decisions-two-native-no-history',[set('value/result/items/0/source_observations/0/state','busy')]);
const currentBusy=decisions([native()]);currentBusy.value.result.complete=false;currentBusy.value.result.degraded=['busy'];currentBusy.value.result.coverage[4].state='busy';currentBusy.value.result.items[0].source_observations[0].state='busy';
add('r006-current-card-busy',currentBusy,['Run tests']);
bad('r006-current-card-missing-reason','r006-current-card-busy',[set('value/result/coverage/4/state','complete'),set('value/result/degraded',['truncated'])]);
const currentSelected=copy(currentBusy);currentSelected.value.result.selected={state:'present',decision:currentSelected.value.result.items[0],stale:false};currentSelected.value.result.items=[];
add('r006-current-selected-busy',currentSelected,['Run tests']);
bad('r006-current-selected-complete','r006-current-selected-busy',[set('value/result/complete',true),set('value/result/degraded',[]),set('value/result/coverage/4/state','complete')]);
const retainedBusy=decisions([],{state:'present',decision:copy(currentBusy.value.result.items[0]),stale:true});retainedBusy.value.result.degraded=['stale'];
add('r006-retained-busy-current-scan-complete',retainedBusy,['Run tests','stale']);
bad('r006-retained-missing-stale-label','r006-retained-busy-current-scan-complete',[set('value/result/degraded',[])]);
bad('r006-retained-claimed-current','r006-retained-busy-current-scan-complete',[set('value/result/selected/stale',false)]);

// I0A-R007/R008: auxiliary payload bounds include the retained projection.
const retainedQuestions={kind:'generic_questions',questions:copy(q.questions),omitted_questions:'0',details_state:'complete'};
const genericTombstone=decisions([],{state:'tombstone',id:q.id,identity_class:'publication',last_display:retainedQuestions,message:'Decision no longer available'});
add('r008-generic-tombstone',genericTombstone,[q.id,'Checks','Which checks?','Unit','Fast tests','Browser','Browser interactions','Branch','Which branch?','Current branch']);
const genericUnavailable=copy(selectedUnavailable);genericUnavailable.value.result.selected={state:'unavailable',id:q.id,last_display:retainedQuestions,reason:'busy'};
add('r008-generic-unavailable',genericUnavailable,[q.id,'Which checks?','Unit','Browser','Current branch']);
const retainedOmitted=copy(genericTombstone);retainedOmitted.value.result.selected.last_display={kind:'generic_questions',questions:copy(omittedQ.questions),omitted_questions:'2',details_state:'truncated'};
add('r008-retained-omission-counts',retainedOmitted,['Which checks?','Unit','Browser','Truncated preview']);
const retainedUnknown=copy(genericUnavailable);retainedUnknown.value.result.selected.last_display={kind:'generic_questions',questions:[],omitted_questions:'0',details_state:'unavailable'};
add('r008-retained-details-unavailable',retainedUnknown,[q.id,'Question details unavailable']);
const jsonBytes = v => Buffer.byteLength(JSON.stringify(v));
const stringBytes = v => typeof v==='string'?Buffer.byteLength(v):v && typeof v==='object'?Object.values(v).reduce((n,x)=>n+stringBytes(x),0):0;
const projectExact=fixtureInput('selected-empty-project');projectExact.value.result.projection_limits.name_bytes=Buffer.byteLength(projectExact.value.result.project.name);projectExact.value.result.projection_limits.item_bytes=jsonBytes(projectExact.value.result.project);
add('r007-project-exact-limits',projectExact,['Selected empty project',project]);
for (const limit of ['name_bytes','item_bytes']) {
  bad(`r007-project-${limit}-one`,'selected-empty-project',[set(`value/result/projection_limits/${limit}`,1)]);
  bad(`r007-project-${limit}-one-below`,'r007-project-exact-limits',[set(`value/result/projection_limits/${limit}`,projectExact.value.result.projection_limits[limit]-1)]);
}
for (const original of ['selected-tombstone','selected-unavailable','selected-closed-identity-display','r008-generic-tombstone','r008-generic-unavailable']) {
  const d=fixtureInput(original), p=d.value.result.projection_limits;
  p.item_bytes=jsonBytes(d.value.result.selected);p.decision_text_bytes=stringBytes(d.value.result.selected);
  const name=`r007-exact-${original}`;add(name,d,cases.find(c=>c.id===original).visible);
  for (const limit of ['item_bytes','decision_text_bytes']) {
    bad(`r007-${original}-${limit}-one`,original,[set(`value/result/projection_limits/${limit}`,1)]);
    bad(`r007-${original}-${limit}-one-below`,name,[set(`value/result/projection_limits/${limit}`,p[limit]-1)]);
  }
}
bad('r008-empty-generic-retention','r008-generic-tombstone',[set('value/result/selected/last_display',{kind:'generic_questions'})]);
bad('r008-nine-retained-questions','r008-generic-tombstone',[set('value/result/selected/last_display/questions',Array(9).fill(q.questions[0]))]);
bad('r008-nine-retained-options','r008-generic-unavailable',[set('value/result/selected/last_display/questions/0/options',Array(9).fill(q.questions[0].options[0]))]);
bad('r008-oversized-retained-header','r008-generic-tombstone',[set('value/result/selected/last_display/questions/0/header','界'.repeat(43))]);
bad('r008-oversized-retained-question','r008-generic-tombstone',[set('value/result/selected/last_display/questions/0/question','x'.repeat(2049))]);
bad('r008-oversized-retained-option','r008-generic-unavailable',[set('value/result/selected/last_display/questions/0/options/0/description','x'.repeat(513))]);
bad('r008-invalid-multi-select','r008-generic-unavailable',[set('value/result/selected/last_display/questions/0/multi_select','true')]);
bad('r008-hidden-question-omission','r008-generic-tombstone',[set('value/result/selected/last_display/omitted_questions','1')]);
bad('r008-hidden-option-omission','r008-generic-tombstone',[set('value/result/selected/last_display/questions/0/omitted_options','1')]);
bad('r008-retained-projection-over8k','r008-generic-tombstone',[set('value/result/selected/last_display/questions',Array(8).fill({...q.questions[0],question:'q'.repeat(2048)}))]);
bad('r008-invented-approval-questions','r003-native-tombstone-preview',[set('value/result/selected/last_display/questions',q.questions)]);
bad('r001-retained-question-array','r008-generic-tombstone',[set('value/result/selected/last_display/questions/0',Object.values(q.questions[0]))]);

// I0A-R009: project-only selection has discovery/session-list roles only.
const projectRoles=state(selection,'2');projectRoles.barrier.pages=['info','projects','sessions'].map(page);
add('r009-project-ready-roles',doc('view_state',projectRoles),[project]);
add('r009-project-selection-ack-roles',doc('selection_ack',{state:projectRoles,operation_id:id(93),previous_selection_generation:'1'}),[project]);
add('r009-project-selection-reset-roles',doc('notice',{event:'selection_reset',data:{state:projectRoles,stream_id:{gateway_epoch:id(901),view_epoch:id(904),sequence:projectRoles.barrier.sequence}}}),[project]);
for (const role of ['foreground','history','tail']) {
  bad(`r009-project-forbidden-${role}`,'view-project-ready',[set('value/barrier/pages',[page(role)])]);
  bad(`r009-project-reset-forbidden-${role}`,'r009-project-selection-reset-roles',[set('value/data/state/barrier/pages',[page(role)])]);
  bad(`r009-project-ack-forbidden-${role}`,'r009-project-selection-ack-roles',[set('value/state/barrier/pages',[page(role)])]);
}

// C001/R001: every tagged variant has an object control and a sequence rejection.
// Controls and ordered field values come from authored fixtures, never codecs.
const canonicalInput = name => {
  const c=cases.find(c=>c.id===name);return copy(c.expected ?? c.input);
};
const unionVariants=new Map();
function unionPair(type,variant,input,path,visible=[]) {
  const label=`${type}/${variant}`;
  if(unionVariants.has(label))return;
  unionVariants.set(label,true);
  const name=`c001-${type}-${variant}`, object=path.reduce((v,k)=>v[k],input);
  add(`${name}-object`,input,visible);
  const positional=Object.values(object);
  if(path.length)bad(`${name}-array`,`${name}-object`,[{op:'set',path,value:positional}]);
  else cases.push({id:`${name}-array`,valid:false,input:positional});
}
const requestNames=['request-info','request-projects','request-sessions','request-detail','request-history','request-decisions'];
const responseNames=['info-capabilities','projects-one','selected-empty-project','detail-runtime-only-empty-history','history-relocation','generic-multiple-questions-options'];
for(let i=0;i<methods.length;i++) {
  const req=canonicalInput(requestNames[i]);
  unionPair('ReadRequestV1',methods[i],req,['value']);
  const client=copy(req.value);if(i===1)client.params={limit:25,cursor:null};
  unionPair('ViewReadRequestV1',methods[i],doc('view_request',client),['value']);
  unionPair('ReadResponseV1',methods[i],canonicalInput(responseNames[i]),['value']);
}
for(const [variant,i] of [['projects',1],['sessions',2],['history',4],['decisions',5]]) {
  const req=canonicalInput(requestNames[i]);req.value.params.cursor={kind:variant,token:`cursor-for-${variant}`};
  unionPair('CursorV1',variant,req,['value','params','cursor']);
}
for(const variant of ['latest','older','newer','interval','locate']) {
  unionPair('HistoryWindowV1',variant,canonicalInput(variant==='latest'?'request-history':`request-history-${variant}`),['value','params','window']);
}
for(const [variant,base] of [['unchanged','history-successful-empty'],['relocated','history-relocation'],['reset','history-reset']]) {
  unionPair('HistoryPositionV1',variant,canonicalInput(base),['value','result','position']);
}
for(const [variant,base] of [['generic_questions','generic-multiple-questions-options'],['native_approval','decisions-two-native-no-history'],['legacy_approval','legacy-label-without-history']]) {
  unionPair('DecisionDisplayV1',variant,canonicalInput(base),['value','result','items','0','display']);
}
for(const [variant,base] of [['generic_questions','r008-generic-tombstone'],['native_approval','r003-native-tombstone-preview'],['legacy_approval','selected-tombstone']]) {
  unionPair('RetainedDecisionDisplayV1',variant,canonicalInput(base),['value','result','selected','last_display'],cases.find(c=>c.id===base).visible);
}
for(const [variant,base] of [['none','decisions-complete-empty'],['present','selected-closed-identity-display'],['tombstone','r008-generic-tombstone'],['unavailable','r003-native-unavailable-preview']]) {
  unionPair('SelectedDecisionV1',variant,canonicalInput(base),['value','result','selected'],cases.find(c=>c.id===base).visible);
}
unionPair('SelectionV1','none',canonicalInput('create-none'),['value','selection']);
unionPair('SelectionV1','project',canonicalInput('view-project-ready'),['value','selection']);
for(const [type,base,path] of [
  ['ProviderV1','session-own-parent-continuation-providers',['value','result','items','0','provider']],
  ['SessionKindV1','session-own-parent-continuation-providers',['value','result','items','0','kind']],
  ['SessionStatusV1','session-own-parent-continuation-providers',['value','result','items','0','status']],
  ['EventKindV1','history-relocation',['value','result','items','0','kind']],
  ['RoleV1','history-relocation',['value','result','items','0','role']],
  ['PublicationStateV1','generic-multiple-questions-options',['value','result','items','0','publication_state']],
]) {
  unionPair(type,'known',canonicalInput(base),path);
  const unknown=canonicalInput(base), parent=path.slice(0,-1).reduce((v,k)=>v[k],unknown);
  parent[path.at(-1)]={state:'unknown',label:`Future ${type}`};
  unionPair(type,'unknown',unknown,path,[`Unknown: Future ${type}`]);
}
for(const c of [...cases].filter(c=>c.valid)) {
  const input=copy(c.expected ?? c.input);
  unionPair('WireDocumentV1',input.type,input,[],c.visible);
  if(input.type==='notice')unionPair('NoticeV1',input.value.event,input,['value'],c.visible);
}
const expectedUnions={
  CursorV1:['projects','sessions','history','decisions'],HistoryWindowV1:['latest','older','newer','interval','locate'],
  HistoryPositionV1:['unchanged','relocated','reset'],DecisionDisplayV1:['generic_questions','native_approval','legacy_approval'],
  RetainedDecisionDisplayV1:['generic_questions','native_approval','legacy_approval'],SelectedDecisionV1:['none','present','tombstone','unavailable'],SelectionV1:['none','project'],
  ProviderV1:['known','unknown'],SessionKindV1:['known','unknown'],SessionStatusV1:['known','unknown'],EventKindV1:['known','unknown'],RoleV1:['known','unknown'],PublicationStateV1:['known','unknown'],
  ReadRequestV1:methods,ViewReadRequestV1:methods,ReadResponseV1:methods,NoticeV1:['ready','selection_reset','page_changed','page_refetch_required','reset','auth_required','service_unavailable'],
  WireDocumentV1:['request','view_request','response','error','create_view','allocated_view','attach_view','attachment_ack','select_view','selection_ack','view_state','notice','ack_view','close_view','bound_read','bound_response','bootstrap','create_app_session','app_session'],
};
const requiredUnionVariants=Object.entries(expectedUnions).flatMap(([type,variants])=>variants.map(v=>`${type}/${v}`));
if(unionVariants.size!==requiredUnionVariants.length || requiredUnionVariants.some(v=>!unionVariants.has(v)))throw Error('Incomplete tagged-union fixture matrix');
const defaultSelection=canonicalInput('view-project-ready');delete defaultSelection.value.selection.session;
add('c001-project-default-session',defaultSelection,[project],{expected:canonicalInput('view-project-ready')});
bad('c001-reset-missing-required-nullable','history-reset',[remove('value/result/position/anchor')]);
bad('c001-selected-missing-required-nullable','selected-slot-tombstone',[remove('value/result/selected/last_display')]);
bad('c001-ready-nested-selection-array','notice-ready-none',[set('value/data/state/selection',['none'])]);
bad('c001-bound-history-window-array','bound-history',[set('value/read/params/window',['latest'])]);
bad('c001-native-unavailable-retained-array','r003-native-unavailable-preview',[set('value/result/selected/last_display',Object.values(nativeUnavailable.value.result.selected.last_display))]);
bad('c001-retained-generic-unavailable-array','r008-generic-unavailable',[set('value/result/selected/last_display',Object.values(retainedQuestions))]);

// C002/R004: stipulated complete source equality admits the durable mirror;
// runtime slots still cannot acquire publication identity from matching text.
const publicationMirror=question();
publicationMirror.source_observations.push({...copy(publicationMirror.source_observations[0]),source:'durable_question_fallback'});
const mirrorHints=[q.id,'question_publications','durable_question_fallback','Checks','Which checks?','Unit','Fast tests','Browser','Browser interactions','Branch','Which branch?','Current branch'];
add('c002-complete-durable-publication-mirror',decisions([publicationMirror]),mirrorHints);
add('c002-selected-durable-publication-mirror',decisions([],{state:'present',decision:publicationMirror,stale:false}),mirrorHints);
const reverseMirror=copy(publicationMirror);reverseMirror.source_observations.reverse();
add('c002-durable-mirror-primary-second',decisions([reverseMirror]),mirrorHints);
const clearedMirror=copy(publicationMirror);clearedMirror.publication_state=known('cleared');clearedMirror.closure_state='closed';clearedMirror.requires_local_action=false;
add('c002-selected-cleared-durable-mirror',decisions([],{state:'present',decision:clearedMirror,stale:false}),mirrorHints);
const distinctFallback=copy(slots[2]);distinctFallback.disagreement=true;distinctFallback.details_state='truncated';distinctFallback.omitted_questions='1';
distinctFallback.questions[0].question='Which separate fallback checks?';distinctFallback.questions[0].omitted_options='2';
add('c002-inconsistent-fallback-remains-separate',decisions([q,distinctFallback]),[q.id,distinctFallback.id,'question_publications','durable_question_fallback','Which checks?','Which separate fallback checks?','Unit','Browser','Truncated preview']);
for(const [name,path,value] of [
  ['missing-publication','source_observations',[sourceObs('durable_question_fallback')]],
  ['runtime-substitution','source_observations',[sourceObs('tracked_question_slot'),sourceObs('durable_question_fallback')]],
  ['runtime-absorption','source_observations',[sourceObs('question_publications'),sourceObs('durable_question_fallback'),sourceObs('session_question_slot')]],
  ['disagreement','disagreement',true],['truncated-details','details_state','truncated'],['unavailable-details','details_state','unavailable'],
  ['omitted-sources','omitted_source_observations','1'],['omitted-questions','omitted_questions','1'],['omitted-options','questions/0/omitted_options','1'],
]) bad(`c002-reject-${name}`,'c002-complete-durable-publication-mirror',[set(`value/result/items/0/${path}`,value)]);
bad('c002-reject-truncated-prefix-equality','c002-complete-durable-publication-mirror',[set('value/result/items/0/details_state','truncated'),set('value/result/items/0/omitted_questions','1'),set('value/result/items/0/questions/0/omitted_options','1')]);
for(const index of [0,1])for(const reason of ['busy','limited','unavailable']) {
  bad(`c002-reject-incomplete-witness-${index}-${reason}`,'c002-complete-durable-publication-mirror',[
    set('value/result/complete',false),set('value/result/degraded',[reason]),set(`value/result/coverage/${index===0?0:3}/state`,reason),set(`value/result/items/0/source_observations/${index}/state`,reason),
  ]);
}
bad('c002-reject-selected-missing-publication','c002-selected-durable-publication-mirror',[set('value/result/selected/decision/source_observations',[sourceObs('durable_question_fallback')])]);
bad('c002-reject-selected-truncated-mirror','c002-selected-durable-publication-mirror',[set('value/result/selected/decision/details_state','truncated')]);
console.log(`Tagged-union controls: ${requiredUnionVariants.length} variants across ${Object.keys(expectedUnions).length} families`);

const output=JSON.stringify({schema_version:1,cases},null,2)+'\n';
const path=new URL('corpus.json',import.meta.url);
if(process.argv.includes('--check')) { if(readFileSync(path,'utf8')!==output)throw Error('Fixture corpus is stale'); }
else writeFileSync(path,output);
console.log(`Fixture corpus: ${cases.filter(c=>c.valid).length} positive, ${cases.filter(c=>!c.valid).length} negative`);
