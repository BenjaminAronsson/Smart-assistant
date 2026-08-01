import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { ListCard, type ListCardData, type ListItemCheckIntent } from './list-card';

describe('ListCard', () => {
  let fixture: ComponentFixture<ListCard>;
  let el: HTMLElement;

  function render(card: ListCardData, pending = false): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(ListCard);
    fixture.componentRef.setInput('card', card);
    fixture.componentRef.setInput('pending', pending);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  const shopping: ListCardData = {
    type: 'card.list',
    id: 'card-11',
    listId: 'list-1',
    list: {
      id: 'list-1',
      name: 'Shopping',
      openCount: 1,
      promotionOffered: false,
      items: [
        { id: 'item-1', text: 'Milk', checked: false },
        { id: 'item-2', text: 'Eggs', checked: true },
      ],
    },
  };

  it('renders the list name and every item as text with its check state', () => {
    render(shopping);
    expect(el.textContent).toContain('Shopping');
    expect(el.textContent).toContain('Milk');
    expect(el.textContent).toContain('Eggs');

    const toggles = el.querySelectorAll('.list-item-toggle');
    expect(toggles.length).toBe(2);
    expect(toggles[0].getAttribute('aria-checked')).toBe('false');
    expect(toggles[1].getAttribute('aria-checked')).toBe('true');
  });

  it('shows the server-computed open count, not a client-derived one', () => {
    render(shopping);
    expect(el.querySelector('.list-open-count')?.textContent).toContain('1 left');
  });

  it('says "All done" once nothing is open', () => {
    render({
      ...shopping,
      list: { ...shopping.list, openCount: 0, items: [{ id: 'item-1', text: 'Milk', checked: true }] },
    });
    expect(el.querySelector('.list-open-count')?.textContent).toContain('All done');
  });

  it('shows an empty-list message and no open-count badge when there are no items', () => {
    render({ ...shopping, list: { ...shopping.list, openCount: 0, items: [] } });
    expect(el.textContent).toContain('Nothing on this list yet.');
    expect(el.querySelector('.list-open-count')).toBeNull();
  });

  it('emits a check-off intent with the toggled state when an open item is tapped', () => {
    render(shopping);
    const emitted: ListItemCheckIntent[] = [];
    fixture.componentInstance.checkItem.subscribe((intent) => emitted.push(intent));

    const toggles = el.querySelectorAll<HTMLButtonElement>('.list-item-toggle');
    toggles[0].click();

    expect(emitted).toEqual([{ listId: 'list-1', itemId: 'item-1', checked: true }]);
  });

  it('emits unchecked when tapping an already-checked item', () => {
    render(shopping);
    const emitted: ListItemCheckIntent[] = [];
    fixture.componentInstance.checkItem.subscribe((intent) => emitted.push(intent));

    const toggles = el.querySelectorAll<HTMLButtonElement>('.list-item-toggle');
    toggles[1].click();

    expect(emitted).toEqual([{ listId: 'list-1', itemId: 'item-2', checked: false }]);
  });

  it('disables every toggle and emits nothing while a check-off is pending', () => {
    render(shopping, true);
    const emitted: ListItemCheckIntent[] = [];
    fixture.componentInstance.checkItem.subscribe((intent) => emitted.push(intent));

    const toggles = el.querySelectorAll<HTMLButtonElement>('.list-item-toggle');
    expect(toggles[0].disabled).toBe(true);
    toggles[0].click();

    expect(emitted).toEqual([]);
  });

  // Invariant #1 / docs/12 §9: no model-authored HTML on the HUD face. Item
  // text and the list name are sanitized human text, but the card must still
  // render markup-shaped text as inert text, never as a live element.
  it('renders markup-shaped item text and list name as inert text', () => {
    render({
      type: 'card.list',
      id: 'card-12',
      listId: 'list-2',
      list: {
        id: 'list-2',
        name: '<img src=x onerror=alert(1)>',
        openCount: 1,
        promotionOffered: false,
        items: [{ id: 'item-1', text: '<b>bold</b> milk', checked: false }],
      },
    });
    expect(el.textContent).toContain('<img src=x onerror=alert(1)>');
    expect(el.textContent).toContain('<b>bold</b> milk');
    expect(el.querySelector('img')).toBeNull();
    expect(el.querySelector('b')).toBeNull();
  });
});
