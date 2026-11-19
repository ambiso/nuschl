The AV1 packet is one synthetic `testsrc2` picture. This command generates it.

```sh
ffmpeg -f lavfi -i testsrc2=size=192x128:rate=1 -frames:v 1 \
  -c:v libaom-av1 -cpu-used 8 -crf 16 -f obu screen.av1
```
