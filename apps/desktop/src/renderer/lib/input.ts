// Captures local pointer/keyboard input over the remote view and maps it to
// the agent's InputEvent shape. Coordinates are normalized to the video
// content box (letterboxing excluded) so the host never sees local DPI.

import { type InputEvent, KEY_CODES, type MouseButton } from '../../../shared/protocol';

const BUTTONS: Record<number, MouseButton> = { 0: 'left', 1: 'middle', 2: 'right', 3: 'back', 4: 'forward' };
const LINE_PX = 16;

export function contentBox(el: HTMLCanvasElement): { left: number; top: number; width: number; height: number } {
  const r = el.getBoundingClientRect();
  const vw = el.width || 1;
  const vh = el.height || 1;
  const scale = Math.min(r.width / vw, r.height / vh);
  const width = vw * scale;
  const height = vh * scale;
  return { left: r.left + (r.width - width) / 2, top: r.top + (r.height - height) / 2, width, height };
}

/** Attaches listeners; returns a detach function that also releases held input. */
export function attachInput(el: HTMLCanvasElement, send: (ev: InputEvent) => void): () => void {
  let pendingMove: { x: number; y: number } | null = null;
  let raf = 0;
  const held = new Set<number>();

  const flushMove = () => {
    raf = 0;
    if (pendingMove) send({ mouseMove: pendingMove });
    pendingMove = null;
  };

  const normalize = (e: PointerEvent | MouseEvent) => {
    const b = contentBox(el);
    return {
      x: Math.min(1, Math.max(0, (e.clientX - b.left) / b.width)),
      y: Math.min(1, Math.max(0, (e.clientY - b.top) / b.height)),
    };
  };

  const onMove = (e: PointerEvent) => {
    // Coalesce to one move per animation frame.
    pendingMove = normalize(e);
    if (!raf) raf = requestAnimationFrame(flushMove);
  };

  const onDown = (e: PointerEvent) => {
    const button = BUTTONS[e.button];
    if (!button) return;
    el.focus();
    el.setPointerCapture(e.pointerId);
    e.preventDefault();
    if (raf) {
      cancelAnimationFrame(raf);
      flushMove();
    }
    send({ mouseMove: normalize(e) });
    held.add(e.button);
    send({ mouseButton: { button, down: true } });
  };

  const onUp = (e: PointerEvent) => {
    const button = BUTTONS[e.button];
    if (!button || !held.delete(e.button)) return;
    e.preventDefault();
    send({ mouseButton: { button, down: false } });
  };

  const onWheel = (e: WheelEvent) => {
    e.preventDefault();
    const k = e.deltaMode === WheelEvent.DOM_DELTA_LINE ? LINE_PX : e.deltaMode === WheelEvent.DOM_DELTA_PAGE ? el.clientHeight : 1;
    send({ wheel: { dx: e.deltaX * k, dy: e.deltaY * k } });
  };

  const onKey = (down: boolean) => (e: KeyboardEvent) => {
    if (!KEY_CODES.has(e.code)) return;
    // Keep browser shortcuts (reload, devtools, tab switching) away from the remote.
    e.preventDefault();
    e.stopPropagation();
    send({ key: { code: e.code, down } });
  };
  const onKeyDown = onKey(true);
  const onKeyUp = onKey(false);

  const releaseAll = () => {
    held.clear();
    send('releaseAll');
  };
  const onContextMenu = (e: Event) => e.preventDefault();

  el.addEventListener('pointermove', onMove);
  el.addEventListener('pointerdown', onDown);
  el.addEventListener('pointerup', onUp);
  el.addEventListener('wheel', onWheel, { passive: false });
  el.addEventListener('keydown', onKeyDown);
  el.addEventListener('keyup', onKeyUp);
  el.addEventListener('blur', releaseAll);
  el.addEventListener('contextmenu', onContextMenu);
  window.addEventListener('blur', releaseAll);

  return () => {
    if (raf) cancelAnimationFrame(raf);
    el.removeEventListener('pointermove', onMove);
    el.removeEventListener('pointerdown', onDown);
    el.removeEventListener('pointerup', onUp);
    el.removeEventListener('wheel', onWheel);
    el.removeEventListener('keydown', onKeyDown);
    el.removeEventListener('keyup', onKeyUp);
    el.removeEventListener('blur', releaseAll);
    el.removeEventListener('contextmenu', onContextMenu);
    window.removeEventListener('blur', releaseAll);
    releaseAll();
  };
}
