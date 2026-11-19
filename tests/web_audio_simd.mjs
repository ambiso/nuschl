// SPDX-License-Identifier: EUPL-1.2
// The audio worker's simd128 build (webClient.audioWasm) against its scalar build: both
// decode the fixture's stereo music packets and encode and decode a mono synthetic voice
// at every bitrate class to the same bits.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [scalarDirectory,simdDirectory,music]=process.argv.slice(2);
assert.ok(scalarDirectory&&simdDirectory&&music,"usage: node tests/web_audio_simd.mjs SCALAR_DIRECTORY SIMD_DIRECTORY MUSIC_OPUS");
const load=async directory=>{
  const codec=await import(pathToFileURL(join(directory,"nuschl_audio_wasm.js")).href);
  codec.initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
  return codec;
};
const builds=[await load(scalarDirectory),await load(simdDirectory)];
// The Ogg Opus stream's audio packets (RFC 7845), after OpusHead and OpusTags.
const oggPackets=bytes=>{
  const packets=[];let packet=[];
  for(let page=0;page<bytes.length;) {
    assert.equal(Buffer.from(bytes.subarray(page,page+4)).toString("latin1"),"OggS");
    const segments=bytes[page+26],table=bytes.subarray(page+27,page+27+segments);
    let offset=page+27+segments;
    for(const size of table) {
      packet.push(bytes.subarray(offset,offset+size));offset+=size;
      if(size<255){packets.push(Buffer.concat(packet));packet=[];}
    }
    page=offset;
  }
  return packets.slice(2);
};
const same=(a,b,what)=>assert.ok(Buffer.from(a.buffer,a.byteOffset,a.byteLength).equals(Buffer.from(b.buffer,b.byteOffset,b.byteLength)),what);
const decoders=builds.map(codec=>new codec.VoiceDecoder());
const packets=oggPackets(await readFile(music));
assert.ok(packets.length>=50&&packets.every(packet=>(packet[0]&4)!==0),"stereo music packets");
packets.forEach((packet,index)=>{
  const [scalar,simd]=decoders.map(decoder=>decoder.decode(packet));
  same(scalar,simd,`music packet ${index}`);
});
// Integers in [-32768, 32767] scaled by 2^-15 are exact f32 values in both builds.
let seed=0x6e757363;
const noise=()=>{seed^=seed<<13;seed^=seed>>>17;seed^=seed<<5;return (seed>>16)%2048;};
const voice=frame=>Float32Array.from({length:960},(_,i)=>{
  const t=frame*960+i,pitch=t%240,syllable=(t%12000)<7000?1:0;
  return (syllable*(pitch<120?pitch*80:(240-pitch)*80)-4800+noise())/32768;
});
let encoded=0;
for(const bitrate of [6000,16000,24000,40000,64000,128000,256000]) {
  const encoders=builds.map(codec=>{const encoder=new codec.VoiceEncoder();encoder.set_bitrate(bitrate);return encoder;});
  for(let frame=0;frame<50;frame++) {
    const pcm=voice(frame);
    const [scalar,simd]=encoders.map(encoder=>encoder.encode(pcm));
    same(scalar,simd,`${bitrate} bit/s frame ${frame}`);
    const [left,right]=decoders.map(decoder=>decoder.decode(scalar));
    same(left,right,`${bitrate} bit/s frame ${frame} decoded`);
    encoded++;
  }
  encoders.forEach(encoder=>encoder.free());
}
decoders.forEach(decoder=>decoder.free());
console.log(JSON.stringify({passed:true,music_packets:packets.length,voice_frames:encoded}));
