#!/usr/bin/env bash
# Generate N uniform test channels: 1280x720 H.264 High, 30fps, GOP 15, 4 slices, AAC.
N=${1:-60}; DUR=${2:-60}; PAR=${3:-8}
FF="${FFMPEG:-ffmpeg}"   # or set FFMPEG=/path/to/ffmpeg.exe
FONT="fontfile='C\:/Windows/Fonts/arialbd.ttf'"
mkdir -p content; rm -f content/probe.*
gen() {
  i=$1
  "$FF" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc2=size=1280x720:rate=30" \
    -f lavfi -i "sine=frequency=$((200+i*13)):sample_rate=48000" -t $DUR \
    -vf "hue=h=$((i*23 % 360)),drawtext=$FONT:text='CH $i':fontsize=220:fontcolor=white:borderw=10:x=(w-text_w)/2:y=(h-text_h)/2,drawtext=$FONT:text='%{pts\:hms}':fontsize=56:fontcolor=yellow:borderw=4:x=40:y=h-110" \
    -c:v libx264 -preset veryfast -profile:v high -pix_fmt yuv420p -g 15 -keyint_min 15 -sc_threshold 0 -bf 2 -slices 4 -b:v 4M \
    -c:a aac -b:a 128k -movflags +faststart "content/ch$(printf %03d $i).mp4"
}
export -f gen; export FF DUR FONT
seq 1 $N | xargs -P $PAR -I{} bash -c 'gen {}'
echo "GEN DONE: $(ls content/*.mp4 | wc -l) files"
