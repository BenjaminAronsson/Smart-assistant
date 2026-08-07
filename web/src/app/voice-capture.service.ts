import { Injectable, computed, inject, signal } from '@angular/core';
import type { EventEnvelope, TransientEvent, VoiceControlDto } from '../generated/api-types';
import { ApiService } from './api.service';
import { HudStateService, type PresenceState } from './hud/hud-state.service';
import { VoicePlayback, voiceErrorMessage } from './voice-playback';

/** The wire format required by docs/05 §1. */
const SAMPLE_RATE_HZ = 16_000;
const SAMPLE_WIDTH_BYTES = 2;
const CHANNELS = 1;
/** Drop a frame rather than allowing a slow socket to grow memory without bound. */
const MAX_BUFFERED_BYTES = 256 * 1024;
/**
 * The conversation voice turns land in. Remembered so a reload continues the
 * same thread instead of littering the session list; re-created if the daemon
 * no longer knows it.
 */
const VOICE_SESSION_KEY = 'jarvis.voiceSession';
/**
 * How long the voice socket stays open after the button is released. The
 * answer — and its spoken audio — arrives *after* capture ends, so closing the
 * socket on release (as the capture-only version did) would cut off the reply.
 * Bounded so an idle HUD holds no open socket.
 */
const IDLE_SOCKET_TIMEOUT_MS = 60_000;

export type VoiceCaptureState = 'unavailable' | 'idle' | 'requesting' | 'listening' | 'error';

/**
 * Browser push-to-talk capture for the HUD.
 *
 * This service owns only capture and framing. The daemon remains the authority
 * for VAD/STT/run creation; no transcript or side effect is invented in the
 * browser. Releasing the button always sends a stop frame and closes the local
 * media graph, including when the page loses focus.
 */
@Injectable({ providedIn: 'root' })
export class VoiceCaptureService {
  private readonly api = inject(ApiService);
  private readonly hud = inject(HudStateService);
  private readonly stateSignal = signal<VoiceCaptureState>(this.isSupported() ? 'idle' : 'unavailable');
  private readonly errorSignal = signal<string | null>(null);
  private readonly droppedFramesSignal = signal(0);
  private readonly transcriptSignal = signal('');
  private readonly transcriptFinalSignal = signal(false);

  readonly state = this.stateSignal.asReadonly();
  readonly error = this.errorSignal.asReadonly();
  readonly droppedFrames = this.droppedFramesSignal.asReadonly();
  readonly transcript = this.transcriptSignal.asReadonly();
  readonly transcriptFinal = this.transcriptFinalSignal.asReadonly();
  readonly active = computed(() => this.stateSignal() === 'listening');

  private socket: WebSocket | null = null;
  private mediaStream: MediaStream | null = null;
  private audioContext: AudioContext | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private processor: ScriptProcessorNode | null = null;
  private mutedSink: GainNode | null = null;
  private streamId: string | null = null;
  private previousPresence: PresenceState | null = null;
  private stopping = false;
  private sessionId: string | null = null;
  private readonly playback = new VoicePlayback();
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private speakingUtterance: string | null = null;

  /** Start one microphone capture session. Repeated pointer/key events are harmless. */
  async begin(): Promise<void> {
    if (this.stateSignal() === 'listening' || this.stateSignal() === 'requesting') return;
    if (!this.isSupported()) {
      this.fail('Microphone capture is unavailable in this browser.');
      return;
    }
    if (!this.api.hasToken()) {
      this.fail('Pair this device before using voice capture.');
      return;
    }

    this.stopping = false;
    this.errorSignal.set(null);
    this.droppedFramesSignal.set(0);
    this.transcriptSignal.set('');
    this.transcriptFinalSignal.set(false);
    this.stateSignal.set('requesting');
    // Barge-in, browser half: whatever is being spoken stops the moment the
    // user starts talking. The daemon cancels synthesis when it sees the
    // `voice.stream.start` frame below; this stops the audio already buffered
    // here, which no server-side cancellation can reach.
    this.playback.stop();
    this.speakingUtterance = null;

    try {
      // Resolved in parallel with the microphone permission so it costs no
      // added latency on the NFR-04 path.
      const sessionPromise = this.ensureSession();
      this.mediaStream = await navigator.mediaDevices.getUserMedia({
        audio: {
          channelCount: CHANNELS,
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
        },
        video: false,
      });
      if (this.stopping) {
        this.releaseMedia();
        this.stateSignal.set('idle');
        return;
      }

      const socket = await this.ensureSocket();
      if (this.stopping) {
        this.releaseMedia();
        this.stateSignal.set('idle');
        return;
      }

      const streamId = crypto.randomUUID();
      this.streamId = streamId;
      const start: VoiceControlDto = {
        type: 'voice.stream.start',
        streamId,
        // Without a session the daemon transcribes and displays, but starts no
        // run — a run needs a conversation to belong to.
        sessionId: await sessionPromise,
        sampleRateHz: SAMPLE_RATE_HZ,
        sampleWidthBytes: SAMPLE_WIDTH_BYTES,
        channels: CHANNELS,
      };
      socket.send(JSON.stringify(start));
      await this.installAudioGraph();
      this.previousPresence = this.hud.presence();
      this.hud.setPresence('listening');
      this.stateSignal.set('listening');
    } catch (error) {
      this.releaseMedia();
      this.closeSocket();
      this.fail(this.captureError(error));
    }
  }

