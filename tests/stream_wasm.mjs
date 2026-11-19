// SPDX-License-Identifier: EUPL-1.2
// Replays the native transcript (crates/stream-wasm/test-vectors/transcript.txt) through the
// wasm-bindgen build of the browser stream worker: every reply, frame and error must equal
// the native one byte for byte. Worker B's lines take worker A's native key messages.
// crypto.getRandomValues is the transcript's xorshift32, one stream per worker.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [directory,transcript]=process.argv.slice(2);
assert.ok(directory&&transcript,"usage: node tests/stream_wasm.mjs WASM_DIRECTORY TRANSCRIPT");
const {initSync,StreamWorker}=await import(pathToFileURL(join(directory,"nuschl_stream_wasm.js")).href);
initSync({module:await readFile(join(directory,"nuschl_stream_wasm_bg.wasm"))});
const state={A:1,B:2};
let current=null,drawn=0;
Object.defineProperty(globalThis,"crypto",{value:{getRandomValues(bytes) {
  let x=state[current];
  for(let i=0;i<bytes.length;i++){x=(x^(x<<13))>>>0;x=(x^(x>>>17))>>>0;x=(x^(x<<5))>>>0;bytes[i]=x&255;}
  state[current]=x;drawn+=bytes.length;return bytes;
}}});
const bytes=hex=>Uint8Array.from(hex.match(/../g)??[],pair=>parseInt(pair,16));
const hex=array=>Array.from(array,byte=>byte.toString(16).padStart(2,"0")).join("");
const workers={A:new StreamWorker(),B:new StreamWorker()};
const lines=(await readFile(transcript,"utf8")).split("\n");
assert.equal(lines.pop(),"");
let checked=0;
for(let i=0;i<lines.length;i+=2) {
  const [name,now,operation,...rest]=lines[i].split(" ");
  const worker=workers[name];current=name;
  let actual;
  try {
    if(operation==="handle")actual=worker.handle(lines[i].slice(name.length+now.length+operation.length+3),Number(now));
    else if(operation==="encrypt")actual=hex(worker.encrypt(Number(rest[0]),rest[1]==="key",bytes(rest[2])));
    else if(operation==="decrypt")actual=hex(worker.decrypt(Number(rest[0]),bytes(rest[1]),Number(now)));
    else assert.fail(`unknown operation on line ${i+1}`);
    actual="= "+actual;
  } catch(error) {
    if(typeof error!=="string")throw error;
    actual="! "+error;
  }
  assert.equal(actual,lines[i+1],`line ${i+2}`);checked++;
}
assert.ok(drawn>0,"the worker draws its randomness from crypto.getRandomValues");
for(const worker of Object.values(workers))worker.free();
console.log(JSON.stringify({passed:true,calls:checked,random_bytes:drawn}));
