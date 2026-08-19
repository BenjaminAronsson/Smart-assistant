import type { VoiceErrorCodeDto } from '../generated/api-types';
import { VoicePlayback, voiceErrorMessage } from './voice-playback';

describe('voiceErrorMessage', () => {
  /**
   * Every wire code must render as something a human can read. The wire
   * deliberately carries only the stable code (no adapter text ever reaches the
   * browser), so a code with no mapping here would surface as blank — exactly
   * the "a dead service looks like silence" failure `voice.error` exists to
   * prevent (F5.2).
   */
  const codes: VoiceErrorCodeDto[] = [
    'voice.stt_unavailable',
    'voice.stt_failed',
    'voice.tts_unavailable',
    'voice.tts_failed',
  ];

  it('gives every voice error code a non-empty message', () => {
    for (const code of codes) {
      expect(voiceErrorMessage(code).trim().length).toBeGreaterThan(0);
    }
  });

  it('distinguishes a recognition failure from a synthesis failure', () => {
    expect(voiceErrorMessage('voice.stt_failed')).not.toBe(voiceErrorMessage('voice.tts_failed'));
  });
});

describe('VoicePlayback', () => {
  /**
   * The bug this exists for: a suspended `AudioContext` accepts `start()` and
   * plays nothing — no error, no warning — so a daemon that was provably
   * synthesizing audio produced a silent browser and looked like working code.
   *
   * This context is always created at that disadvantage: `begin()` runs when
   * `voice.speak.start` arrives, which is after the transcript, the model and
   * the first synthesized clause — long after the push-to-talk gesture ended.
   */
  it('resumes a context the browser handed back suspended', () => {
    const resume = jasmine.createSpy('resume').and.returnValue(Promise.resolve());
    const fake = { state: 'suspended', currentTime: 0, resume, close: () => Promise.resolve() };
    const original = window.AudioContext;
    (window as unknown as { AudioContext: unknown }).AudioContext = function () {
      return fake;
    };
    try {
      new VoicePlayback().begin(22_050, 1);
      expect(resume).toHaveBeenCalled();
    } finally {
      (window as unknown as { AudioContext: unknown }).AudioContext = original;
    }
  });

  it('does not throw when resuming is refused outright', () => {
    const fake = {
      state: 'suspended',
      currentTime: 0,
      resume: () => Promise.reject(new Error('no user activation')),
      close: () => Promise.resolve(),
    };
    const original = window.AudioContext;
    (window as unknown as { AudioContext: unknown }).AudioContext = function () {
      return fake;
    };
    try {
      // `begin()` is called from a synchronous socket handler; a rejected
      // resume must not throw into it and tear down the voice session.
      expect(() => new VoicePlayback().begin(22_050, 1)).not.toThrow();
    } finally {
      (window as unknown as { AudioContext: unknown }).AudioContext = original;
    }
  });

  it('ignores audio that was never announced', () => {
    const playback = new VoicePlayback();
    // No `voice.speak.start` has been seen, so there is no utterance these
    // bytes could belong to. Playing them anyway would let a stray frame — for
    // instance one racing a barge-in — speak over the user.
    expect(() => playback.push(new ArrayBuffer(64))).not.toThrow();
    expect(playback.active).toBeFalse();
  });

  it('stops cleanly when nothing is playing', () => {
    const playback = new VoicePlayback();
    expect(() => playback.stop()).not.toThrow();
    expect(playback.active).toBeFalse();
  });
});
