import {
  ChangeDetectionStrategy,
  Component,
  computed,
  input,
  output,
  signal,
} from '@angular/core';
import { CommonModule } from '@angular/common';
import type { MediaPlayerDto, MediaStateDto } from '../generated/api-types';

/**
 * MediaBar — the minimal instrument panel for whatever is playing (FR-22,
 * docs/02 §11a, ADR-012). Exit evidence #4 is this surface: pause whatever is
 * playing, from here.
 *
 * Rules it follows, all of them spec rather than styling:
 *
 * - **It is an instrument panel, not a jukebox** (media-integration skill §6):
 *   compact, no album-art dominance. Art is a small thumbnail when the player
 *   published an `https` one, absent otherwise — never a placeholder image
 *   (no fabricated content).
 * - **Absent when there is nothing to control.** No players ⇒ the bar renders
 *   nothing at all rather than a dead shell.
 * - **Ambiguity is asked, not guessed** (ADR-016). With two players active the
 *   server sends no `activePlayer`; the bar then shows both rows and requires
 *   the human to pick one. It never defaults to `players[0]`.
 * - **The cap is the server's** (docs/02 §11a). The slider is clamped to
 *   `maxVolumePct` for a sane UI, but the server enforces it regardless —
 *   raising volume beyond it is an approved R2 action, deliberately not a
 *   button here.
 * - Track/artist/player text is **player-published, untrusted** (Z4): it is
 *   interpolated as text only, never as markup.
 *
 * The component is pure: it renders the state its host passes and emits
 * commands. The host owns the POST and the WS subscription.
 */
@Component({
  selector: 'app-media-bar',
  standalone: true,
  imports: [CommonModule],
  templateUrl: './media-bar.html',
  styleUrl: './media-bar.scss',
  changeDetection: ChangeDetectionStrategy.OnPush,
})
export class MediaBar {
  /** Current playback state, from `GET /media/state` then `media.state` events. */
  readonly state = input.required<MediaStateDto>();
  /** A command is in flight — controls block until it resolves. */
  readonly pending = input(false);
  /** Last error, shown inline (e.g. "more than one player is active"). */
  readonly error = input<string | null>(null);

  /** A transport command for a specific player. */
  readonly transport = output<{ command: string; player: string }>();
  /** A volume change for a specific player, already clamped to the cap. */
  readonly volume = output<{ player: string; volumePct: number }>();

  /** Which player the human selected when the choice was ambiguous. */
  private readonly chosen = signal<string | null>(null);

  protected readonly players = computed(() => this.state().players);

  /** True when there is nothing at all to control — the bar hides itself. */
  protected readonly hidden = computed(() => this.players().length === 0);

  /**
   * The player controls act on: the server's unambiguous `activePlayer`, or the
   * one the human explicitly picked. `null` means "ambiguous, and nobody has
   * chosen" — controls stay disabled rather than guessing.
   */
  protected readonly target = computed<MediaPlayerDto | null>(() => {
    const players = this.players();
    const chosen = this.chosen();
    if (chosen !== null) {
      return players.find((p) => p.player === chosen) ?? null;
    }
    const active = this.state().activePlayer;
    if (active === undefined || active === null) {
      return null;
    }
    return players.find((p) => p.player === active) ?? null;
  });

  /** Two or more candidates and no resolution yet — the human must pick. */
  protected readonly ambiguous = computed(
    () => !this.hidden() && this.target() === null && this.players().length > 1,
  );

  protected readonly nowPlaying = computed(() => {
    const target = this.target();
    if (target === null) return null;
    const { title, artist, artUrl } = target.metadata;
    return {
      title: title ?? 'Unknown track',
      artist: artist ?? target.identity,
      // Only ever an https URL — the server drops every other scheme, so the
      // shell can never be induced to fetch a local file or internal address.
      artUrl: artUrl ?? null,
    };
  });

  protected choose(player: string): void {
    this.chosen.set(player);
  }

  protected send(verb: string): void {
    const target = this.target();
    if (target === null || this.pending()) return;
    this.transport.emit({ command: verb, player: target.player });
  }

  protected onVolume(event: Event): void {
    const target = this.target();
    if (target === null || this.pending()) return;
    const raw = Number((event.target as HTMLInputElement).value);
    // Clamp locally for a sane control; the server enforces the cap regardless.
    const clamped = Math.max(0, Math.min(this.state().maxVolumePct, Math.round(raw)));
    this.volume.emit({ player: target.player, volumePct: clamped });
  }
}
