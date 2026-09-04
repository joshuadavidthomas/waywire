<script lang="ts">
  import { Clipboard, Keyboard, Monitor, Play, Square } from "@lucide/svelte";
  import {
    DesktopStatus,
    type DesktopStatus as DesktopStatusValue,
  } from "@sprite-desktop/shared/status";
  import { onMount } from "svelte";

  import Badge from "$lib/components/ui/badge/Badge.svelte";
  import Button from "$lib/components/ui/button/Button.svelte";
  import Card from "$lib/components/ui/card/Card.svelte";
  import { DesktopSession } from "$lib/vnc/session.svelte";

  const session = new DesktopSession();
  let viewport: HTMLDivElement;
  let status = $state<DesktopStatusValue | null>(null);
  let checkingStatus = $state(false);

  const spriteName = $derived(status?.sprite ?? "josh-desktop");

  async function refreshStatus(): Promise<void> {
    checkingStatus = true;
    try {
      const response = await fetch("/api/desktop/status", {
        headers: { Accept: "application/json" },
      });
      if (!response.ok)
        throw new Error(`Health request failed (${response.status})`);
      status = DesktopStatus.parse(await response.json());
    } catch {
      status = null;
    } finally {
      checkingStatus = false;
    }
  }

  function healthTone(health: DesktopStatusValue["health"] | undefined) {
    if (health === "healthy") return "healthy" as const;
    if (health === "hibernated" || health === "unknown")
      return "warning" as const;
    if (health === "unhealthy" || health === "error") return "danger" as const;
    return "neutral" as const;
  }

  $effect(() => {
    const shouldPoll =
      session.phase === "idle" || session.phase === "disconnected";
    if (!shouldPoll) return;

    void refreshStatus();
    const interval = window.setInterval(() => void refreshStatus(), 10_000);
    return () => window.clearInterval(interval);
  });

  onMount(() => () => session.destroy());
</script>

<svelte:head>
  <title>Sprite Desktop</title>
  <meta
    name="description"
    content="A live Linux desktop running inside a Fly Sprite and streamed through Cloudflare."
  />
</svelte:head>

<main class="isolate grid min-h-dvh grid-rows-[auto_minmax(0,1fr)] bg-canvas">
  <header
    class="flex min-w-0 items-center justify-between gap-4 border-b border-edge px-4 py-3 sm:px-6"
  >
    <a
      href="/"
      aria-label="Homepage"
      class="flex min-w-0 items-center gap-3 outline-none focus-visible:rounded-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-action"
    >
      <Monitor class="size-4 shrink-0 stroke-ink" aria-hidden="true" />
      <div class="min-w-0">
        <h1
          class="truncate text-sm/5 font-semibold tracking-[-0.01em] text-ink"
        >
          Sprite Desktop
        </h1>
        <p class="truncate font-mono text-xs/4 text-ink-tertiary">
          local spike / josh
        </p>
      </div>
    </a>

    <div class="flex min-w-0 items-center gap-2">
      <p class="hidden truncate font-mono text-xs/4 text-ink-tertiary sm:block">
        {spriteName}
      </p>
      <Badge tone={healthTone(status?.health)}>
        {#snippet children()}
          {checkingStatus && !status
            ? "checking"
            : (status?.health ?? "unknown")}
        {/snippet}
      </Badge>
    </div>
  </header>

  <section
    class="grid min-h-0 grid-rows-[minmax(18rem,1fr)_auto] gap-3 p-3 sm:gap-4 sm:p-4"
  >
    <Card class="relative min-h-0 overflow-hidden bg-well">
      <!-- svelte-ignore a11y_no_noninteractive_tabindex (noVNC needs a focusable canvas host) -->
      <div
        bind:this={viewport}
        role="application"
        tabindex="0"
        aria-label="Remote Linux desktop"
        class="h-full min-h-72 w-full overflow-hidden outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-action [&>canvas]:h-full [&>canvas]:w-full"
      ></div>

      {#if !session.connected && !session.busy}
        <div
          class="pointer-events-none absolute inset-0 grid place-items-center p-6"
          aria-hidden="true"
        >
          <div class="flex max-w-sm flex-col items-center gap-3 text-center">
            <Monitor class="size-4 shrink-0 stroke-ink-tertiary" />
            <div>
              <p class="text-base/6 font-medium text-ink sm:text-sm/5">
                Desktop tunnel is closed
              </p>
              <p class="pt-1 text-base/6 text-ink-tertiary sm:text-sm/5">
                Connect to wake the sprite and attach the VNC client.
              </p>
            </div>
          </div>
        </div>
      {/if}
    </Card>

    <nav
      aria-label="Desktop controls"
      class="flex min-w-0 flex-wrap items-center gap-2"
    >
      <Button
        variant="primary"
        disabled={session.busy || session.connected}
        onclick={() => void session.connect(viewport)}
      >
        {#snippet children()}
          <Play class="size-4 shrink-0 stroke-action-ink" aria-hidden="true" />
          {session.busy ? session.label : "Connect"}
        {/snippet}
      </Button>

      <Button
        disabled={!session.canDisconnect}
        onclick={() => session.disconnect()}
      >
        {#snippet children()}
          <Square
            class="size-4 shrink-0 stroke-ink-secondary"
            aria-hidden="true"
          />
          Disconnect
        {/snippet}
      </Button>

      <div class="hidden h-5 w-px bg-edge sm:block" aria-hidden="true"></div>

      <Button
        variant="ghost"
        disabled={!session.connected}
        onclick={() => session.sendCtrlAltDel()}
      >
        {#snippet children()}
          <Keyboard
            class="size-4 shrink-0 stroke-ink-secondary"
            aria-hidden="true"
          />
          Send Ctrl+Alt+Del
        {/snippet}
      </Button>

      <Button
        variant="ghost"
        disabled={!session.connected}
        onclick={() => void session.paste()}
      >
        {#snippet children()}
          <Clipboard
            class="size-4 shrink-0 stroke-ink-secondary"
            aria-hidden="true"
          />
          Paste
        {/snippet}
      </Button>

      {#if session.error}
        <p role="alert" class="text-sm/5 text-danger">{session.error}</p>
      {/if}
    </nav>
  </section>
</main>
