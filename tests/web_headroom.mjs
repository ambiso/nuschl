// SPDX-License-Identifier: EUPL-1.2
// Real processor, reference-decoded independent streams, no browser devices.
import assert from "node:assert/strict";
import {readFile,writeFile} from "node:fs/promises";
import {renderStereo,RING_PCM_BYTES} from "./web_output_render.mjs";
const [fixturePath,expectedPath,outputPath]=process.argv.slice(2);
const f32=async path=>{const raw=await readFile(path);return Float32Array.from(new Float32Array(raw.buffer,raw.byteOffset,raw.byteLength/4));};
const streams=await Promise.all(JSON.parse(await readFile(fixturePath)).map(async stream=>({...stream,samples:await f32(stream.pcm)})));
const expected=await f32(expectedPath),actual=renderStereo(streams);
assert.equal(actual.length,expected.length);
let maxError=0;
for(let i=0;i<expected.length;i++)maxError=Math.max(maxError,Math.abs(actual[i]-expected[i]));
assert.equal(maxError,0,"actual processor must equal the independent final-clipped f64 sum");
const proof={actual_processor:true,independent_two_stream_sum_exact:true,frames:actual.length/2,max_abs_error:maxError,
  ring_pcm_bytes:RING_PCM_BYTES,render_allocates_pcm_scratch:false};
await writeFile(outputPath,JSON.stringify(proof,null,2)+"\n");
console.log(JSON.stringify(proof));
