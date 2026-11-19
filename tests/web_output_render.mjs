// SPDX-License-Identifier: EUPL-1.2
// The production browser mixer and playback ring with a fixed test epoch.
import assert from "node:assert/strict";
import {BlockRing,RingPlayout,mixSpeaker,PLAYOUT_FRAME,PLAYOUT_BLOCKS} from "../web/app/src/audio-worklet.js";

const WIDTH=PLAYOUT_FRAME*2;
export const RING_PCM_BYTES=PLAYOUT_BLOCKS*WIDTH*Float64Array.BYTES_PER_ELEMENT;

export function renderStereo(streams,{mono=false}={}) {
  const length=streams[0].samples.length/2;
  const buffer=new SharedArrayBuffer(BlockRing.bytes(PLAYOUT_BLOCKS,WIDTH,Float64Array));
  const policy=new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT));
  Atomics.store(policy,0,1);
  const producer=new BlockRing(buffer,PLAYOUT_BLOCKS,WIDTH,Float64Array);
  const playback=new RingPlayout(new BlockRing(buffer,PLAYOUT_BLOCKS,WIDTH,Float64Array),policy);
  const left=new Float32Array(PLAYOUT_FRAME),right=mono?null:new Float32Array(PLAYOUT_FRAME);
  const mix=new Float64Array(WIDTH),block=new Float32Array(WIDTH);
  const rendered=new Float32Array(length*(mono?1:2));
  for(let frame=0;frame<length;frame+=PLAYOUT_FRAME) {
    mix.fill(0);
    for(const {samples,volume=1} of streams) {
      assert.equal(samples.length,length*2);
      block.fill(0);
      block.set(samples.subarray(frame*2,frame*2+WIDTH));
      mixSpeaker(mix,block,volume);
    }
    assert.ok(producer.push(1,0,mix));
    playback.render(left,right);
    for(let i=0;i<Math.min(PLAYOUT_FRAME,length-frame);i++) {
      if(mono)rendered[frame+i]=left[i];
      else {
        rendered[(frame+i)*2]=left[i];
        rendered[(frame+i)*2+1]=right[i];
      }
    }
  }
  return rendered;
}
