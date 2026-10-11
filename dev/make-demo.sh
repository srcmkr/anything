#!/usr/bin/env bash
# Generates the demo images and videos in dev/demo/ that the fake index points
# to, so previews can be developed without touching anyone's real files.
# Deterministic: running it again produces the same files.
#
# Usage: dev/make-demo.sh [images] [videos]   (defaults: 300 images, 30 videos)
set -euo pipefail

cd "$(dirname "$0")"
out=demo
images=${1:-300}
videos=${2:-30}
font=/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf
jobs=$(nproc)

command -v ffmpeg >/dev/null || { echo "ffmpeg is required" >&2; exit 1; }
[ -f "$font" ] || { echo "missing font $font" >&2; exit 1; }

# Words that never occur in the fake index, so a search for them only shows
# demo files.
albums=(Strand Bergsee Leuchtturm Sonnenaufgang Waldweg Testbild)
sizes=(1600x1200 1200x1600 1920x1080 1080x1920 1400x1400 2400x1000)
sources=(gradients mandelbrot cellauto life sierpinski zoneplate colorspectrum testsrc2)

image() {
    local i=$1 album=${albums[$(( $1 % 6 ))]} size=${sizes[$(( ($1 / 6) % 6 ))]}
    local source=${sources[$(( $1 % 8 ))]} dir="$out/Bilder/${albums[$(( $1 % 6 ))]}"
    local name label input
    name=$(printf '%s %03d' "$album" "$i")
    label=${name//:/\\:}
    case $source in
        gradients)     input="gradients=s=$size:seed=$i:nb_colors=$(( 2 + i % 5 ))" ;;
        mandelbrot)    input="mandelbrot=s=$size:start_scale=$(( 1 + i % 3 )).5" ;;
        cellauto)      input="cellauto=s=$size:rule=$(( 18 + i % 100 )):random_seed=$i" ;;
        life)          input="life=s=$size:random_seed=$i:mold=10" ;;
        sierpinski)    input="sierpinski=s=$size:seed=$i" ;;
        zoneplate)     input="zoneplate=s=$size:kt2=$(( i % 7 ))" ;;
        colorspectrum) input="colorspectrum=s=$size" ;;
        *)             input="testsrc2=s=$size" ;;
    esac
    local ext=jpg; (( i % 4 == 3 )) && ext=png
    mkdir -p "$dir"
    ffmpeg -loglevel error -y -f lavfi -i "$input" -ss $(( i % 5 )) -frames:v 1 \
        -vf "drawtext=fontfile=$font:text='$label':fontsize=h/12:fontcolor=white:box=1:boxcolor=black@0.55:boxborderw=20:x=(w-tw)/2:y=h-th-h/10" \
        "$dir/$name.$ext"
}

video() {
    local i=$1 source=${sources[$(( ($1 + 3) % 8 ))]} name
    name=$(printf 'Testvideo %02d' "$i")
    mkdir -p "$out/Videos"
    ffmpeg -loglevel error -y -f lavfi -i "$source=s=960x540:r=25" -t 4 \
        -vf "drawtext=fontfile=$font:text='$name':fontsize=48:fontcolor=white:box=1:boxcolor=black@0.55:boxborderw=16:x=(w-tw)/2:y=h-th-60" \
        -c:v libx264 -pix_fmt yuv420p -preset veryfast "$out/Videos/$name.mp4"
}

export -f image video
export out font
export albums_list="${albums[*]}" sizes_list="${sizes[*]}" sources_list="${sources[*]}"

# Arrays do not survive export; rebuild them in the child shells.
run() {
    read -ra albums <<<"$albums_list"; read -ra sizes <<<"$sizes_list"; read -ra sources <<<"$sources_list"
    "$@"
}
export -f run

rm -rf "$out"
seq 0 $(( images - 1 )) | xargs -P "$jobs" -I{} bash -c 'run image {}'
seq 1 "$videos" | xargs -P "$jobs" -I{} bash -c 'run video {}'
echo "$(find "$out" -type f | wc -l) files in dev/$out ($(du -sh "$out" | cut -f1))"
