// SPDX-License-Identifier: EUPL-1.2
// Actual worklet/optional WASM; no browser device/network/credentials.
import assert from "node:assert/strict";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {pathToFileURL} from "node:url";
import {renderStereo} from "./web_output_render.mjs";

const arguments_=process.argv.slice(2),requireParity=arguments_.includes("--require-parity");
const option=name=>{const at=arguments_.indexOf(name);return at<0?null:arguments_[at+1];};
function render(samples,volume=1,mono=true) {
  return renderStereo([{samples:Float32Array.from({length:samples.length*2},(_,index)=>samples[index>>1]),volume}],{mono});
}
const unity=Float32Array.from({length:960},(_,i)=>Math.sin(2*Math.PI*997*i/48000)*.2);
assert.deepEqual(render(unity),unity,"safe unity PCM must not change");
const over=new Float32Array(960).fill(2),attenuated=render(over,.25);
const gainBeforeClipping=Math.max(...attenuated)===.5;
const stereo=render(unity,1,false);
const duplicatedMono=stereo.every((value,index)=>value===unity[index>>1]);
const distinct=Float32Array.from({length:1920},(_,i)=>i%2===0?.2:-.3);
const stereoPreserved=renderStereo([{samples:distinct}]).every((value,index)=>value===distinct[index]);
const result={worklet_unity_exact:true,gain_before_clipping:gainBeforeClipping,
  diagnostic_overshoot_attenuated:{input:2,volume:.25,expected:.5,observed:attenuated[0]},
  actual_processor_duplicates_mono:duplicatedMono,actual_processor_stereo_preserved:stereoPreserved,codec:null,
  limits:["Node executes actual worklet code; no hardware/browser resampler proof.","No perceptual quality equivalence claim."]};
if(option("--codec")&&option("--fixture")){
  const directory=resolve(option("--codec")),fixture=JSON.parse(await readFile(option("--fixture"),"utf8"));
  const {initSync,VoiceDecoder}=await import(pathToFileURL(join(directory,"nuschl_audio_wasm.js")).href);
  initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
  const decoder=new VoiceDecoder(),actual=[];
  try{for(const packet of fixture.packets)actual.push(...decoder.decode(Uint8Array.from(packet)));}
  finally{decoder.free();}
  const expected=fixture.reference_stereo,mono=fixture.reference_mono;
  assert.ok(actual.every(Number.isFinite));
  const channels=actual.length/(expected.length/2);
  let energy=0,error=0;
  const comparison=channels===2?expected:mono;
  assert.equal(actual.length,comparison.length);
  for(let i=0;i<comparison.length;i++){energy+=comparison[i]**2;error+=(comparison[i]-actual[i])**2;}
  result.codec={actual_codec:directory,source_channels:fixture.channels,decoded_channels:channels,
    samples:actual.length,reference_stereo_samples:expected.length,
    reference_channels:channels,agrees_with_reference_fixed_gain_db:10*Math.log10(Math.max(energy,1e-30)/Math.max(error,1e-30)),
    stereo_preserved:channels===2};
}
result.parity_gate_passed=gainBeforeClipping&&stereoPreserved&&result.codec?.stereo_preserved===true&&result.codec.agrees_with_reference_fixed_gain_db>=35;
if(option("--output"))await writeFile(option("--output"),JSON.stringify(result,null,2)+"\n");
console.log(JSON.stringify(result));
if(requireParity&&!result.parity_gate_passed)process.exitCode=1;
