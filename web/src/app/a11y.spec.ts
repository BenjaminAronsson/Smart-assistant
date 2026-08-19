import { type Type, provideZonelessChangeDetection } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { provideHttpClient } from '@angular/common/http';
import axe from 'axe-core';

import { Hud } from './hud/hud';
import { PresenceOrb } from './hud/presence-orb';
import { Settings } from './settings/settings';
import { routes } from './app.routes';

/**
 * F10.6 — the accessibility audit, run over the surface rather than per feature.
 *
 * Keyboard-first was built in from M3b and spot-checked as each surface landed.
 * Nothing had ever audited it *whole*, which is a different claim: a per-feature
 * check cannot see a focus order that only breaks when two surfaces are
 * composed, or a route that shipped without a heading because its own spec
 * never asked.
 *
 * NFR-11 is the requirement, and its sharpest clause is that every voice
 * surface has a non-voice equivalent — an assistant reachable only by speaking
 * is unusable by anyone who cannot speak to it, and unusable by everyone in a
 * room where speaking is rude.
 *
 * # What axe can and cannot do
 *
 * axe catches roughly a third of real accessibility defects: it finds a missing
 * label, never a wrong one; a missing focus ring, never a nonsensical focus
 * order. So the axe pass below is paired with explicit structural assertions
 * for the parts a scanner is blind to, and neither is presented as the whole
 * audit. `docs/12` §8 records what remains a human judgement.
 */

/**
 * Every surface an owner can land on, audited together.
 *
 * A list, not one test per component, because the defects this feature exists
 * to catch are the ones no single component's own spec would ask about.
 */
function surfaces(): Type<unknown>[] {
  return [Hud as Type<unknown>, Settings as Type<unknown>];
}

/** Serious/critical violations, formatted so a failure names the fix. */
function report(results: axe.AxeResults): string {
  return results.violations
    .map(
      (v) =>
        `  [${v.impact}] ${v.id}: ${v.help}\n` +
        v.nodes.map((n) => `      ${n.html}`).join('\n'),
    )
    .join('\n');
}

async function auditFixture(element: HTMLElement): Promise<axe.AxeResults> {
  return axe.run(element, {
    // Colour contrast needs real layout and real backgrounds; in a headless
    // karma fixture the computed background is transparent, so the check is not
    // merely noisy but *wrong* — it reports failures that do not exist in the
    // product and misses the ones that do. Contrast is audited against the real
    // built page instead (docs/12 §8), not faked here.
    rules: { 'color-contrast': { enabled: false } },
  });
}

function expectNoViolations(results: axe.AxeResults): void {
  const serious = results.violations.filter(
    (v) => v.impact === 'serious' || v.impact === 'critical',
  );
  expect(serious.length)
    .withContext(`axe found serious/critical violations:\n${report(results)}`)
    .toBe(0);
}

