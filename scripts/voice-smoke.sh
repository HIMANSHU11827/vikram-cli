#!/usr/bin/env bash
# voice-smoke.sh — end-to-end check that the built-in wavo voice path works on this machine.
#
# What it does
#   1. Generates a spoken phrase with espeak-ng (or uses the wav you pass as $1).
#   2. Runs `vikram voice listen --enable --max-record-ms 12000` with a PATH that does not
#      contain `voxtype`, so the built-in wavo engine is exercised instead of the fast path.
#   3. Plays the wav into the default sink while that recording is open.
#   4. Asserts the transcript is non-empty, is not a known Whisper hallucination, is not a
#      repetition loop, has at least three words, and — when the script generated the phrase —
#      shares a word with it. Prints PASS or FAIL and exits non-zero on failure.
#
# What a PASS means: the microphone, the VAD gate, the wavo model and the transcript path all
# worked. It is not an accuracy score — on speakers and a built-in mic a partial transcript
# ("Hello, big one!") still passes, because that is what the hardware gave.
#
# Assumptions (all of them matter — the script does not fake any of them)
#   * A default output sink that is audible to the default input source. On a laptop this is
#     speakers → built-in mic; on a desktop it needs a real microphone near the speakers or a
#     PulseAudio/PipeWire loopback (`pactl load-module module-loopback`). The script prints the
#     default source and sink first, so a loopback mistake is visible before the verdict.
#   * `espeak-ng` when no wav is given; `paplay` (or `pw-play`/`aplay`) to play it.
#   * The wavo model is already downloaded (`vikram voice status` shows a path), or the network
#     is available for the one-time ~80 MB hub download on the first utterance.
#   * The operator's saved voice.json is used as-is: a wrong `input_device` in it fails this
#     check, which is the honest outcome.
#   * A human is at the machine; audio plays out loud for a few seconds.
#
# Usage
#   scripts/voice-smoke.sh                 # espeak-ng phrase, then record + transcribe
#   scripts/voice-smoke.sh /path/clip.wav  # use your own recording instead
#
# Exit codes: 0 PASS · 1 FAIL · 2 missing prerequisite.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

