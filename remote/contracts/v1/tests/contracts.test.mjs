import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { run } from './support.mjs';
import { decode, encode } from '../dist/index.js';
test('shared wire fixtures preserve identity and reject invalid boundaries', () => { run(); });
test('producer revalidates programmatically changed contracts', () => {
  const d=decode('{"type":"request","value":{"method":"RemoteListSessionsV1","params":{"project_id":"00000000-0000-4000-8000-000000000001"}}}');
  d.value.params.limit=101;
  assert.throws(()=>encode(d),{message:'invalid Remote V1 wire value'});
});
test('producer rejects number-to-null and unknown-field serialization losses', () => {
  const d=decode('{"type":"request","value":{"method":"RemoteGetDecisionsV1","params":{"project_id":"00000000-0000-4000-8000-000000000001","session_id":"00000000-0000-4000-8000-000000000002"}}}');
  d.value.params.cursor=NaN;
  assert.throws(()=>encode(d),{message:'invalid Remote V1 wire value'});
  d.value.params.cursor=null; d.value.params.unrecognized=undefined;
  assert.throws(()=>encode(d),{message:'invalid Remote V1 wire value'});
});
test('correction-review controls preserve retained questions and durable publication provenance', () => {
  const corpus=JSON.parse(readFileSync(new URL('../fixtures/corpus.json',import.meta.url),'utf8'));
  const fixture=id=>structuredClone(corpus.cases.find(c=>c.id===id).input);
  const selection=fixture('create-none'), retained=fixture('r008-generic-tombstone'), publication=fixture('generic-multiple-questions-options');
  const selectionArray=structuredClone(selection);selectionArray.value.selection=['none'];
  const retainedArray=structuredClone(retained);retainedArray.value.result.selected.last_display=Object.values(retained.value.result.selected.last_display);
  const mirror=structuredClone(publication);mirror.value.result.items[0].source_observations.push({...structuredClone(publication.value.result.items[0].source_observations[0]),source:'durable_question_fallback'});
  const inputs=[selection,selectionArray,retained,retainedArray,publication,mirror];
  const expected=[true,false,true,false,true,true];
  const outputs=inputs.map((input,i)=>{
    if(!expected[i]) {assert.throws(()=>decode(JSON.stringify(input)),{message:'invalid Remote V1 wire value'});return null;}
    const output=JSON.parse(encode(decode(JSON.stringify(input))));assert.deepEqual(output,input);return output;
  });
  const questions=[
    {header:'Checks',question:'Which checks?',options:[{label:'Unit',description:'Fast tests'},{label:'Browser',description:'Browser interactions'}],multi_select:true,omitted_options:'0'},
    {header:'Branch',question:'Which branch?',options:[{label:'Current branch',description:'Keep sandbox custody'}],multi_select:false,omitted_options:'0'},
  ];
  assert.deepEqual(outputs[2].value.result.selected.last_display.questions,questions);
  for(const n of [4,5]) {
    const card=outputs[n].value.result.items[0];
    assert.equal(card.id,'question:00000000-0000-4000-8000-000000000050');
    assert.equal(card.identity_class,'publication');assert.deepEqual(card.questions,questions);
    assert.equal(card.details_state,'complete');assert.equal(card.omitted_questions,'0');assert.equal(card.omitted_source_observations,'0');assert.equal(card.disagreement,false);
  }
  assert.deepEqual(outputs[5].value.result.items[0].source_observations.map(s=>s.source),['question_publications','durable_question_fallback']);
  assert.ok(outputs[5].value.result.items[0].source_observations.every(s=>s.state==='complete'));
});
