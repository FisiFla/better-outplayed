<script lang="ts">
  /**
   * The session list: newest first, with the facts that identify a recording, a favourite
   * toggle and a confirmed delete.
   *
   * The list does not sort. `list_sessions` returns rows newest first on the store's own
   * order (`started_at DESC, id DESC`), which is the same order the retention rules use;
   * sorting again here would be a second opinion that could silently disagree with it.
   */
  import { sessionLengthLabel } from '../sessions';
  import type { SessionView } from '../sessions';
  import { formatDateTime } from '../time';

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
  <p class="empty">No sessions yet — full-session recordings appear here.</p>
{:else}
  <ul>
    {#each sessions as session (session.id)}
      <li class:selected={session.id === selectedId}>
        <button class="row" type="button" onclick={() => onSelect(session.id)}>
          <span class="title">{session.title}</span>
          <span class="when muted">{formatDateTime(session.startedAtMs)}</span>
          <span class="facts mono muted">
            {sessionLengthLabel(session)}
            {#if session.running}· recording{/if}
          </span>
        </button>

        <div class="actions">
          <button
            type="button"
            class="star"
            aria-pressed={session.favourite}
            title={session.favourite
              ? 'Favourite: protected from both session retention rules'
              : 'Protect this session from the retention rules'}
            onclick={() => onFavourite(session.id, !session.favourite)}
          >
            {session.favourite ? '★' : '☆'}
          </button>

          {#if confirming === session.id}
            <button type="button" class="danger" onclick={() => { confirming = null; onDelete(session.id); }}>
              Confirm
            </button>
            <button type="button" onclick={() => (confirming = null)}>Cancel</button>
          {:else}
            <button type="button" onclick={() => (confirming = session.id)}>Delete</button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
{/if}

<style>
  ul {
    margin: 0;
    padding: 0;
    list-style: none;
    overflow-y: auto;
    /*
     * The sidebar's second scroll area. Without a cap this sizes to its content and
     * pushes the storage panel below the fold — with no way to reach it, since the
     * sidebar itself does not scroll. The clip list stays the flexible one.
     */
    max-height: 30vh;
  }

  li {
    border-bottom: 1px solid var(--line);
    padding: 6px 10px 8px;
  }

  li.selected {
    background: var(--panel-2);
  }

  .row {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 2px 8px;
    width: 100%;
    padding: 0;
    border: 0;
    background: transparent;
    text-align: left;
    cursor: pointer;
  }

  .row:hover .title {
    color: var(--accent);
  }

  .title {
    font-size: 13px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .when {
    font-size: 11px;
  }

  .facts {
    grid-column: 1 / -1;
    font-size: 11px;
  }

  .actions {
    display: flex;
    gap: 6px;
    align-items: center;
    margin-top: 5px;
    opacity: 0;
    transition: opacity 150ms ease-out;
  }

  li:hover .actions {
    opacity: 1;
  }

  li:focus-within .actions {
    opacity: 1;
  }

  li.selected .actions {
    opacity: 1;
  }

  @media (prefers-reduced-motion: reduce) {
    .actions {
      transition: none;
    }
  }

  @media (hover: none) {
    .actions {
      opacity: 1;
    }
  }

  .actions button {
    padding: 1px 7px;
    font-size: 11px;
  }

  .star {
    color: var(--muted);
  }

  .star[aria-pressed='true'] {
    color: var(--star);
    border-color: var(--star);
  }
</style>
