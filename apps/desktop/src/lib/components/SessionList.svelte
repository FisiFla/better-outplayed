<script lang="ts">
  /**
   * The session list: newest first, with the facts that identify a recording, a favourite
   * toggle and a confirmed delete.
   *
   * The list does not sort. `list_sessions` returns rows newest first on the store's own
   * order (`started_at DESC, id DESC`), which is the same order the retention rules use;
   * sorting again here would be a second opinion that could silently disagree with it.
   */
  import Icon from './Icon.svelte';
  import { sessionLengthLabel } from '../sessions';
  import type { SessionView } from '../sessions';
  import { formatBytes, formatDateTime } from '../time';

  interface Props {
    sessions: SessionView[];
    selectedId: number | null;
    onSelect: (id: number) => void;
    onFavourite: (id: number, favourite: boolean) => void;
    onDelete: (id: number) => void;
  }

  let { sessions, selectedId, onSelect, onFavourite, onDelete }: Props = $props();

  /** The session whose delete button is armed, if any. */
  let confirming = $state<number | null>(null);
</script>

{#if sessions.length === 0}
  <div class="empty-state">
    <Icon name="list-video" size={40} />
    <p>No sessions yet — full-session recordings appear here.</p>
  </div>
{:else}
  <ul>
    {#each sessions as session (session.id)}
      <li class:selected={session.id === selectedId}>
        <button
          class="row"
          type="button"
          aria-current={session.id === selectedId ? 'true' : undefined}
          onclick={() => onSelect(session.id)}
        >
          <span class="badge" aria-hidden="true">{session.title.charAt(0).toUpperCase()}</span>
          <span class="title">{session.title}</span>
          <span class="when muted">{formatDateTime(session.startedAtMs)}</span>
          <span class="facts mono muted">
            {sessionLengthLabel(session)}
            {#if session.running}<span class="live">· recording</span>{/if}
            · {formatBytes(session.sizeBytes)}
          </span>
        </button>

        <div class="actions">
          <button
            type="button"
            class="icon star"
            aria-pressed={session.favourite}
            aria-label={session.favourite ? 'Unfavourite session' : 'Favourite session'}
            title={session.favourite
              ? 'Favourite: protected from both session retention rules'
              : 'Protect this session from the retention rules'}
            onclick={() => onFavourite(session.id, !session.favourite)}
          >
            <Icon name="star" size={14} />
          </button>

          {#if confirming === session.id}
            <button type="button" class="danger" onclick={() => { confirming = null; onDelete(session.id); }}>
              Confirm
            </button>
            <button type="button" onclick={() => (confirming = null)}>Cancel</button>
          {:else}
            <button
              type="button"
              class="icon"
              aria-label="Delete this session"
              title="Delete this session"
              onclick={() => (confirming = session.id)}
            >
              <Icon name="trash-2" size={14} />
            </button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
{/if}

<style>
  ul {
    margin: 0;
    padding: var(--s-1) var(--s-2) var(--s-3);
    list-style: none;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  li {
    position: relative;
    border-radius: var(--r-md);
    border: 1px solid transparent;
    transition: background var(--d-fast) var(--ease);
  }

  li:hover {
    background: var(--surface-2);
  }

  li.selected {
    background: var(--accent-soft);
    border-color: var(--accent);
  }

  .row {
    display: grid;
    grid-template-columns: 36px minmax(0, 1fr);
    grid-template-areas:
      'badge title'
      'badge when'
      'badge facts';
    gap: 1px var(--s-3);
    align-items: center;
    width: 100%;
    padding: var(--s-2) var(--s-3);
    border: 0;
    background: transparent;
    text-align: left;
    cursor: pointer;
  }

  .row:hover:not(:disabled) {
    background: transparent;
  }

  .badge {
    grid-area: badge;
    display: grid;
    place-items: center;
    width: 36px;
    height: 36px;
    border-radius: var(--r-md);
    background: var(--surface-3);
    color: var(--accent);
    font: var(--t-heading);
  }

  .selected .badge {
    background: var(--accent-strong);
    color: #fff;
  }

  .title {
    grid-area: title;
    font: var(--t-heading);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    /* leave room for the overlaid actions */
    padding-right: 64px;
  }

  .when {
    grid-area: when;
    font: var(--t-small);
  }

  .facts {
    grid-area: facts;
    font: var(--t-small);
    font-feature-settings: 'tnum';
  }

  .live {
    color: var(--rec);
  }

  .actions {
    position: absolute;
    top: var(--s-2);
    right: var(--s-2);
    display: flex;
    gap: var(--s-1);
    align-items: center;
    opacity: 0;
    transition: opacity var(--d-base) var(--ease);
  }

  li:hover .actions,
  li:focus-within .actions,
  li.selected .actions {
    opacity: 1;
  }

  @media (hover: none) {
    .actions {
      opacity: 1;
    }
  }

  .actions button {
    padding: 3px 6px;
    font: var(--t-small);
    background: var(--surface-3);
  }

  .actions .icon {
    display: inline-grid;
    place-items: center;
    padding: 4px;
  }

  .star {
    color: var(--text-muted);
  }

  .star[aria-pressed='true'] {
    color: var(--star);
    border-color: var(--star);
  }

  .star[aria-pressed='true'] :global(svg) {
    fill: currentColor;
  }
</style>