  /** End capture, including a deterministic stop frame. */
  end(): void {
    if (this.stateSignal() !== 'listening' && this.stateSignal() !== 'requesting') return;
    this.stopping = true;
    const socket = this.socket;
    const streamId = this.streamId;
    if (socket?.readyState === WebSocket.OPEN && streamId !== null) {
      const stop: VoiceControlDto = { type: 'voice.stream.stop', streamId };
      socket.send(JSON.stringify(stop));
    }
    this.releaseMedia();
    // The socket deliberately stays open: the run's text and its spoken audio
    // arrive after the button is released. An idle timer closes it.
    this.armIdleClose();
    this.streamId = null;
    if (this.hud.presence() === 'listening') {
      this.hud.setPresence(this.previousPresence ?? 'idle');
    }
    this.previousPresence = null;
    this.stateSignal.set('idle');
  }

  /** Called by the shell when the tab/window loses focus. */
  cancel(): void {
    this.end();
  }

  private async installAudioGraph(): Promise<void> {
    if (!this.mediaStream) throw new Error('microphone stream missing');
    const context = new AudioContext();
    if (context.state === 'suspended') await context.resume();
    const source = context.createMediaStreamSource(this.mediaStream);
    const processor = context.createScriptProcessor(2048, CHANNELS, CHANNELS);
    const mutedSink = context.createGain();
    mutedSink.gain.value = 0;
    processor.onaudioprocess = (event) => {
      const socket = this.socket;
      if (this.stopping || socket?.readyState !== WebSocket.OPEN) return;
      if (socket.bufferedAmount > MAX_BUFFERED_BYTES) {
        this.droppedFramesSignal.update((count) => count + 1);
        return;
      }
      const pcm = resampleToPcm16(event.inputBuffer.getChannelData(0), context.sampleRate);
      socket.send(pcm);
    };
    source.connect(processor);
    processor.connect(mutedSink);
    mutedSink.connect(context.destination);
    this.audioContext = context;
    this.source = source;
    this.processor = processor;
    this.mutedSink = mutedSink;
  }

  /**
   * The conversation voice turns belong to. Reuses the remembered one when the
   * daemon still knows it, so a reload continues the thread rather than opening
   * a new one per page load. A failure here is not fatal: the turn is still
   * transcribed and displayed, it just does not start a run.
   */
  private async ensureSession(): Promise<string | null> {
    if (this.sessionId !== null) return this.sessionId;
    const remembered = localStorage.getItem(VOICE_SESSION_KEY);
    if (remembered !== null) {
      try {
        const session = await this.api.getSession(remembered);
        this.sessionId = session.id;
        return session.id;
      } catch {
        localStorage.removeItem(VOICE_SESSION_KEY);
      }
    }
    try {
      const session = await this.api.createSession('Voice');
      this.sessionId = session.id;
      localStorage.setItem(VOICE_SESSION_KEY, session.id);
      return session.id;
    } catch {
      return null;
    }
  }

  /** Reuse the open voice socket if there is one; otherwise connect. */
  private async ensureSocket(): Promise<WebSocket> {
    this.clearIdleClose();
    const existing = this.socket;
    if (existing?.readyState === WebSocket.OPEN) return existing;
    if (existing !== null) this.closeSocket();

    const socket = this.api.openSocket('/ws/v1');
    // Synthesized PCM arrives as binary frames; without this they surface as
    // Blobs and would have to be read asynchronously, reordering playback.
    socket.binaryType = 'arraybuffer';
    this.socket = socket;
    socket.onmessage = (message) => this.handleSocketMessage(message.data);
    await this.waitForSocket(socket);
    return socket;
  }

  private closeSocket(): void {
    this.clearIdleClose();
    const socket = this.socket;
    this.socket = null;
    socket?.close();
    this.playback.stop();
    this.speakingUtterance = null;
  }

  private armIdleClose(): void {
    this.clearIdleClose();
    this.idleTimer = globalThis.setTimeout(() => {
      this.idleTimer = null;
      // Never cut off audio that is still playing.
      if (this.speakingUtterance !== null) {
        this.armIdleClose();
        return;
      }
      this.closeSocket();
    }, IDLE_SOCKET_TIMEOUT_MS);
  }

