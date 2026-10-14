#!/usr/bin/env bash
# Unattended FireWire (DV) tape capture loop for Digital8 / DV camcorders.
#
# Insert a tape and the script rewinds it, captures it to a raw .dv file at
# 1x speed, stops at end of tape, then waits for the next tape. All transport
# control goes over AV/C, so the only manual steps are inserting and ejecting.
#
# Camera setup: AC power, power switch on PLAY/EDIT (VCR), i.LINK cable in.
# Requires: dvgrab, libavc1394-tools (dvcont).
#
# Usage: dv-tape-capture.sh [-o outdir] [-m max_duration] [-p prefix] [-1]
#   -o  output directory                      [default ./tapes]
#   -m  safety cap per tape, dvgrab SMIL time  [default 250min, LP 120 + margin]
#   -p  file name prefix                      [default tape]
#   -1  capture a single tape, then exit
#
# Files: <outdir>/<prefix>NNN-YYYYmmdd-HHMMSS-001.dv plus a matching .log.
# Analog Hi8/Video8 tapes carry no timecode or recording date, so each tape
# is kept as one continuous file; split it into scenes afterwards.

set -u

OUT=./tapes MAX=250min PREFIX=tape ONCE=0
POLL=3                 # seconds between AV/C status polls
REWIND_TIMEOUT=900     # seconds; a full 120-min Hi8 rewind is a few minutes
PLAY_TIMEOUT=60        # seconds to wait for playback to start
STALL_WARN=30          # seconds without file growth before warning
GB_PER_HOUR=13         # DV is ~3.6 MB/s

while getopts "o:m:p:1h" opt; do
    case $opt in
        o) OUT=$OPTARG ;;
        m) MAX=$OPTARG ;;
        p) PREFIX=$OPTARG ;;
        1) ONCE=1 ;;
        *) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    esac
done

for tool in dvgrab dvcont; do
    command -v "$tool" >/dev/null || { echo "missing $tool (apt install dvgrab libavc1394-tools)" >&2; exit 1; }
done
mkdir -p "$OUT" || exit 1

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*"; }

# Transport state as decoded by libavc1394, e.g. "Loading Medium" (no tape),
# "Winding stopped", "Winding reverse", "Playing". Empty when unreachable.
status() {
    local s
    s=$(timeout 10 dvcont status 2>/dev/null | tail -n1)
    case $s in
        "Loading Medium"|Winding*|Playing*|Recording*) echo "$s" ;;
        *) echo "" ;;
    esac
}

# Highest NNN already used in OUT, so numbering survives restarts.
next_number() {
    local max=0 f n
    for f in "$OUT/$PREFIX"[0-9][0-9][0-9]-*.dv; do
        [[ -e $f ]] || continue
        n=${f##*/$PREFIX}; n=$((10#${n%%-*}))
        (( n > max )) && max=$n
    done
    printf '%03d' $((max + 1))
}

DVGRAB_PID=
cleanup() {
    echo
    if [[ -n $DVGRAB_PID ]] && kill -0 "$DVGRAB_PID" 2>/dev/null; then
        log "interrupted, closing capture file"
        kill -INT "$DVGRAB_PID"; wait "$DVGRAB_PID"
    fi
    dvcont stop >/dev/null 2>&1
    exit 130
}
trap cleanup INT TERM

wait_for_tape() {
    local s last=
    while :; do
        s=$(status)
        if [[ -z $s ]]; then
            [[ $last != offline ]] && log "camera not reachable; check power, VCR mode and cable"
            last=offline
        elif [[ $s == "Loading Medium" ]]; then
            [[ $last != empty ]] && log "waiting for a tape..."
            last=empty
        else
            log "tape detected ($s)"
            return
        fi
        sleep "$POLL"
    done
}

wait_for_eject() {
    log "done; eject the tape to continue (Ctrl-C to quit)"
    while [[ $(status) != "Loading Medium" ]]; do sleep "$POLL"; done
    log "tape removed"
}

rewind() {
    log "rewinding"
    dvcont stop >/dev/null 2>&1; sleep 1
    dvcont rewind >/dev/null 2>&1; sleep "$POLL"
    local s t=0
    while (( t < REWIND_TIMEOUT )); do
        s=$(status)
        [[ $s == "Winding stopped" ]] && { log "at start of tape"; return 0; }
        [[ -z $s || $s == "Loading Medium" ]] && { log "lost the tape while rewinding"; return 1; }
        sleep "$POLL"; t=$((t + POLL))
    done
    log "rewind did not finish within ${REWIND_TIMEOUT}s"
    return 1
}

check_space() {
    local free_gb
    free_gb=$(df -BG --output=avail "$OUT" | tail -n1 | tr -dc 0-9)
    if (( free_gb < 4 * GB_PER_HOUR )); then
        log "WARNING: only ${free_gb}G free in $OUT (~${GB_PER_HOUR}G per hour of tape)"
    fi
}

capture() {
    local num base file logf start s seen_play=0 idle=0 waited=0 size last_size=0 stall=0
    num=$(next_number)
    base="$OUT/$PREFIX$num-$(date +%Y%m%d-%H%M%S)-"
    file="${base}001.dv"; logf="${base%-}.log"
    check_space
    log "capturing to $file (cap $MAX)"
    start=$(date +%s)

    # dvgrab sends AV/C play on start and stop on exit.
    dvgrab -format raw -size 0 -duration "$MAX" "$base" >"$logf" 2>&1 &
    DVGRAB_PID=$!

    while kill -0 "$DVGRAB_PID" 2>/dev/null; do
        sleep "$POLL"
        s=$(status)
        if [[ $s == Playing* ]]; then
            seen_play=1 idle=0
        elif (( seen_play )); then
            # Two non-playing polls in a row: end of tape, or tape ejected.
            (( ++idle >= 2 )) && { log "playback stopped ($s)"; break; }
        elif (( (waited += POLL) >= PLAY_TIMEOUT )); then
            log "playback never started ($s); is the camera in VCR mode?"
            break
        fi

        size=$(stat -c %s "$file" 2>/dev/null || echo 0)
        if (( size > last_size )); then
            stall=0
        elif (( seen_play && (stall += POLL) == STALL_WARN )); then
            log "WARNING: playing but no data for ${STALL_WARN}s"
        fi
        last_size=$size
        printf '\r  %s  %5d MiB  %s   ' "$(date -u -d @$(( $(date +%s) - start )) +%H:%M:%S)" \
            $((size >> 20)) "$s"
    done
    echo

    if kill -0 "$DVGRAB_PID" 2>/dev/null; then
        kill -INT "$DVGRAB_PID"
    fi
    wait "$DVGRAB_PID"
    DVGRAB_PID=
    dvcont stop >/dev/null 2>&1

    size=$(stat -c %s "$file" 2>/dev/null || echo 0)
    if (( size == 0 )); then
        log "nothing captured; see $logf"
        rm -f "$file"
        return 1
    fi
    log "captured $((size >> 20)) MiB in $(date -u -d @$(( $(date +%s) - start )) +%H:%M:%S)"
}

while :; do
    wait_for_tape
    if rewind; then
        capture
    fi
    (( ONCE )) && exit 0
    wait_for_eject
done
