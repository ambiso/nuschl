// SPDX-License-Identifier: EUPL-1.2
// Actual isolated WASM decoder class; independent libopus4046=1 expected PCM.
import assert from "node:assert/strict";
import {readFile,writeFile} from "node:fs/promises";
import {join,resolve} from "node:path";
import {renderStereo} from "./web_output_render.mjs";
import {pathToFileURL} from "node:url";
const [directory,fixture,output]=process.argv.slice(2),root=resolve(directory);
const {initSync,VoiceDecoder}=await import(pathToFileURL(join(root,"nuschl_audio_wasm.js")));
initSync({module:await readFile(join(root,"nuschl_audio_wasm_bg.wasm"))});
const source=JSON.parse(await readFile(fixture)),decoder=new VoiceDecoder(),actual=[];
try {for(const packet of source.packets)actual.push(...decoder.decode(Uint8Array.from(packet)));}
finally {decoder.free();}
assert.equal(actual.length,source.reference.length);assert.ok(actual.every(Number.isFinite));
let energy=0,error=0,max=0,monoEnergy=0,monoError=0;
for(let i=0;i<actual.length;i++){
 const wanted=source.reference[i],delta=actual[i]-wanted;energy+=wanted*wanted;error+=delta*delta;max=Math.max(max,Math.abs(delta));
 if(i%2===0){const m=(source.reference[i]+source.reference[i+1])/2,a=(actual[i]+actual[i+1])/2;monoEnergy+=m*m;monoError+=(a-m)**2;}
}
const streams=[{samples:Float32Array.from(actual)}];
const rendered=[renderStereo(streams),renderStereo(streams,{mono:true})];
const clipped=v=>Math.max(-1,Math.min(1,v));
function snr(expected,observed){let e=0,d=0;assert.equal(expected.length,observed.length);for(let i=0;i<expected.length;i++){e+=expected[i]**2;d+=(expected[i]-observed[i])**2;}return 10*Math.log10(e/Math.max(d,1e-30));}
const expectedMono=[];for(let i=0;i<source.reference.length;i+=2)expectedMono.push(clipped((source.reference[i]+source.reference[i+1])/2));
const worklet={stereo_fixed_gain_snr_db:snr(source.reference.map(clipped),rendered[0]),mono_fixed_gain_snr_db:snr(expectedMono,rendered[1])};
assert.ok(worklet.stereo_fixed_gain_snr_db>=35);assert.ok(worklet.mono_fixed_gain_snr_db>=35);
const result={actual_wasm:root,frames:actual.length/2,fixed_gain_snr_db:10*Math.log10(energy/Math.max(error,1e-30)),
 mono_fold_fixed_gain_snr_db:10*Math.log10(monoEnergy/Math.max(monoError,1e-30)),max_abs_error:max,actual_worklet:worklet};
await writeFile(output,JSON.stringify(result,null,2)+"\n");
assert.ok(result.fixed_gain_snr_db>=35);assert.ok(result.mono_fold_fixed_gain_snr_db>=35);console.log(JSON.stringify(result));