pick_bin() {
    for candidate in "$ROOT/target/debug/vikram" "$ROOT/target/release/vikram"; do
        if [[ -x $candidate ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

if ! BIN=$(pick_bin); then
    echo "FAIL: no vikram binary at $ROOT/target/{debug,release}/vikram — run: cargo build" >&2
    exit 2
fi
BIN=$(realpath -- "$BIN")

PLAYER=""
PLAYER_TOOL=""
for candidate in paplay pw-play aplay; do
    if path=$(command -v "$candidate" 2>/dev/null); then
        PLAYER_TOOL=$candidate
        PLAYER=$path
        break
    fi
done
if [[ -z $PLAYER ]]; then
    echo "FAIL: need paplay, pw-play or aplay to play the test wav" >&2
    exit 2
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/vikram-voice-smoke.XXXXXX")
REC_PID=""
PLAY_PID=""
cleanup() {
    for pid in "$PLAY_PID" "$REC_PID"; do
        if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    rm -rf -- "$WORK"
}
trap cleanup EXIT INT TERM

# A PATH without voxtype. `which("voxtype")` is how the crate decides to use the fast path, so a
# PATH that cannot see it is what forces the built-in engine. Symlink only what is needed.
mkdir -p "$WORK/bin"
for tool in paplay pw-play aplay parecord arecord pactl espeak-ng; do
    if path=$(command -v "$tool" 2>/dev/null); then
        ln -sf "$path" "$WORK/bin/$tool"
    fi
done

WAV=${1:-}
GENERATED=0
if [[ -n $WAV ]]; then
    if [[ ! -f $WAV ]]; then
        echo "FAIL: $WAV is not a file" >&2
        exit 2
    fi
    WAV=$(realpath -- "$WAV")
else
    if ! ESPEAK=$(command -v espeak-ng 2>/dev/null); then
        echo "FAIL: espeak-ng is missing and no wav path was given" >&2
        exit 2
    fi
    GENERATED=1
    WAV="$WORK/phrase.wav"
    # -a 200 is espeak-ng's maximum amplitude. At the default level the phrase reaches the mic
    # too quietly on a laptop and the recognizer answers with noise.
    "$ESPEAK" -w "$WAV" -s 145 -a 200 "hello vikram, this is a voice test"
fi

echo "binary:   $BIN"
echo "player:   $PLAYER_TOOL ($PLAYER)"
echo "wav:      $WAV"
if command -v pactl >/dev/null 2>&1; then
    echo "sink:     $(pactl get-default-sink 2>/dev/null || echo unknown)"
    echo "source:   $(pactl get-default-source 2>/dev/null || echo unknown)"
    echo "          (the source must be able to hear the sink — mic near speakers, or a loopback)"
fi
echo
echo "equivalent manual run:"
echo "  PATH=$WORK/bin $BIN voice listen --enable --max-record-ms 12000"
echo "  $PLAYER $WAV"
echo

echo "recording..."
PATH="$WORK/bin" "$BIN" voice listen --enable --max-record-ms 12000 \
    >"$WORK/out.txt" 2>"$WORK/err.txt" &
REC_PID=$!

# Let the capture stream open (tens of ms) before the phrase starts.
sleep 1.5
# Twice, with a gap. A sink that was suspended resumes on the first play and can eat it, and one
# short phrase is not enough signal for a stable verdict.
echo "playing the wav (twice)..."
for _ in 1 2; do
    "$PLAYER" "$WAV" >/dev/null 2>&1 &
    PLAY_PID=$!
    wait "$PLAY_PID" 2>/dev/null || true
    PLAY_PID=""
    sleep 1
done

# Recording cap plus model load plus inference. The model loads per process, so this is generous
# on purpose; a hang past it is a real failure, not slowness.
deadline=$((SECONDS + 90))
while kill -0 "$REC_PID" 2>/dev/null; do
    if ((SECONDS >= deadline)); then
        echo "FAIL: voice listen did not finish within 90s" >&2
        cat "$WORK/err.txt" >&2
        exit 1
    fi
    sleep 0.5
done
set +e
wait "$REC_PID"
status=$?
set -e
REC_PID=""
if ((status != 0)); then
    echo "FAIL: voice listen exited $status" >&2
    cat "$WORK/err.txt" >&2
    exit 1
fi

TRANSCRIPT=$(tr -d '\r' <"$WORK/out.txt" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' | tr '\n' ' ')
TRANSCRIPT=$(printf '%s' "$TRANSCRIPT" | sed -e 's/[[:space:]]\+/ /g' -e 's/ *$//')

echo
echo "transcript: ${TRANSCRIPT:-<empty>}"
if [[ -s $WORK/err.txt ]]; then
    echo "notes:"
    sed -e 's/^/  /' "$WORK/err.txt"
fi

# voxtype prints its own progress to stdout; seeing it means the shim PATH did not take effect
# and the built-in engine was never exercised.
if grep -qiE 'VAD backend|Processing [0-9]+ samples|Loading audio file' <<<"$TRANSCRIPT"; then
    echo "FAIL: voxtype ran anyway — the built-in wavo engine was not exercised" >&2
    exit 1
fi

if [[ -z $TRANSCRIPT ]]; then
    echo "FAIL: empty transcript" >&2
    exit 1
fi
if [[ ${TRANSCRIPT,,} == "no speech detected" ]]; then
    echo "FAIL: the VAD gate never opened — check the mic, the sink volume, or floor_ratio" >&2
    exit 1
fi

# What Whisper-family models emit on silence or noise. An exact match means nothing was heard.
normalized=$(printf '%s' "$TRANSCRIPT" | tr '[:upper:]' '[:lower:]' | tr -d '[:punct:]' | tr -s ' ')
case "$normalized" in
    "thank you"|"thank you for watching"|"thanks for watching"|"thank you very much"|\
    "you"|"bye"|"goodbye"|"please subscribe"|"subscribe"|"the end"|"silence"|\
    "blank_audio"|"okay"|"ok"|"yeah"|"yes"|"no"|"hmm"|"uh"|"um"|"sorry"|"i am sorry"|\
    "so"|"and"|"right"|"the"|"a")
        echo "FAIL: \"$TRANSCRIPT\" is a known hallucination, not the spoken phrase" >&2
        exit 1
        ;;
esac

# "Hahahaha!" is non-empty, is not on the hallucination list, and proves nothing: a real pass has
# to show words.
if (( $(wc -w <<<"$TRANSCRIPT") < 3 )); then
    echo "FAIL: only one or two words came back — the phrase was not heard" >&2
    exit 1
fi
if ((GENERATED)) && ! grep -qiE 'hello|vikram|voice|test' <<<"$TRANSCRIPT"; then
    echo "FAIL: \"$TRANSCRIPT\" shares no word with the spoken phrase" >&2
    exit 1
fi

# A repetition loop ("No! No! No! ..." over a hundred times) is the other shape of a Whisper
# hallucination: it is non-empty and every token is a real word, so it needs its own check.
read -r tokens unique maxrep < <(
    printf '%s' "$normalized" | awk '{
        for (i = 1; i <= NF; i++) { count[$i]++; tokens++ }
        for (word in count) { unique++; if (count[word] > maxrep) maxrep = count[word] }
        printf "%d %d %d\n", tokens, unique, maxrep
    }'
)
if ((tokens > 8 && unique * 100 / tokens < 40)); then
    echo "FAIL: ${tokens} words, only ${unique} distinct — a repetition loop, not speech" >&2
    exit 1
fi
if ((maxrep > 6)); then
    echo "FAIL: one word repeated ${maxrep} times — a repetition loop, not speech" >&2
    exit 1
fi

echo
echo "PASS: built-in wavo transcribed the phrase (${tokens} words, ${unique} distinct)"
echo "      path works; this is not an accuracy score (see the header)"
