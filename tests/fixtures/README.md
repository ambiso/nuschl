# WebRTC video fixture

`rtc-keyframe.vp8` is a single 160x120 VP8 keyframe, generated locally from FFmpeg's
`testsrc2` pattern. It contains no camera capture or third-party video. It is part of
Nuschl's EUPL-1.2 test material.

Generation:

```sh
ffmpeg -f lavfi -i testsrc2=size=160x120:rate=1 -frames:v 1 \
  -c:v libvpx -b:v 800k -threads 2 -f ivf fixture.ivf
```

Extract the first frame after the 32-byte IVF header and 12-byte frame header. The
transport tests compare the entire reassembled payload. FFmpeg can decode the original
IVF to validate the fixture; neither the application nor the Nix build needs FFmpeg.
This establishes encoded-frame transport, not live capture, rendering or A/V sync.

# Music bot VM certificate

`musicbot-murmur.crt` and `musicbot-murmur.key` are a self-signed certificate and its
key for the stock Mumble server in `nix/vm-musicbot.nix`. They exist only so that the
test can pin the server (SHA-256 of the DER certificate:
`d216518fdf25aa0b1c57881888c4304fb2bbbd4bbd763f267ee75b3d9445d1d8`) at evaluation
time. The key is public test material: never use it anywhere else.

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 36500 \
  -subj "/CN=nuschl-musicbot-test-murmur" \
  -keyout musicbot-murmur.key -out musicbot-murmur.crt
```

# Opus music fixture

`music-290k.opus` is one second of synthetic stereo music (sine chords and pink
noise) in Ogg Opus: 51 CELT fullband stereo packets of 725 bytes, 20 ms each, at
290 kbit/s CBR. `tests/web_audio_simd.mjs` decodes it with the browser audio
worker's SIMD and scalar builds. It is part of Nuschl's EUPL-1.2 test material.

```sh
ffmpeg -f lavfi -i "aevalsrc=0.2*sin(2*PI*220*t)+0.15*sin(2*PI*277.2*t)+0.12*sin(2*PI*329.6*t)+0.05*sin(2*PI*3520*t)*sin(2*PI*3*t)|0.2*sin(2*PI*220.5*t)+0.15*sin(2*PI*329.6*t)+0.1*sin(2*PI*440*t)+0.05*sin(2*PI*7040*t):s=48000:d=1" \
  -f lavfi -i "anoisesrc=c=pink:a=0.05:r=48000:d=1:s=1,pan=stereo|c0=c0|c1=c0" \
  -filter_complex "[0][1]amix=inputs=2:normalize=0" -map_metadata -1 \
  -fflags +bitexact -flags:a +bitexact -c:a libopus -b:a 290k -vbr off \
  -frame_duration 20 -application audio music-290k.opus
```
