import { ChangeDetectionStrategy, Component, computed, input, output } from '@angular/core';
import { PRESENCE_HUE, PRESENCE_LABEL, type PresenceState } from './hud-state.service';

/**
 * Presence orb (docs/12 §2.1): the always-visible state indicator, and the
 * click target for the ops layer.
 *
 * State is carried by **colour and motion together** — colour alone fails
 * accessibility — plus an announced state name for screen readers (§8). The hue
 * comes from `PRESENCE_HUE`, the single place amber is bound to "waiting on
 * you"; the component never picks a colour itself.
 */
@Component({
  selector: 'app-presence-orb',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './presence-orb.html',
  styleUrl: './presence-orb.scss',
  host: {
    '[class]': '"orb-host state-" + state()',
    '[style.--hue]': 'hueVar()',
    '[class.motion-still]': '!ambient()',
  },
})
export class PresenceOrb {
  readonly state = input.required<PresenceState>();
  /** Whether ambient motion may run (docs/12 §6 — hidden/unfocused/reduced/battery). */
  readonly ambient = input(true);
  readonly activate = output<void>();

  protected readonly hueVar = computed(() => `var(${PRESENCE_HUE[this.state()]})`);
  protected readonly label = computed(() => PRESENCE_LABEL[this.state()]);

  protected onActivate(): void {
    this.activate.emit();
  }
}
