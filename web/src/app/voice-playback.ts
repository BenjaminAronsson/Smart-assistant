import type { VoiceErrorCodeDto } from '../generated/api-types';

/**
 * Gapless playback of the daemon's synthesized PCM (F5.2).
 *
 * Chunks arrive as raw little-endian s16 frames bracketed by
 * `voice.speak.start` / `voice.speak.stop`. Each chunk is scheduled on the
 * Web Audio clock immediately after the previous one, so a slow synthesizer
 * produces a pause rather than a click or an overlap.
 *
 * `stop()` is the browser half of barge-in: the daemon cancels synthesis, and
 * this stops what has already been buffered. Both halves are needed — audio
 * already delivered would otherwise keep playing over the user's next sentence.
 */
export class VoicePlayback {
  private context: AudioContext | null = null;
  private readonly sources = new Set<AudioBufferSourceNode>();
  private nextStartTime = 0;
  private format: { sampleRateHz: number; channels: number } | null = null;

  /** Whether audio is currently scheduled or playing. */
  get active(): boolean {
    return this.context !== null;
  }

  begin(sampleRateHz: number, channels: number): void {
    this.stop();
    this.format = { sampleRateHz, channels };
    // A dedicated context at the synthesizer's own rate: no resampling, and
    // closing it on barge-in releases the audio device promptly.
    const context = new AudioContext({ sampleRate: sampleRateHz });
    this.context = context;
    this.nextStartTime = context.currentTime;

    // Resume if the browser handed us a suspended context.
    //
    // This is not belt-and-braces, it is the difference between hearing the
    // answer and not. Under Chrome's autoplay policy a context created without
    // user activation starts `suspended`, and a suspended context accepts
    // `start()` calls and plays nothing — no error, no warning, and
    // `currentTime` frozen at zero. It fails exactly like working code.
    //
    // This context is *always* created at that disadvantage: `begin()` runs
    // when `voice.speak.start` arrives, which is after the transcript, the
    // model and the first synthesized clause — long after the push-to-talk
    // gesture that started it all ended. The capture context (voice-capture
    // .service.ts) is created during the gesture and still resumes defensively;
    // this one needed it more and did not do it, which is how a daemon that was
    // provably synthesizing audio produced a silent browser.
    if (context.state === 'suspended') {
      // Fire and forget: `begin()` is called from a synchronous socket handler,
      // and a rejected resume (no activation at all yet) must not throw into it.
      // Chunks scheduled meanwhile are queued on the context and play on resume.
      void context.resume().catch(() => undefined);
    }
  }

  /** Schedule one PCM chunk. Ignored when no utterance is open. */
  push(pcm: ArrayBuffer): void {
    const context = this.context;
    const format = this.format;
    if (!context || !format || pcm.byteLength < 2) return;

    const samples = Math.floor(pcm.byteLength / 2 / format.channels);
    if (samples === 0) return;
    const buffer = context.createBuffer(format.channels, samples, format.sampleRateHz);
    const view = new DataView(pcm);
    for (let channel = 0; channel < format.channels; channel += 1) {
      const target = buffer.getChannelData(channel);
      for (let frame = 0; frame < samples; frame += 1) {
        const offset = (frame * format.channels + channel) * 2;
        target[frame] = view.getInt16(offset, true) / 0x8000;
      }
    }

    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(context.destination);
    source.onended = () => {
      this.sources.delete(source);
    };
    const startAt = Math.max(this.nextStartTime, context.currentTime);
    source.start(startAt);
    this.nextStartTime = startAt + buffer.duration;
    this.sources.add(source);
  }

  /** Stop immediately and release the device (barge-in, cancel, or failure). */
  stop(): void {
    for (const source of this.sources) {
      try {
        source.stop();
      } catch {
        // Already ended; nothing to stop.
      }
    }
    this.sources.clear();
    const context = this.context;
    this.context = null;
    this.format = null;
    this.nextStartTime = 0;
    if (context) void context.close();
  }
}

/**
 * User-facing text for a `voice.error` code. The wire carries only the stable
 * code (no adapter or transport text ever reaches the browser, docs/06 §5), so
 * the wording lives here — and every code has wording, because "the service
 * died" must never render as silence.
 */
export function voiceErrorMessage(code: VoiceErrorCodeDto): string {
  switch (code) {
    case 'voice.stt_unavailable':
      return 'Speech recognition is unavailable.';
    case 'voice.stt_failed':
      return 'Speech recognition failed — nothing was heard.';
    case 'voice.tts_unavailable':
      return 'The voice could not be reached; the answer is text only.';
    case 'voice.tts_failed':
      return 'The spoken answer was cut short.';
  }
}
