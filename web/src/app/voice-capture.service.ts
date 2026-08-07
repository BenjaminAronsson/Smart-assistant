import { Injectable, computed, inject, signal } from '@angular/core';
import type { EventEnvelope, TransientEvent, VoiceControlDto } from '../generated/api-types';
import { ApiService } from './api.service';
import { HudStateService, type PresenceState } from './hud/hud-state.service';

/** The wire format required by docs/05 §1. */
const SAMPLE_RATE_HZ = 16_000;
const SAMPLE_WIDTH_BYTES = 2;
const CHANNELS = 1;
/** Drop a frame rather than allowing a slow socket to grow memory without bound. */
const MAX_BUFFERED_BYTES = 256 * 1024;

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

    try {
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

      const socket = this.api.openSocket('/ws/v1');
      this.socket = socket;
      socket.onmessage = (message) => this.handleSocketMessage(message.data);
      await this.waitForSocket(socket);
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
        sessionId: null,
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
      this.socket?.close();
      this.socket = null;
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
    socket?.close();
    this.socket = null;
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
        if (!this.stopping) this.fail('Voice channel disconnected.');
      };
    });
  }

  private handleSocketMessage(data: unknown): void {
    if (typeof data !== 'string') return;
    try {
      const envelope = JSON.parse(data) as EventEnvelope;
      if (envelope.channel !== 'voice' || envelope.type !== 'voice.transcript') return;
      const event = envelope.payload as TransientEvent;
      if (event.type !== 'voice.transcript') return;
      this.transcriptSignal.set(event.text);
      if (event.final) this.transcriptFinalSignal.set(true);
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