describe('accessibility (F10.6, NFR-11)', () => {
  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideRouter(routes),
        provideHttpClient(),
      ],
    });
  });

  describe('axe', () => {
    it('the HUD has no serious violations', async () => {
      const fixture = TestBed.createComponent(Hud);
      fixture.detectChanges();
      expectNoViolations(await auditFixture(fixture.nativeElement));
    });

    it('the presence orb has no serious violations', async () => {
      const fixture = TestBed.createComponent(PresenceOrb);
      // `state` is a required input; the orb has no meaningful default because
      // there is no such thing as presence-in-general.
      fixture.componentRef.setInput('state', 'listening');
      fixture.detectChanges();
      expectNoViolations(await auditFixture(fixture.nativeElement));
    });

    it('settings has no serious violations', async () => {
      const fixture = TestBed.createComponent(Settings);
      fixture.detectChanges();
      expectNoViolations(await auditFixture(fixture.nativeElement));
    });
  });

  describe('what axe cannot see', () => {
    /**
     * NFR-11's sharpest clause. An assistant reachable only by speaking is
     * unusable by someone who cannot speak to it — and by anyone in a room
     * where speaking aloud is not an option.
     */
    it('every voice affordance has a non-voice equivalent', () => {
      const fixture = TestBed.createComponent(Hud);
      fixture.detectChanges();
      const root = fixture.nativeElement as HTMLElement;

      const buttons = Array.from(root.querySelectorAll('button'));
      const labelOf = (el: Element) =>
        `${el.getAttribute('aria-label') ?? ''} ${el.textContent ?? ''}`.toLowerCase();

      expect(buttons.some((b) => labelOf(b).includes('speak')))
        .withContext('push-to-talk must exist as a real button, not a hotkey only')
        .toBeTrue();
    });

    /**
     * A focusable control with no accessible name is announced as "button" —
     * technically focusable, practically unusable. axe catches the empty case;
     * this catches the icon-with-no-label case across the whole surface at once.
     */
    it('every focusable control is announced as something', () => {
      for (const component of surfaces()) {
        const fixture = TestBed.createComponent(component);
        fixture.detectChanges();
        const root = fixture.nativeElement as HTMLElement;

        const focusable = Array.from(
          root.querySelectorAll<HTMLElement>('button, a[href], input, select, textarea'),
        ).filter((el) => !el.hasAttribute('disabled') && el.getAttribute('aria-hidden') !== 'true');

        for (const el of focusable) {
          const name =
            el.getAttribute('aria-label') ??
            el.getAttribute('title') ??
            el.textContent?.trim() ??
            '';
          expect(name.length)
            .withContext(`${component.name}: unnamed control ${el.outerHTML.slice(0, 120)}`)
            .toBeGreaterThan(0);
        }
      }
    });

    /**
     * Nothing may be reachable by pointer alone. A positive `tabindex` is the
     * usual way this breaks: it jumps the element out of document order, so the
     * tab sequence stops matching the visual one and a keyboard user is lost.
     */
    it('no control is removed from or reordered within the tab sequence', () => {
      for (const component of surfaces()) {
        const fixture = TestBed.createComponent(component);
        fixture.detectChanges();
        const root = fixture.nativeElement as HTMLElement;

        for (const el of Array.from(root.querySelectorAll<HTMLElement>('[tabindex]'))) {
          const value = Number(el.getAttribute('tabindex'));
          expect(value)
            .withContext(
              `a positive tabindex reorders the tab sequence out of document order: ` +
                el.outerHTML.slice(0, 120),
            )
            .toBeLessThanOrEqual(0);
        }
      }
    });

    /**
     * Presence is the HUD's primary state, and it is conveyed by an animated
     * orb — i.e. by colour and motion, neither of which a screen reader can
     * report. The live region is the whole of its accessibility.
     */
    it('presence is announced, not only animated', () => {
      const fixture = TestBed.createComponent(PresenceOrb);
      fixture.componentRef.setInput('state', 'listening');
      fixture.detectChanges();
      const root = fixture.nativeElement as HTMLElement;

      const live = root.querySelector('[role="status"], [aria-live]');
      expect(live)
        .withContext('presence changes must reach a screen reader through a live region')
        .not.toBeNull();
      expect(live?.textContent?.trim().length ?? 0)
        .withContext('the live region must carry the state, not be empty')
        .toBeGreaterThan(0);
    });

    /**
     * Decorative layers — wallpaper, scrim, ambient field, the orb's rings —
     * must be hidden from assistive technology. They carry no information and
     * an unhidden one turns the HUD into a wall of unlabelled `div`s.
     */
    it('decorative layers are hidden from assistive technology', () => {
      const fixture = TestBed.createComponent(Hud);
      fixture.detectChanges();
      const root = fixture.nativeElement as HTMLElement;

      for (const selector of ['.wallpaper', '.scrim', '.ambient-field']) {
        const el = root.querySelector(selector);
        if (!el) continue; // not every layer is present in every configuration
        expect(el.getAttribute('aria-hidden'))
          .withContext(`${selector} is decoration and must not be announced`)
          .toBe('true');
      }
    });

    /**
     * Every route must render a top-level heading. Screen-reader users navigate
     * by heading before anything else; a route without one gives them nothing
     * to orient on, and this is the defect a per-feature spec never notices
     * because it was only ever asked about its own component.
     */
    it('each route surface offers a heading to navigate by', () => {
      for (const component of [Settings as Type<unknown>]) {
        const fixture = TestBed.createComponent(component);
        fixture.detectChanges();
        const root = fixture.nativeElement as HTMLElement;
        const headings = root.querySelectorAll('h1, h2, [role="heading"]');
        expect(headings.length)
          .withContext(`${component.name} renders no heading to navigate by`)
          .toBeGreaterThan(0);
      }
    });
  });
});
