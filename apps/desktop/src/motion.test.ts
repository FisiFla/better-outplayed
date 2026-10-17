// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import Toasts, { ERROR_MS, NOTICE_MS } from './lib/components/Toasts.svelte';

describe('toasts auto-dismiss', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('dismisses a notice and an error after their own delays, not before', async () => {
    const onDismissError = vi.fn();
    const onDismissNotice = vi.fn();
    const app = mount(Toasts, {
      target: document.body,
      props: { error: 'boom', notice: 'done', onDismissError, onDismissNotice },
    });
    await tick();
    vi.advanceTimersByTime(NOTICE_MS - 1);
    expect(onDismissNotice).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onDismissNotice).toHaveBeenCalledTimes(1);
    expect(onDismissError).not.toHaveBeenCalled();
    vi.advanceTimersByTime(ERROR_MS - NOTICE_MS);
    expect(onDismissError).toHaveBeenCalledTimes(1);
    unmount(app);
  });

  it('sets no timer while there is nothing to show', async () => {
    const onDismissNotice = vi.fn();
    const app = mount(Toasts, {
      target: document.body,
      props: { error: null, notice: null, onDismissError: vi.fn(), onDismissNotice },
    });
    await tick();
    vi.advanceTimersByTime(ERROR_MS * 2);
    expect(onDismissNotice).not.toHaveBeenCalled();
    unmount(app);
  });
});

describe('reduced motion', () => {
  const css = readFileSync('src/app.css', 'utf8');
  const block = css.match(/@media \(prefers-reduced-motion: reduce\) \{([\s\S]*?\n\})\n/)?.[1] ?? '';

  it('zeroes every animation and transition, with no pulse left running', () => {
    expect(block).toMatch(/animation-duration: 0s !important/);
    expect(block).toMatch(/animation-iteration-count: 1 !important/);
    expect(block).toMatch(/transition-duration: 0s !important/);
  });

  it('keeps the view animations inside what that rule overrides', () => {
    // Every animation is declared with a duration, so the universal override reaches it.
    expect(css).toMatch(/animation: view-in var\(--d-base\)/);
    expect(css).toMatch(/animation: page-in var\(--d-slow\)/);
  });
});