  private clearIdleClose(): void {
    if (this.idleTimer !== null) {
      globalThis.clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
  }

  private waitForSocket(socket: WebSocket): Promise<void> {
    return new Promise((resolve, reject) => {
      const timeout = globalThis.setTimeout(() => {
        socket.close();
        reject(new Error('voice channel did not connect'));
      }, 5000);
      socket.onopen = () => {
        globalThis.clearTimeout(timeout);
        resolve();
      };
      socket.onerror = () => {
        globalThis.clearTimeout(timeout);
        reject(new Error('voice channel is unavailable'));
      };
      socket.onclose = () => {
        globalThis.clearTimeout(timeout);
        this.socket = null;
        this.playback.stop();
        this.speakingUtterance = null;
        if (!this.stopping) this.fail('Voice channel disconnected.');
      };
    });
  }

  private handleSocketMessage(data: unknown): void {
    // Binary frames are synthesized PCM for the utterance the daemon announced.
    if (data instanceof ArrayBuffer) {
      if (this.speakingUtterance !== null) this.playback.push(data);
      return;
    }
    if (typeof data !== 'string') return;
    try {
      const envelope = JSON.parse(data) as EventEnvelope;
      if (envelope.channel !== 'voice') return;
      switch (envelope.type) {
        case 'voice.transcript': {
          const event = envelope.payload as TransientEvent;
          if (event.type !== 'voice.transcript') return;
          this.transcriptSignal.set(event.text);
          if (event.final) this.transcriptFinalSignal.set(true);
          return;
        }
        case 'voice.error': {
          const event = envelope.payload as TransientEvent;
          if (event.type !== 'voice.error') return;
          // A failed leg is shown, never rendered as silence — the whole point
          // of the `voice.error` event (docs/02 §9, F5.2).
          this.errorSignal.set(voiceErrorMessage(event.code));
          this.playback.stop();
          this.speakingUtterance = null;
          return;
        }
        case 'voice.speak.start': {
          const control = envelope.payload as VoiceControlDto;
          if (control.type !== 'voice.speak.start') return;
          this.speakingUtterance = control.utteranceId;
          this.playback.begin(control.sampleRateHz, control.channels);
          return;
        }
        case 'voice.speak.stop': {
          const control = envelope.payload as VoiceControlDto;
          if (control.type !== 'voice.speak.stop') return;
          if (control.utteranceId !== this.speakingUtterance) return;
          this.speakingUtterance = null;
          // `cancelled` is barge-in: drop what is buffered rather than letting
          // the superseded answer keep talking over the user. `completed` lets
          // the already-scheduled tail play out.
          if (control.reason !== 'completed') this.playback.stop();
          return;
        }
        default:
          return;
      }
    } catch {
      // Voice is fail-closed: malformed recognition output never becomes UI text.
    }
  }

  private releaseMedia(): void {
    this.processor?.disconnect();
    this.source?.disconnect();
    this.mutedSink?.disconnect();
    this.processor = null;
    this.source = null;
    this.mutedSink = null;
    this.mediaStream?.getTracks().forEach((track) => track.stop());
    this.mediaStream = null;
    const context = this.audioContext;
    this.audioContext = null;
    if (context) void context.close();
  }

  private isSupported(): boolean {
    return (
      typeof navigator !== 'undefined' &&
      !!navigator.mediaDevices?.getUserMedia &&
      typeof AudioContext !== 'undefined'
    );
  }

  private fail(message: string): void {
    this.errorSignal.set(message);
    if (this.stateSignal() === 'requesting' || this.stateSignal() === 'listening') {
      this.hud.setPresence('error');
    }
    this.stateSignal.set('error');
  }

  private captureError(error: unknown): string {
    if (error instanceof DOMException && error.name === 'NotAllowedError') {
      return 'Microphone permission was not granted.';
    }
    if (error instanceof DOMException && error.name === 'NotFoundError') {
      return 'No microphone was found.';
    }
    return error instanceof Error ? error.message : 'Microphone capture failed.';
  }
}

/** Downsample one browser float buffer and encode the documented PCM format. */
function resampleToPcm16(input: Float32Array, inputRate: number): ArrayBuffer {
  const ratio = inputRate / SAMPLE_RATE_HZ;
  const outputLength = Math.max(1, Math.floor(input.length / ratio));
  const output = new ArrayBuffer(outputLength * SAMPLE_WIDTH_BYTES);
  const view = new DataView(output);
  for (let index = 0; index < outputLength; index += 1) {
    const sourceIndex = Math.min(input.length - 1, Math.floor(index * ratio));
    const sample = Math.max(-1, Math.min(1, input[sourceIndex]));
    const pcm = sample < 0 ? sample * 0x8000 : sample * 0x7fff;
    view.setInt16(index * SAMPLE_WIDTH_BYTES, pcm, true);
  }
  return output;
}
